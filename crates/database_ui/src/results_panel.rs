use database::QueryResult;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, WeakEntity, Window, px,
};
use ui::{
    Banner, Button, ButtonStyle, Color, IconName, Label, LabelSize, Severity, Table,
    Toggleable as _, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::database_panel::ToggleResultsFocus;

const DATABASE_RESULTS_PANEL_KEY: &str = "DatabaseResultsPanel";

pub(crate) struct StatementExecution {
    pub statement: String,
    pub result: Result<QueryResult, String>,
}

enum ResultsState {
    Empty,
    Running {
        console_name: String,
        statement_count: usize,
    },
    Completed {
        console_name: String,
        executions: Vec<StatementExecution>,
    },
}

pub struct DatabaseResultsPanel {
    _workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    active: bool,
    selected_result: usize,
    state: ResultsState,
}

impl DatabaseResultsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let workspace_handle = workspace.clone();
        workspace.update_in(&mut cx, move |_, _, cx| {
            cx.new(|cx| Self {
                _workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                active: false,
                selected_result: 0,
                state: ResultsState::Empty,
            })
        })
    }

    pub(crate) fn start(
        &mut self,
        console_name: String,
        statement_count: usize,
        cx: &mut Context<Self>,
    ) {
        self.selected_result = 0;
        self.state = ResultsState::Running {
            console_name,
            statement_count,
        };
        cx.notify();
    }

    pub(crate) fn complete(
        &mut self,
        console_name: String,
        executions: Vec<StatementExecution>,
        cx: &mut Context<Self>,
    ) {
        self.selected_result = self.selected_result.min(executions.len().saturating_sub(1));
        self.state = ResultsState::Completed {
            console_name,
            executions,
        };
        cx.notify();
    }

    fn render_query_result(
        execution: &StatementExecution,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let statement_label = execution
            .statement
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let statement_label = if statement_label.chars().count() > 160 {
            format!("{}…", statement_label.chars().take(160).collect::<String>())
        } else {
            statement_label
        };

        let statement_header = div()
            .flex_none()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(statement_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            );

        let result = match &execution.result {
            Ok(result) => result,
            Err(error) => {
                return v_flex()
                    .size_full()
                    .child(statement_header)
                    .child(
                        div().p_3().child(
                            Banner::new()
                                .severity(Severity::Error)
                                .wrap_content(true)
                                .child(Label::new(error.clone()).size(LabelSize::Small)),
                        ),
                    )
                    .into_any_element();
            }
        };

        let summary = if let Some(affected_rows) = result.affected_rows {
            format!(
                "{affected_rows} row{} affected in {} ms",
                if affected_rows == 1 { "" } else { "s" },
                result.elapsed_millis
            )
        } else {
            format!(
                "{} row{} returned in {} ms",
                result.rows.len(),
                if result.rows.len() == 1 { "" } else { "s" },
                result.elapsed_millis
            )
        };
        let status = h_flex()
            .min_h(px(28.))
            .flex_none()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(summary).size(LabelSize::Small))
            .when(result.truncated, |this| {
                this.child(
                    Label::new("Result truncated")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            });

        if result.columns.is_empty() {
            return v_flex()
                .size_full()
                .child(statement_header)
                .child(status)
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .child(Label::new("Statement completed").color(Color::Muted)),
                )
                .into_any_element();
        }

        let column_count = result.columns.len();
        let headers = result
            .columns
            .iter()
            .map(|column| {
                v_flex()
                    .min_w_0()
                    .child(Label::new(column.label.clone()).truncate())
                    .child(
                        Label::new(column.type_name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .into_any_element()
            })
            .collect();
        let mut table = Table::new(column_count)
            .width(px((column_count.max(1) * 180) as f32))
            .header(headers)
            .striped();
        for row in &result.rows {
            table = table.row(
                row.iter()
                    .map(|value| match value {
                        Some(value) => Label::new(value.clone())
                            .size(LabelSize::Small)
                            .into_any_element(),
                        None => Label::new("NULL")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .into_any_element(),
                    })
                    .collect(),
            );
        }

        v_flex()
            .size_full()
            .child(statement_header)
            .child(status)
            .child(
                div()
                    .id("database-results-scroll")
                    .flex_1()
                    .overflow_scroll()
                    .child(table),
            )
            .into_any_element()
    }
}

impl Focusable for DatabaseResultsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabaseResultsPanel {}

impl Render for DatabaseResultsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            ResultsState::Empty => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("Run a SQL statement to see its results").color(Color::Muted))
                .into_any_element(),
            ResultsState::Running {
                console_name,
                statement_count,
            } => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new(format!(
                        "Executing {statement_count} statement{} from {console_name}…",
                        if *statement_count == 1 { "" } else { "s" }
                    ))
                    .color(Color::Muted),
                )
                .into_any_element(),
            ResultsState::Completed {
                console_name,
                executions,
            } => {
                let selected = self.selected_result.min(executions.len().saturating_sub(1));
                v_flex()
                    .size_full()
                    .child(
                        h_flex()
                            .min_h(px(34.))
                            .flex_none()
                            .gap_1()
                            .px_2()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(
                                Label::new(console_name.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .children(executions.iter().enumerate().map(|(index, execution)| {
                                let failed = execution.result.is_err();
                                Button::new(
                                    format!("database-result-{index}"),
                                    format!("Result {}", index + 1),
                                )
                                .style(if failed {
                                    ButtonStyle::Tinted(ui::TintColor::Error)
                                } else {
                                    ButtonStyle::Subtle
                                })
                                .toggle_state(index == selected)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.selected_result = index;
                                        cx.notify();
                                    },
                                ))
                            })),
                    )
                    .when_some(executions.get(selected), |this, execution| {
                        this.child(Self::render_query_result(execution, cx))
                    })
                    .into_any_element()
            }
        };

        v_flex()
            .id("database-results-panel")
            .key_context("DatabaseResultsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .child(content)
    }
}

impl Panel for DatabaseResultsPanel {
    fn persistent_name() -> &'static str {
        "Database Results"
    }

    fn panel_key() -> &'static str {
        DATABASE_RESULTS_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Database Query Results")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleResultsFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        self.active
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}
