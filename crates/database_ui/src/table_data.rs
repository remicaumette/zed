use anyhow::{Context as _, Result};
use database::{ConnectionId, ConnectionProfile, MetadataTable, QueryResult, browse_table};
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

const TABLE_DATA_MAX_ROWS: u32 = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone)]
enum TableDataState {
    Empty,
    Loading,
    Loaded(QueryResult),
    Error(String),
}

pub(crate) struct TableDataView {
    profile: ConnectionProfile,
    table: MetadataTable,
    where_clause: Entity<InputField>,
    order_by: Entity<InputField>,
    sort: Option<(String, SortDirection)>,
    state: TableDataState,
    task: Option<Task<()>>,
}

impl TableDataView {
    pub(crate) fn new(
        profile: ConnectionProfile,
        table: MetadataTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let where_clause = cx.new(|cx| {
            InputField::new(window, cx, "Expression, for example: status = 'active'")
                .label("WHERE")
                .tab_index(0)
        });
        let order_by = cx.new(|cx| {
            InputField::new(window, cx, "Columns, for example: created_at desc")
                .label("ORDER BY")
                .tab_index(1)
        });
        Self {
            profile,
            table,
            where_clause,
            order_by,
            sort: None,
            state: TableDataState::Empty,
            task: None,
        }
    }

    pub(crate) fn matches(&self, connection_id: ConnectionId, table: &MetadataTable) -> bool {
        self.profile.id == connection_id && self.table == *table
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

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let where_clause = self.where_clause.read(cx).text(cx);
        let order_by = self.order_by.read(cx).text(cx);
        if self
            .sort
            .as_ref()
            .map(|(column, direction)| self.order_expression(column, *direction))
            .is_some_and(|expression| expression != order_by.trim())
        {
            self.sort = None;
        }

        let profile = self.profile.clone();
        let table = self.table.clone();
        let credentials_provider = zed_credentials_provider::global(cx);
        self.state = TableDataState::Loading;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                browse_table(
                    &profile,
                    password.as_deref(),
                    &table,
                    Some(&where_clause),
                    Some(&order_by),
                    TABLE_DATA_MAX_ROWS,
                )
                .await
            }
            .await;

            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(result) => TableDataState::Loaded(result),
                    Err(error) => TableDataState::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn order_expression(&self, column: &str, direction: SortDirection) -> String {
        let column = if let Some(quote) = self.table.identifier_quote.as_deref() {
            format!(
                "{quote}{}{quote}",
                column.replace(quote, &format!("{quote}{quote}"))
            )
        } else {
            column.to_owned()
        };
        let direction = match direction {
            SortDirection::Ascending => "asc",
            SortDirection::Descending => "desc",
        };
        format!("{column} {direction}")
    }

    fn sort_by(&mut self, column: String, window: &mut Window, cx: &mut Context<Self>) {
        self.sort = match &self.sort {
            Some((current, SortDirection::Ascending)) if current == &column => {
                Some((column, SortDirection::Descending))
            }
            Some((current, SortDirection::Descending)) if current == &column => None,
            _ => Some((column, SortDirection::Ascending)),
        };

        let expression = self
            .sort
            .as_ref()
            .map(|(column, direction)| self.order_expression(column, *direction))
            .unwrap_or_default();
        self.order_by
            .update(cx, |input, cx| input.set_text(&expression, window, cx));
        self.refresh(cx);
    }

    fn render_result(
        &mut self,
        result: &QueryResult,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        if result.columns.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("The table returned no columns").color(Color::Muted))
                .into_any_element();
        }

        let headers = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let column_name = column.label.clone();
                let direction = self.sort.as_ref().and_then(|(sorted_column, direction)| {
                    (sorted_column == &column_name).then_some(*direction)
                });
                v_flex()
                    .min_w_0()
                    .child(
                        Button::new(
                            format!("table-data-sort-column-{index}"),
                            column.label.clone(),
                        )
                        .style(ButtonStyle::Subtle)
                        .when_some(direction, |button, direction| {
                            button.end_icon(
                                Icon::new(match direction {
                                    SortDirection::Ascending => IconName::ArrowUp,
                                    SortDirection::Descending => IconName::ArrowDown,
                                })
                                .size(ui::IconSize::XSmall),
                            )
                        })
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.sort_by(column_name.clone(), window, cx)
                            },
                        )),
                    )
                    .child(
                        Label::new(column.type_name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .into_any_element()
            })
            .collect();
        let mut table = Table::new(result.columns.len())
            .width(px((result.columns.len().max(1) * 180) as f32))
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
            .child(
                h_flex()
                    .min_h(px(28.))
                    .flex_none()
                    .gap_2()
                    .px_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(format!(
                            "{} row{} in {} ms",
                            result.rows.len(),
                            if result.rows.len() == 1 { "" } else { "s" },
                            result.elapsed_millis
                        ))
                        .size(LabelSize::Small),
                    )
                    .when(result.truncated, |this| {
                        this.child(
                            Label::new(format!("Limited to {TABLE_DATA_MAX_ROWS} rows"))
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .child(
                div()
                    .id("database-table-data-scroll")
                    .flex_1()
                    .overflow_scroll()
                    .child(table),
            )
            .into_any_element()
    }
}

impl Focusable for TableDataView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.where_clause.focus_handle(cx)
    }
}

impl EventEmitter<()> for TableDataView {}

impl Item for TableDataView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        self.table.name.clone().into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::DatabaseZap))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<gpui::SharedString> {
        Some(format!("{} · {}", self.profile.name, self.table.name).into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Database Table Data Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        _workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for TableDataView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.clone();
        v_flex()
            .id("database-table-data")
            .key_context("DatabaseTableData")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .flex_none()
                    .items_end()
                    .gap_2()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(div().min_w_0().flex_1().child(self.where_clause.clone()))
                    .child(div().min_w_0().flex_1().child(self.order_by.clone()))
                    .child(
                        Button::new("refresh-database-table-data", "Apply")
                            .style(ButtonStyle::Filled)
                            .start_icon(Icon::new(IconName::RefreshTitle))
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(match state {
                TableDataState::Empty => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(Label::new("Load table data").color(Color::Muted))
                    .into_any_element(),
                TableDataState::Loading => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(Label::new("Loading table data…").color(Color::Muted))
                    .into_any_element(),
                TableDataState::Loaded(result) => {
                    self.render_result(&result, cx).into_any_element()
                }
                TableDataState::Error(error) => div()
                    .flex_1()
                    .p_3()
                    .child(
                        Banner::new()
                            .severity(Severity::Error)
                            .wrap_content(true)
                            .child(Label::new(error).size(LabelSize::Small)),
                    )
                    .into_any_element(),
            })
    }
}
