use crate::{DatabasePanel, DatabaseResultsPanel, results_panel::StatementExecution};
use anyhow::{Context as _, Result, anyhow};
use database::{
    ConnectionProfile, ConsoleId, QueryConsole, execute_query, split_sql_statements,
    sql_statement_at_offset,
};
use editor::{Editor, EditorEvent, EditorMode};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, Subscription,
    Task, WeakEntity, Window, px,
};
use language::Buffer;
use multi_buffer::{MultiBuffer, MultiBufferOffset};
use project::Project;
use std::path::Path;
use ui::{
    Banner, Button, ButtonStyle, Color, Icon, IconName, Label, LabelSize, Severity, prelude::*,
};
use workspace::{Item, Workspace};

const CONSOLE_MAX_ROWS: u32 = 200;

#[derive(Clone, Copy)]
enum ExecutionTarget {
    SelectionOrCurrent,
    All,
}

pub(crate) struct DatabaseConsole {
    workspace: WeakEntity<Workspace>,
    profile: ConnectionProfile,
    console_id: ConsoleId,
    console_name: String,
    editor: Entity<Editor>,
    is_running: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    _language_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl DatabaseConsole {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<DatabasePanel>,
        project: Entity<Project>,
        profile: ConnectionProfile,
        console: QueryConsole,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let buffer = cx.new(|cx| {
            let buffer = Buffer::local(console.sql.clone(), cx);
            buffer.set_language_registry(language_registry.clone());
            buffer
        });
        let multi_buffer = cx
            .new(|cx| MultiBuffer::singleton(buffer.clone(), cx).with_title(console.name.clone()));
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::new(EditorMode::full(), multi_buffer, Some(project), window, cx);
            editor.set_placeholder_text("Write one or more SQL statements…", window, cx);
            editor.set_show_runnables(false, cx);
            editor.set_use_modal_editing(true);
            editor
        });

        let console_id = console.id;
        let subscription = cx.subscribe(&editor, move |_, editor, event, cx| {
            if matches!(event, EditorEvent::Edited { .. }) {
                let sql = editor.read(cx).text(cx);
                panel
                    .update(cx, |panel, cx| {
                        panel.update_console_sql(console_id, sql, cx)
                    })
                    .ok();
            }
        });

        let console_name = console.name.clone();
        let language_path = console.name.clone();
        let language_task = cx.spawn(async move |_, cx| {
            let Ok(language) = language_registry
                .load_language_for_file_path(Path::new(&language_path))
                .await
            else {
                return;
            };
            buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
        });

        Self {
            workspace,
            profile,
            console_id,
            console_name,
            editor,
            is_running: false,
            error: None,
            task: None,
            _language_task: language_task,
            _subscriptions: vec![subscription],
        }
    }

    pub(crate) fn console_id(&self) -> ConsoleId {
        self.console_id
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

    fn statements_for_execution(
        &self,
        target: ExecutionTarget,
        cx: &mut Context<Self>,
    ) -> Result<Vec<String>> {
        let (sql, selection) = self.editor.update(cx, |editor, cx| {
            let sql = editor.text(cx);
            let display_snapshot = editor.display_snapshot(cx);
            let selection = editor
                .selections
                .newest::<MultiBufferOffset>(&display_snapshot);
            (sql, selection)
        });

        let ranges = match target {
            ExecutionTarget::All => split_sql_statements(&sql),
            ExecutionTarget::SelectionOrCurrent if !selection.is_empty() => {
                let selection = selection.range();
                let start = selection.start.0.min(sql.len());
                let end = selection.end.0.min(sql.len());
                let selected_sql = sql
                    .get(start..end)
                    .ok_or_else(|| anyhow!("The SQL selection is not on valid text boundaries"))?;
                return statements_from_text(selected_sql);
            }
            ExecutionTarget::SelectionOrCurrent => {
                sql_statement_at_offset(&sql, selection.head().0)
                    .into_iter()
                    .collect()
            }
        };

        let statements = ranges
            .into_iter()
            .filter_map(|range| sql.get(range).map(str::to_owned))
            .collect::<Vec<_>>();
        if statements.is_empty() {
            return Err(anyhow!("No executable SQL statement found"));
        }
        Ok(statements)
    }

    fn results_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<DatabaseResultsPanel>> {
        let workspace = self.workspace.upgrade()?;
        let results_panel = workspace.read(cx).panel::<DatabaseResultsPanel>(cx)?;
        workspace.update(cx, |workspace, cx| {
            workspace.open_panel::<DatabaseResultsPanel>(window, cx)
        });
        Some(results_panel)
    }

    fn execute(&mut self, target: ExecutionTarget, window: &mut Window, cx: &mut Context<Self>) {
        let statements = match self.statements_for_execution(target, cx) {
            Ok(statements) => statements,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let Some(results_panel) = self.results_panel(window, cx) else {
            self.error = Some("The database results panel is not available".to_owned());
            cx.notify();
            return;
        };

        let profile = self.profile.clone();
        let console_name = self.console_name.clone();
        let credentials_provider = zed_credentials_provider::global(cx);
        results_panel.update(cx, |panel, cx| {
            panel.start(console_name.clone(), statements.len(), cx)
        });
        self.is_running = true;
        self.error = None;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let password = Self::saved_password(&profile, credentials_provider.as_ref(), cx).await;
            let mut executions = Vec::with_capacity(statements.len());
            match password {
                Ok(password) => {
                    for statement in statements {
                        let result = execute_query(
                            &profile,
                            password.as_deref(),
                            &statement,
                            CONSOLE_MAX_ROWS,
                        )
                        .await
                        .map_err(|error| error.to_string());
                        executions.push(StatementExecution { statement, result });
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    executions.extend(statements.into_iter().map(|statement| StatementExecution {
                        statement,
                        result: Err(error.clone()),
                    }));
                }
            }

            results_panel.update(cx, |panel, cx| panel.complete(console_name, executions, cx));
            this.update(cx, |this, cx| {
                this.is_running = false;
                cx.notify();
            })
            .ok();
        }));
    }
}

fn statements_from_text(sql: &str) -> Result<Vec<String>> {
    let statements = split_sql_statements(sql)
        .into_iter()
        .filter_map(|range| sql.get(range).map(str::to_owned))
        .collect::<Vec<_>>();
    if statements.is_empty() {
        Err(anyhow!(
            "No executable SQL statement found in the selection"
        ))
    } else {
        Ok(statements)
    }
}

impl Focusable for DatabaseConsole {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<()> for DatabaseConsole {}

impl Item for DatabaseConsole {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        self.console_name.clone().into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDoc))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<gpui::SharedString> {
        Some(format!("{} · {}", self.profile.name, self.console_name).into())
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
        window.focus(&self.editor.focus_handle(cx), cx);
    }
}

impl Render for DatabaseConsole {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("database-console")
            .key_context("DatabaseConsole")
            .track_focus(&self.editor.focus_handle(cx))
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
                            .min_w_0()
                            .gap_2()
                            .child(Icon::new(IconName::DatabaseZap).color(Color::Muted))
                            .child(Label::new(self.profile.name.clone()).truncate())
                            .child(
                                Label::new(self.console_name.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when(self.profile.read_only, |this| {
                                this.child(
                                    Label::new("Read-only")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(
                                    "run-selected-database-query",
                                    "Run Selection / Current",
                                )
                                .style(ButtonStyle::Filled)
                                .start_icon(Icon::new(IconName::PlayFilled))
                                .disabled(self.is_running)
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.execute(
                                            ExecutionTarget::SelectionOrCurrent,
                                            window,
                                            cx,
                                        )
                                    },
                                )),
                            )
                            .child(
                                Button::new("run-all-database-queries", "Run All")
                                    .style(ButtonStyle::Outlined)
                                    .disabled(self.is_running)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.execute(ExecutionTarget::All, window, cx)
                                    })),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div().flex_none().p_2().child(
                        Banner::new()
                            .severity(Severity::Error)
                            .wrap_content(true)
                            .child(Label::new(error).size(LabelSize::Small)),
                    ),
                )
            })
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}
