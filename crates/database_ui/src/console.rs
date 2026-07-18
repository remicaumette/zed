use anyhow::{Context as _, Result};
use database::{ConnectionProfile, QueryResult, execute_query};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, Task, Window,
    px,
};
use ui::{
    Banner, Button, ButtonStyle, Color, Icon, IconName, Label, LabelSize, Severity, Table,
    prelude::*,
};
use ui_input::InputField;
use workspace::{Item, Workspace};

const CONSOLE_MAX_ROWS: u32 = 200;

pub(crate) struct DatabaseConsole {
    profile: ConnectionProfile,
    sql: Entity<InputField>,
    status: ConsoleStatus,
    task: Option<Task<()>>,
}

enum ConsoleStatus {
    Idle,
    Running,
    Completed(QueryResult),
    Error(String),
}

impl DatabaseConsole {
    pub(crate) fn new(
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sql = cx.new(|cx| {
            let input = InputField::new(window, cx, "Enter a SQL statement…")
                .label("SQL")
                .tab_index(0);
            input.editor().set_multiline(Some(10), window, cx);
            input.set_text("select 1;", window, cx);
            input
        });

        Self {
            profile,
            sql,
            status: ConsoleStatus::Idle,
            task: None,
        }
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

    fn execute(&mut self, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).text(cx);
        if sql.trim().is_empty() {
            self.status = ConsoleStatus::Error("Enter a SQL statement to run".to_owned());
            cx.notify();
            return;
        }

        let profile = self.profile.clone();
        let credentials_provider = zed_credentials_provider::global(cx);
        self.status = ConsoleStatus::Running;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                execute_query(&profile, password.as_deref(), &sql, CONSOLE_MAX_ROWS).await
            }
            .await;

            this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(result) => ConsoleStatus::Completed(result),
                    Err(error) => ConsoleStatus::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_completed(result: &QueryResult, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
            .min_h(px(32.))
            .flex_none()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(summary).size(LabelSize::Small))
            .when(result.truncated, |this| {
                this.child(
                    Label::new(format!("Result truncated at {CONSOLE_MAX_ROWS} rows"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            });

        if result.columns.is_empty() {
            return v_flex()
                .size_full()
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
            .child(status)
            .child(
                div()
                    .id("database-result-scroll")
                    .flex_1()
                    .overflow_scroll()
                    .child(table),
            )
            .into_any_element()
    }
}

impl Focusable for DatabaseConsole {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.sql.focus_handle(cx)
    }
}

impl EventEmitter<()> for DatabaseConsole {}

impl Item for DatabaseConsole {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        format!("{} Console", self.profile.name).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Terminal))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Database Console Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        _workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.sql.focus_handle(cx), cx);
    }
}

impl Render for DatabaseConsole {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_running = matches!(self.status, ConsoleStatus::Running);

        v_flex()
            .id("database-console")
            .key_context("DatabaseConsole")
            .track_focus(&self.sql.focus_handle(cx))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .min_h(px(36.))
                    .flex_none()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::DatabaseZap).color(Color::Muted))
                            .child(Label::new(self.profile.name.clone()))
                            .when(self.profile.read_only, |this| {
                                this.child(
                                    Label::new("Read-only")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        Button::new(
                            "run-database-query",
                            if is_running { "Running…" } else { "Run" },
                        )
                        .style(ButtonStyle::Filled)
                        .start_icon(Icon::new(IconName::PlayFilled))
                        .disabled(is_running)
                        .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.sql.clone()),
            )
            .child(match &self.status {
                ConsoleStatus::Idle => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(Label::new("Run a statement to see its result").color(Color::Muted))
                    .into_any_element(),
                ConsoleStatus::Running => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(Label::new("Executing query…").color(Color::Muted))
                    .into_any_element(),
                ConsoleStatus::Completed(result) => {
                    Self::render_completed(result, cx).into_any_element()
                }
                ConsoleStatus::Error(error) => v_flex()
                    .flex_1()
                    .p_3()
                    .child(
                        Banner::new()
                            .severity(Severity::Error)
                            .wrap_content(true)
                            .child(Label::new(error.clone()).size(LabelSize::Small)),
                    )
                    .into_any_element(),
            })
    }
}
