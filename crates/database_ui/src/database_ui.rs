mod connection_editor;
mod console;
mod results_panel;

use connection_editor::ConnectionEditorModal;
use console::DatabaseConsole;
use database::{
    ConnectionId, ConnectionProfile, ConnectionRegistry, ConsoleId, ConsoleRegistry,
    DatabaseDriver, QueryConsole,
};
use db::kvp::KeyValueStore;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Pixels, Render, StatefulInteractiveElement, Task, WeakEntity, Window,
    px,
};
pub use results_panel::DatabaseResultsPanel;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
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
    connection_states: HashMap<ConnectionId, ConnectionState>,
    pending_console_persist: Task<()>,
}

#[derive(Clone)]
pub(crate) enum ConnectionState {
    Connected,
    Error,
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
                                let profile = profile.clone();
                                IconButton::new(format!("new-console-{id}"), IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("New query console"))
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        cx.stop_propagation();
                                        panel.create_console(profile.clone(), window, cx);
                                    }))
                            })
                            .child({
                                let profile = profile.clone();
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
                this.children(consoles.into_iter().map(|console| {
                    let profile = profile.clone();
                    let console_for_open = console.clone();
                    ListItem::new(console.id.to_string())
                        .inset(true)
                        .indent_level(1)
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
                }))
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
