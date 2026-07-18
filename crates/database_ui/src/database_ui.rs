use database::{ConnectionId, ConnectionProfile, ConnectionRegistry, DatabaseDriver};
use db::kvp::KeyValueStore;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Pixels, Render, StatefulInteractiveElement, WeakEntity, Window, px,
};
use ui::{
    Button, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, ListItem,
    Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::database_panel::{Toggle, ToggleFocus};

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
    })
    .detach();
}

pub struct DatabasePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    registry: ConnectionRegistry,
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

        workspace.update_in(&mut cx, move |_, _, cx| {
            cx.new(|cx| Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Right,
                active: false,
                registry,
            })
        })
    }

    fn add_connection(&mut self, driver: DatabaseDriver, cx: &mut Context<Self>) {
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

        self.registry
            .add(ConnectionProfile::new(name, driver))
            .expect("the generated connection profile should be valid");
        self.persist(cx);
        cx.notify();
    }

    fn remove_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.registry
            .connections
            .retain(|connection| connection.id != id);
        self.persist(cx);
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
                Label::new("Choose a driver above to create a local draft")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_add_button(
        &self,
        driver: DatabaseDriver,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        Button::new(format!("add-{driver:?}"), driver.display_name())
            .style(ButtonStyle::OutlinedGhost)
            .label_size(LabelSize::Small)
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.add_connection(driver, cx);
            }))
    }

    fn render_connection(
        &self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = profile.id;
        ListItem::new(profile.id.to_string()).inset(true).child(
            h_flex()
                .w_full()
                .gap_2()
                .child(
                    Icon::new(IconName::DatabaseZap)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .flex_1()
                        .child(Label::new(profile.name).truncate())
                        .child(
                            Label::new(profile.jdbc_url)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .child(
                    IconButton::new(format!("remove-{id}"), IconName::Trash)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Remove connection"))
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.remove_connection(id, cx);
                        })),
                ),
        )
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
                    .gap_1()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.render_add_button(DatabaseDriver::PostgreSql, cx))
                    .child(self.render_add_button(DatabaseDriver::MySql, cx))
                    .child(self.render_add_button(DatabaseDriver::ClickHouse, cx))
                    .child(self.render_add_button(DatabaseDriver::Sqlite, cx)),
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
