mod connection_editor;
mod console;
mod results_panel;
mod table_data;

use anyhow::{Context as _, Result};
use connection_editor::ConnectionEditorModal;
use console::DatabaseConsole;
use database::{
    ConnectionId, ConnectionProfile, ConnectionRegistry, ConsoleId, ConsoleRegistry,
    DatabaseDriver, MetadataDatabase, MetadataTable, QueryConsole, TableMetadataDetails,
    describe_table, list_databases, list_tables,
};
use db::kvp::KeyValueStore;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, Pixels, Render, StatefulInteractiveElement, Task,
    WeakEntity, Window, px,
};
pub use results_panel::DatabaseResultsPanel;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use table_data::TableDataView;
use ui::{
    Button, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, ListItem,
    Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::database_panel::{Toggle, ToggleFocus, ToggleResults, ToggleResultsFocus};

const DATABASE_PANEL_KEY: &str = "DatabasePanel";
const CONNECTIONS_STORAGE_KEY: &str = "database-viewer-connections-v1";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<DatabasePanel>(window, cx) {
                workspace.close_panel::<DatabasePanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ToggleResultsFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabaseResultsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleResults, window, cx| {
            if !workspace.toggle_panel_focus::<DatabaseResultsPanel>(window, cx) {
                workspace.close_panel::<DatabaseResultsPanel>(window, cx);
            }
        });
    })
    .detach();
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    registry: ConnectionRegistry,
    console_registry: ConsoleRegistry,
    console_storage_key: Option<String>,
    expanded_connections: HashSet<ConnectionId>,
    expanded_console_groups: HashSet<ConnectionId>,
    expanded_database_groups: HashSet<ConnectionId>,
    metadata_states: HashMap<ConnectionId, ConnectionMetadataState>,
    connection_states: HashMap<ConnectionId, ConnectionState>,
    pending_console_persist: Task<()>,
}

#[derive(Clone)]
pub(crate) enum ConnectionState {
    Connected,
    Error,
}

#[derive(Clone, Default)]
enum MetadataLoadState<T> {
    #[default]
    NotLoaded,
    Loading,
    Loaded(T),
    Error(String),
}

#[derive(Default)]
struct ConnectionMetadataState {
    databases: MetadataLoadState<Vec<MetadataDatabase>>,
    expanded_databases: HashSet<MetadataDatabase>,
    tables: HashMap<MetadataDatabase, MetadataLoadState<Vec<MetadataTable>>>,
    expanded_tables: HashSet<MetadataTable>,
    details: HashMap<MetadataTable, MetadataLoadState<TableMetadataDetails>>,
}

impl DatabasePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
        let serialized = cx
            .background_spawn(async move { kvp.read_kvp(CONNECTIONS_STORAGE_KEY) })
            .await?;
        let registry = serialized
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .unwrap_or_else(|error| {
                log::error!("failed to load database connections: {error:#}");
                None
            })
            .unwrap_or_default();

        let console_storage_key = workspace
            .read_with(&cx, |workspace, _| Self::console_storage_key(workspace))
            .ok()
            .flatten();
        let console_registry = if let Some(storage_key) = console_storage_key.clone() {
            let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
            cx.background_spawn(async move { kvp.read_kvp(&storage_key) })
                .await?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .unwrap_or_else(|error| {
                    log::error!("failed to load database consoles: {error:#}");
                    None
                })
                .unwrap_or_default()
        } else {
            ConsoleRegistry::default()
        };
        let expanded_connections = console_registry
            .consoles
            .iter()
            .map(|console| console.connection_id)
            .collect();
        let expanded_console_groups = console_registry
            .consoles
            .iter()
            .map(|console| console.connection_id)
            .collect();

        let workspace_handle = workspace.clone();
        workspace.update_in(&mut cx, move |_, _, cx| {
            cx.new(|cx| Self {
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Right,
                active: false,
                registry,
                console_registry,
                console_storage_key,
                expanded_connections,
                expanded_console_groups,
                expanded_database_groups: HashSet::default(),
                metadata_states: HashMap::default(),
                connection_states: HashMap::default(),
                pending_console_persist: Task::ready(()),
            })
        })
    }

    fn console_storage_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(i64::from)
            .map(|id| id.to_string())
            .or_else(|| workspace.session_id())
            .map(|id| format!("database-viewer-consoles-v1-{id}"))
    }

    fn new_connection_profile(&self) -> ConnectionProfile {
        let driver = DatabaseDriver::PostgreSql;
        let base_name = format!("Local {driver}");
        let matching_connections = self
            .registry
            .connections
            .iter()
            .filter(|connection| connection.name.starts_with(&base_name))
            .count();
        let name = if matching_connections == 0 {
            base_name
        } else {
            format!("{base_name} {}", matching_connections + 1)
        };

        ConnectionProfile::new(name, driver)
    }

    fn open_connection_editor(
        &mut self,
        profile: ConnectionProfile,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            log::error!("database panel workspace was dropped");
            return;
        };
        let panel = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                ConnectionEditorModal::new(panel, profile, is_new, window, cx)
            });
        });
    }

    fn create_console(
        &mut self,
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut index = 1;
        let name = loop {
            let name = if index == 1 {
                "console.sql".to_owned()
            } else {
                format!("console-{index}.sql")
            };
            if !self
                .console_registry
                .consoles
                .iter()
                .any(|console| console.connection_id == profile.id && console.name == name)
            {
                break name;
            }
            index += 1;
        };
        let mut console = QueryConsole::new(profile.id, name);
        console.sql = format!("-- {}\n\n", profile.name);
        self.console_registry.upsert(console.clone());
        self.expanded_connections.insert(profile.id);
        self.expanded_console_groups.insert(profile.id);
        self.persist_consoles(cx);
        self.open_console(profile, console, window, cx);
    }

    fn open_console(
        &mut self,
        profile: ConnectionProfile,
        console: QueryConsole,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            log::error!("database panel workspace was dropped");
            return;
        };
        let workspace_handle = self.workspace.clone();
        let panel = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            let existing = workspace.panes().iter().find_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.downcast::<DatabaseConsole>())
                    .find(|item| item.read(cx).console_id() == console.id)
            });
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }

            let project = workspace.project().clone();
            let console = cx.new(|cx| {
                DatabaseConsole::new(
                    workspace_handle,
                    panel,
                    project,
                    profile,
                    console,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(console), None, true, window, cx);
        });
    }

    async fn saved_password(
        profile: &ConnectionProfile,
        credentials_provider: &dyn credentials_provider::CredentialsProvider,
        cx: &gpui::AsyncApp,
    ) -> Result<Option<String>> {
        credentials_provider
            .read_credentials(&profile.id.credential_key(), cx)
            .await?
            .map(|(_, password)| {
                String::from_utf8(password).context("saved database password is not valid UTF-8")
            })
            .transpose()
    }

    fn load_databases(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        let id = profile.id;
        self.metadata_states.entry(id).or_default().databases = MetadataLoadState::Loading;
        let credentials_provider = zed_credentials_provider::global(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                list_databases(&profile, password.as_deref()).await
            }
            .await;
            this.update(cx, |this, cx| {
                let metadata = this.metadata_states.entry(id).or_default();
                metadata.databases = match result {
                    Ok(databases) => {
                        this.connection_states
                            .insert(id, ConnectionState::Connected);
                        MetadataLoadState::Loaded(databases)
                    }
                    Err(error) => {
                        this.connection_states.insert(id, ConnectionState::Error);
                        MetadataLoadState::Error(error.to_string())
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_tables(
        &mut self,
        profile: ConnectionProfile,
        database: MetadataDatabase,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id;
        self.metadata_states
            .entry(id)
            .or_default()
            .tables
            .insert(database.clone(), MetadataLoadState::Loading);
        let credentials_provider = zed_credentials_provider::global(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                list_tables(&profile, password.as_deref(), &database).await
            }
            .await;
            this.update(cx, |this, cx| {
                let state = match result {
                    Ok(tables) => MetadataLoadState::Loaded(tables),
                    Err(error) => MetadataLoadState::Error(error.to_string()),
                };
                this.metadata_states
                    .entry(id)
                    .or_default()
                    .tables
                    .insert(database, state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_table_details(
        &mut self,
        profile: ConnectionProfile,
        table: MetadataTable,
        cx: &mut Context<Self>,
    ) {
        let id = profile.id;
        self.metadata_states
            .entry(id)
            .or_default()
            .details
            .insert(table.clone(), MetadataLoadState::Loading);
        let credentials_provider = zed_credentials_provider::global(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                describe_table(&profile, password.as_deref(), &table).await
            }
            .await;
            this.update(cx, |this, cx| {
                let state = match result {
                    Ok(details) => MetadataLoadState::Loaded(details),
                    Err(error) => MetadataLoadState::Error(error.to_string()),
                };
                this.metadata_states
                    .entry(id)
                    .or_default()
                    .details
                    .insert(table, state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_database_group(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        let id = profile.id;
        if self.expanded_database_groups.remove(&id) {
            cx.notify();
            return;
        }
        self.expanded_database_groups.insert(id);
        let should_load = matches!(
            self.metadata_states
                .get(&id)
                .map(|metadata| &metadata.databases),
            None | Some(MetadataLoadState::NotLoaded | MetadataLoadState::Error(_))
        );
        if should_load {
            self.load_databases(profile, cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_metadata_database(
        &mut self,
        profile: ConnectionProfile,
        database: MetadataDatabase,
        cx: &mut Context<Self>,
    ) {
        let metadata = self.metadata_states.entry(profile.id).or_default();
        if metadata.expanded_databases.remove(&database) {
            cx.notify();
            return;
        }
        metadata.expanded_databases.insert(database.clone());
        let should_load = matches!(
            metadata.tables.get(&database),
            None | Some(MetadataLoadState::NotLoaded | MetadataLoadState::Error(_))
        );
        if should_load {
            self.load_tables(profile, database, cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_metadata_table(
        &mut self,
        profile: ConnectionProfile,
        table: MetadataTable,
        cx: &mut Context<Self>,
    ) {
        let metadata = self.metadata_states.entry(profile.id).or_default();
        if metadata.expanded_tables.remove(&table) {
            cx.notify();
            return;
        }
        metadata.expanded_tables.insert(table.clone());
        let should_load = matches!(
            metadata.details.get(&table),
            None | Some(MetadataLoadState::NotLoaded | MetadataLoadState::Error(_))
        );
        if should_load {
            self.load_table_details(profile, table, cx);
        } else {
            cx.notify();
        }
    }

    fn open_table_data(
        &mut self,
        profile: ConnectionProfile,
        table: MetadataTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            log::error!("database panel workspace was dropped");
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let existing = workspace.panes().iter().find_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.downcast::<TableDataView>())
                    .find(|item| item.read(cx).matches(profile.id, &table))
            });
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }

            let view = cx.new(|cx| TableDataView::new(profile, table, window, cx));
            view.update(cx, |view, cx| view.refresh(cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    pub(crate) fn update_console_sql(
        &mut self,
        id: ConsoleId,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(console) = self
            .console_registry
            .consoles
            .iter_mut()
            .find(|console| console.id == id)
        else {
            return;
        };
        if console.sql == sql {
            return;
        }
        console.sql = sql;
        self.persist_consoles(cx);
    }

    fn toggle_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if !self.expanded_connections.remove(&id) {
            self.expanded_connections.insert(id);
        }
        cx.notify();
    }

    fn toggle_console_group(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if !self.expanded_console_groups.remove(&id) {
            self.expanded_console_groups.insert(id);
        }
        cx.notify();
    }

    fn persist_consoles(&mut self, cx: &mut Context<Self>) {
        let Some(storage_key) = self.console_storage_key.clone() else {
            return;
        };
        let Ok(serialized) = serde_json::to_string(&self.console_registry) else {
            log::error!("failed to serialize database consoles");
            return;
        };
        let kvp = KeyValueStore::global(cx);
        let executor = cx.background_executor().clone();
        self.pending_console_persist = cx.spawn(async move |_, _| {
            executor.timer(Duration::from_millis(150)).await;
            if let Err(error) = kvp.write_kvp(storage_key, serialized).await {
                log::error!("failed to save database consoles: {error:#}");
            }
        });
    }

    pub(crate) fn upsert_connection(&mut self, profile: ConnectionProfile, cx: &mut Context<Self>) {
        self.registry
            .upsert(profile)
            .expect("the editor validates profiles before saving");
        self.persist(cx);
        cx.notify();
    }

    pub(crate) fn set_connection_state(
        &mut self,
        id: ConnectionId,
        state: ConnectionState,
        cx: &mut Context<Self>,
    ) {
        self.connection_states.insert(id, state);
        cx.notify();
    }

    fn remove_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.registry.remove(id);
        self.console_registry
            .consoles
            .retain(|console| console.connection_id != id);
        self.expanded_connections.remove(&id);
        self.expanded_console_groups.remove(&id);
        self.expanded_database_groups.remove(&id);
        self.metadata_states.remove(&id);
        self.connection_states.remove(&id);
        self.persist(cx);
        self.persist_consoles(cx);
        let credential_key = id.credential_key();
        let credentials_provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |_, cx| {
            if let Err(error) = credentials_provider
                .delete_credentials(&credential_key, cx)
                .await
            {
                log::error!("failed to remove database credentials: {error:#}");
            }
        })
        .detach();
        cx.notify();
    }

    fn persist(&self, cx: &mut Context<Self>) {
        let Ok(serialized) = serde_json::to_string(&self.registry) else {
            log::error!("failed to serialize database connections");
            return;
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            if let Err(error) = kvp
                .write_kvp(CONNECTIONS_STORAGE_KEY.to_owned(), serialized)
                .await
            {
                log::error!("failed to save database connections: {error:#}");
            }
        })
        .detach();
    }

    fn render_empty_state(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .child(
                Icon::new(IconName::DatabaseZap)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Label::new("No database connections").color(Color::Muted))
            .child(
                Label::new("Create a connection to start exploring a database")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_add_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Button::new("new-database-connection", "New Connection")
            .style(ButtonStyle::Outlined)
            .start_icon(Icon::new(IconName::Plus))
            .label_size(LabelSize::Small)
            .on_click(cx.listener(|panel, _, window, cx| {
                let profile = panel.new_connection_profile();
                panel.open_connection_editor(profile, true, window, cx);
            }))
    }

    fn render_metadata_message(
        &self,
        id: impl Into<gpui::ElementId>,
        indent: usize,
        message: impl Into<gpui::SharedString>,
        color: Color,
    ) -> AnyElement {
        ListItem::new(id)
            .inset(true)
            .indent_level(indent)
            .disabled(true)
            .child(Label::new(message).size(LabelSize::Small).color(color))
            .into_any_element()
    }

    fn render_table_details(
        &self,
        connection_id: ConnectionId,
        database_index: usize,
        table_index: usize,
        state: MetadataLoadState<TableMetadataDetails>,
    ) -> Vec<AnyElement> {
        match state {
            MetadataLoadState::NotLoaded | MetadataLoadState::Loading => {
                vec![self.render_metadata_message(
                    format!(
                        "metadata-details-loading-{connection_id}-{database_index}-{table_index}"
                    ),
                    4,
                    "Loading columns and indexes…",
                    Color::Muted,
                )]
            }
            MetadataLoadState::Error(error) => vec![self.render_metadata_message(
                format!("metadata-details-error-{connection_id}-{database_index}-{table_index}"),
                4,
                error,
                Color::Error,
            )],
            MetadataLoadState::Loaded(details) => {
                let primary_key = details.primary_key;
                let mut nodes = vec![self.render_metadata_message(
                    format!("metadata-columns-{connection_id}-{database_index}-{table_index}"),
                    4,
                    format!("Columns ({})", details.columns.len()),
                    Color::Muted,
                )];
                nodes.extend(
                    details
                        .columns
                        .into_iter()
                        .enumerate()
                        .map(|(index, column)| {
                            let is_primary_key = primary_key
                                .iter()
                                .any(|key| key.eq_ignore_ascii_case(&column.name));
                            ListItem::new(format!(
                                "metadata-column-{connection_id}-{database_index}-{table_index}-{index}"
                            ))
                            .inset(true)
                            .indent_level(5)
                            .disabled(true)
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_2()
                                    .child(
                                        Icon::new(IconName::Hash)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(Label::new(column.name).truncate())
                                    .when(is_primary_key, |this| {
                                        this.child(
                                            Label::new("PK")
                                                .size(LabelSize::Small)
                                                .color(Color::Accent),
                                        )
                                    })
                                    .child(
                                        Label::new(column.type_name)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .when(!column.nullable, |this| {
                                        this.child(
                                            Label::new("not null")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                            .into_any_element()
                        }),
                );
                nodes.push(self.render_metadata_message(
                    format!("metadata-indexes-{connection_id}-{database_index}-{table_index}"),
                    4,
                    format!("Indexes ({})", details.indexes.len()),
                    Color::Muted,
                ));
                if details.indexes.is_empty() {
                    nodes.push(self.render_metadata_message(
                        format!(
                            "metadata-no-indexes-{connection_id}-{database_index}-{table_index}"
                        ),
                        5,
                        "No indexes",
                        Color::Muted,
                    ));
                } else {
                    nodes.extend(
                        details
                            .indexes
                            .into_iter()
                            .enumerate()
                            .map(|(index, item)| {
                                ListItem::new(format!(
                                    "metadata-index-{connection_id}-{database_index}-{table_index}-{index}"
                                ))
                                .inset(true)
                                .indent_level(5)
                                .disabled(true)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .min_w_0()
                                        .gap_2()
                                        .child(
                                            Icon::new(IconName::ListTree)
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(Label::new(item.name).truncate())
                                        .when(item.unique, |this| {
                                            this.child(
                                                Label::new("unique")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            )
                                        })
                                        .child(
                                            Label::new(item.columns.join(", "))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                )
                                .into_any_element()
                            }),
                    );
                }
                nodes
            }
        }
    }

    fn render_tables(
        &self,
        profile: &ConnectionProfile,
        database_index: usize,
        state: MetadataLoadState<Vec<MetadataTable>>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let connection_id = profile.id;
        match state {
            MetadataLoadState::NotLoaded | MetadataLoadState::Loading => {
                vec![self.render_metadata_message(
                    format!("metadata-tables-loading-{connection_id}-{database_index}"),
                    3,
                    "Loading tables…",
                    Color::Muted,
                )]
            }
            MetadataLoadState::Error(error) => vec![self.render_metadata_message(
                format!("metadata-tables-error-{connection_id}-{database_index}"),
                3,
                error,
                Color::Error,
            )],
            MetadataLoadState::Loaded(tables) if tables.is_empty() => {
                vec![self.render_metadata_message(
                    format!("metadata-tables-empty-{connection_id}-{database_index}"),
                    3,
                    "No tables or views",
                    Color::Muted,
                )]
            }
            MetadataLoadState::Loaded(tables) => {
                let metadata = self.metadata_states.get(&connection_id);
                let mut nodes = Vec::new();
                for (table_index, table) in tables.into_iter().enumerate() {
                    let expanded =
                        metadata.is_some_and(|metadata| metadata.expanded_tables.contains(&table));
                    let table_label = table
                        .schema
                        .as_deref()
                        .map(|schema| format!("{schema}.{}", table.name))
                        .unwrap_or_else(|| table.name.clone());
                    let profile_for_open = profile.clone();
                    let table_for_open = table.clone();
                    let profile_for_toggle = profile.clone();
                    let table_for_toggle = table.clone();
                    nodes.push(
                        ListItem::new(format!(
                            "metadata-table-{connection_id}-{database_index}-{table_index}"
                        ))
                        .inset(true)
                        .indent_level(3)
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.open_table_data(
                                profile_for_open.clone(),
                                table_for_open.clone(),
                                window,
                                cx,
                            );
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .child(
                                    IconButton::new(
                                        format!(
                                            "toggle-table-details-{connection_id}-{database_index}-{table_index}"
                                        ),
                                        if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        },
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Show columns and indexes"))
                                    .on_click(cx.listener(move |panel, _, _, cx| {
                                        cx.stop_propagation();
                                        panel.toggle_metadata_table(
                                            profile_for_toggle.clone(),
                                            table_for_toggle.clone(),
                                            cx,
                                        );
                                    })),
                                )
                                .child(
                                    Icon::new(IconName::FileTree)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(table_label).truncate())
                                .child(
                                    Label::new(table.table_type.to_lowercase())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .into_any_element(),
                    );
                    if expanded {
                        let details = metadata
                            .and_then(|metadata| metadata.details.get(&table))
                            .cloned()
                            .unwrap_or_default();
                        nodes.extend(self.render_table_details(
                            connection_id,
                            database_index,
                            table_index,
                            details,
                        ));
                    }
                }
                nodes
            }
        }
    }

    fn render_databases(
        &self,
        profile: &ConnectionProfile,
        databases: Vec<MetadataDatabase>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let connection_id = profile.id;
        let metadata = self.metadata_states.get(&connection_id);
        let mut nodes = Vec::new();
        for (database_index, database) in databases.into_iter().enumerate() {
            let expanded =
                metadata.is_some_and(|metadata| metadata.expanded_databases.contains(&database));
            let profile_for_toggle = profile.clone();
            let database_for_toggle = database.clone();
            nodes.push(
                ListItem::new(format!(
                    "metadata-database-{connection_id}-{database_index}"
                ))
                .inset(true)
                .indent_level(2)
                .on_click(cx.listener(move |panel, _, _, cx| {
                    panel.toggle_metadata_database(
                        profile_for_toggle.clone(),
                        database_for_toggle.clone(),
                        cx,
                    );
                }))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::DatabaseZap)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(database.name.clone()).truncate()),
                )
                .into_any_element(),
            );
            if expanded {
                let tables = metadata
                    .and_then(|metadata| metadata.tables.get(&database))
                    .cloned()
                    .unwrap_or_default();
                nodes.extend(self.render_tables(profile, database_index, tables, cx));
            }
        }
        nodes
    }

    fn render_console_group(
        &self,
        profile: ConnectionProfile,
        consoles: Vec<QueryConsole>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let connection_id = profile.id;
        let expanded = self.expanded_console_groups.contains(&connection_id);
        let profile_for_create = profile.clone();
        let mut group = v_flex().w_full().child(
            ListItem::new(format!("database-consoles-{connection_id}"))
                .inset(true)
                .indent_level(1)
                .on_click(
                    cx.listener(move |panel, _, _, cx| {
                        panel.toggle_console_group(connection_id, cx)
                    }),
                )
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::TerminalAlt)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().child(Label::new("Consoles")))
                        .child(
                            IconButton::new(format!("new-console-{connection_id}"), IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("New query console"))
                                .on_click(cx.listener(move |panel, _, window, cx| {
                                    cx.stop_propagation();
                                    panel.create_console(profile_for_create.clone(), window, cx);
                                })),
                        ),
                ),
        );
        if expanded {
            if consoles.is_empty() {
                group = group.child(self.render_metadata_message(
                    format!("database-consoles-empty-{connection_id}"),
                    2,
                    "No query consoles",
                    Color::Muted,
                ));
            } else {
                group = group.children(consoles.into_iter().map(|console| {
                    let profile = profile.clone();
                    let console_for_open = console.clone();
                    ListItem::new(console.id.to_string())
                        .inset(true)
                        .indent_level(2)
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.open_console(
                                profile.clone(),
                                console_for_open.clone(),
                                window,
                                cx,
                            );
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::FileDoc)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(console.name).truncate()),
                        )
                }));
            }
        }
        group.into_any_element()
    }

    fn render_database_group(
        &self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let connection_id = profile.id;
        let expanded = self.expanded_database_groups.contains(&connection_id);
        let state = self
            .metadata_states
            .get(&connection_id)
            .map(|metadata| metadata.databases.clone())
            .unwrap_or_default();
        let profile_for_toggle = profile.clone();
        let profile_for_refresh = profile.clone();
        let mut group = v_flex().w_full().child(
            ListItem::new(format!("metadata-databases-{connection_id}"))
                .inset(true)
                .indent_level(1)
                .on_click(cx.listener(move |panel, _, _, cx| {
                    panel.toggle_database_group(profile_for_toggle.clone(), cx)
                }))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::Server)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new("Databases").flex_1())
                        .child(
                            IconButton::new(
                                format!("refresh-databases-{connection_id}"),
                                IconName::RefreshTitle,
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh databases"))
                            .on_click(cx.listener(
                                move |panel, _, _, cx| {
                                    cx.stop_propagation();
                                    panel.load_databases(profile_for_refresh.clone(), cx);
                                },
                            )),
                        ),
                ),
        );

        if expanded {
            group = match state {
                MetadataLoadState::NotLoaded | MetadataLoadState::Loading => {
                    group.child(self.render_metadata_message(
                        format!("metadata-databases-loading-{connection_id}"),
                        2,
                        "Loading databases…",
                        Color::Muted,
                    ))
                }
                MetadataLoadState::Error(error) => group.child(self.render_metadata_message(
                    format!("metadata-databases-error-{connection_id}"),
                    2,
                    error,
                    Color::Error,
                )),
                MetadataLoadState::Loaded(databases) if databases.is_empty() => {
                    group.child(self.render_metadata_message(
                        format!("metadata-databases-empty-{connection_id}"),
                        2,
                        "No databases reported by the JDBC driver",
                        Color::Muted,
                    ))
                }
                MetadataLoadState::Loaded(databases) => {
                    group.children(self.render_databases(&profile, databases, cx))
                }
            };
        }
        group.into_any_element()
    }

    fn render_connection(
        &self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = profile.id;
        let expanded = self.expanded_connections.contains(&id);
        let consoles = self
            .console_registry
            .consoles
            .iter()
            .filter(|console| console.connection_id == id)
            .cloned()
            .collect::<Vec<_>>();
        let profile_for_consoles = profile.clone();
        let profile_for_metadata = profile.clone();
        let connection_state = self.connection_states.get(&id).cloned();
        v_flex()
            .w_full()
            .child(
                ListItem::new(profile.id.to_string())
                    .inset(true)
                    .on_click(cx.listener(move |panel, _, _, cx| panel.toggle_connection(id, cx)))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(
                                Icon::new(IconName::DatabaseZap)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .child(Label::new(profile.name.clone()).truncate())
                                    .child(
                                        Label::new(profile.jdbc_url.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                            .when_some(connection_state, |this, state| match state {
                                ConnectionState::Connected => this.child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                ),
                                ConnectionState::Error => this.child(
                                    Icon::new(IconName::XCircle)
                                        .size(IconSize::Small)
                                        .color(Color::Error),
                                ),
                            })
                            .child({
                                IconButton::new(format!("edit-{id}"), IconName::Pencil)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Edit connection"))
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        cx.stop_propagation();
                                        panel.open_connection_editor(
                                            profile.clone(),
                                            false,
                                            window,
                                            cx,
                                        );
                                    }))
                            })
                            .child(
                                IconButton::new(format!("remove-{id}"), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Remove connection"))
                                    .on_click(cx.listener(move |panel, _, _, cx| {
                                        cx.stop_propagation();
                                        panel.remove_connection(id, cx);
                                    })),
                            ),
                    ),
            )
            .when(expanded, |this| {
                this.child(self.render_console_group(profile_for_consoles, consoles, cx))
                    .child(self.render_database_group(profile_for_metadata, cx))
            })
    }
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Render for DatabasePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connections = self.registry.connections.clone();

        v_flex()
            .id("database-panel")
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(34.))
                    .flex_none()
                    .justify_between()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Connections")),
            )
            .child(
                h_flex()
                    .min_h(px(34.))
                    .flex_none()
                    .flex_wrap()
                    .gap_1()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.render_add_button(cx)),
            )
            .child(if connections.is_empty() {
                self.render_empty_state(cx).into_any_element()
            } else {
                v_flex()
                    .id("database-connections-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .py_1()
                    .children(
                        connections
                            .into_iter()
                            .map(|profile| self.render_connection(profile, cx)),
                    )
                    .into_any_element()
            })
    }
}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "Database Panel"
    }

    fn panel_key() -> &'static str {
        DATABASE_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, _: &mut Context<Self>) {
        self.position = position;
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Database Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}
