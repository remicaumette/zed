use std::collections::HashSet;

use anyhow::{Context as _, Result, anyhow};
use database::{
    ConnectionId, ConnectionProfile, MetadataColumn, MetadataTable, QueryColumn, QueryResult,
    TableChanges, TableInsert, TableMetadataDetails, TableMutationCell, TableRowDelete,
    TableRowUpdate, apply_table_changes, browse_table, describe_table,
};
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Subscription, Task, Window, px,
};
use ui::{
    Banner, Button, ButtonStyle, Color, Icon, IconName, Label, LabelSize, Severity, Table, Tooltip,
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
    Loaded(LoadedTableData),
    Error(String),
}

#[derive(Clone)]
struct EditableRow {
    original: Option<Vec<Option<String>>>,
    values: Vec<Option<String>>,
    edited_columns: HashSet<usize>,
    deleted: bool,
}

#[derive(Clone)]
struct LoadedTableData {
    columns: Vec<QueryColumn>,
    rows: Vec<EditableRow>,
    details: TableMetadataDetails,
    truncated: bool,
    values_truncated: bool,
    elapsed_millis: u64,
}

impl LoadedTableData {
    fn new(result: QueryResult, details: TableMetadataDetails) -> Self {
        Self {
            columns: result.columns,
            rows: result
                .rows
                .into_iter()
                .map(|values| EditableRow {
                    original: Some(values.clone()),
                    values,
                    edited_columns: HashSet::default(),
                    deleted: false,
                })
                .collect(),
            details,
            truncated: result.truncated,
            values_truncated: result.values_truncated,
            elapsed_millis: result.elapsed_millis,
        }
    }

    fn metadata_column(&self, name: &str) -> Option<&MetadataColumn> {
        self.details
            .columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }

    fn has_changes(&self) -> bool {
        self.rows.iter().any(|row| {
            row.deleted
                || row
                    .original
                    .as_ref()
                    .is_none_or(|original| original != &row.values)
        })
    }

    fn change_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| {
                row.deleted
                    || row
                        .original
                        .as_ref()
                        .is_none_or(|original| original != &row.values)
            })
            .count()
    }
}

struct ActiveCellEditor {
    row: usize,
    column: usize,
    input: Entity<InputField>,
    _focus_out_subscription: Subscription,
}

pub(crate) struct TableDataView {
    profile: ConnectionProfile,
    table: MetadataTable,
    where_clause: Entity<InputField>,
    order_by: Entity<InputField>,
    sort: Option<(String, SortDirection)>,
    state: TableDataState,
    editing_cell: Option<ActiveCellEditor>,
    is_saving: bool,
    save_error: Option<String>,
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
            editing_cell: None,
            is_saving: false,
            save_error: None,
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

    fn has_pending_changes(&self) -> bool {
        self.editing_cell.is_some()
            || matches!(&self.state, TableDataState::Loaded(data) if data.has_changes())
    }

    fn can_edit(&self) -> bool {
        !self.profile.read_only
            && matches!(
                &self.state,
                TableDataState::Loaded(data)
                    if !data.details.primary_key.is_empty() && !data.values_truncated
            )
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.commit_active_edit(cx);
        if self.has_pending_changes() {
            self.save_error = Some("Save or discard the pending changes before reloading".into());
            cx.notify();
            return;
        }
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
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
        self.editing_cell = None;
        self.is_saving = false;
        self.save_error = None;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                let details = describe_table(&profile, password.as_deref(), &table).await?;
                let result = browse_table(
                    &profile,
                    password.as_deref(),
                    &table,
                    Some(&where_clause),
                    Some(&order_by),
                    TABLE_DATA_MAX_ROWS,
                )
                .await?;
                Ok::<_, anyhow::Error>((result, details))
            }
            .await;

            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok((result, details)) => {
                        TableDataState::Loaded(LoadedTableData::new(result, details))
                    }
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

    fn commit_active_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editing_cell.take() else {
            return;
        };
        let text = editor.input.read(cx).text(cx);
        let value = if text.eq_ignore_ascii_case("NULL") {
            None
        } else {
            Some(text)
        };
        if let TableDataState::Loaded(data) = &mut self.state
            && let Some(row) = data.rows.get_mut(editor.row)
            && let Some(cell) = row.values.get_mut(editor.column)
        {
            *cell = value;
            row.edited_columns.insert(editor.column);
        }
    }

    fn start_editing(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_edit() || self.is_saving {
            return;
        }
        self.commit_active_edit(cx);
        let Some(value) = (match &self.state {
            TableDataState::Loaded(data) => data
                .rows
                .get(row)
                .and_then(|row| (!row.deleted).then_some(row))
                .and_then(|row| row.values.get(column))
                .cloned(),
            _ => None,
        }) else {
            return;
        };
        let input = cx.new(|cx| {
            let input = InputField::new(window, cx, "NULL").label_min_width(px(80.));
            input.set_text(value.as_deref().unwrap_or("NULL"), window, cx);
            input
        });
        let focus_handle = input.focus_handle(cx);
        let focus_out_subscription = cx.on_focus_out(&focus_handle, window, |this, _, _, cx| {
            this.commit_active_edit(cx);
            cx.notify();
        });
        self.editing_cell = Some(ActiveCellEditor {
            row,
            column,
            input,
            _focus_out_subscription: focus_out_subscription,
        });
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    fn add_row(&mut self, cx: &mut Context<Self>) {
        if !self.can_edit() || self.is_saving {
            return;
        }
        self.commit_active_edit(cx);
        if let TableDataState::Loaded(data) = &mut self.state {
            data.rows.push(EditableRow {
                original: None,
                values: vec![None; data.columns.len()],
                edited_columns: HashSet::default(),
                deleted: false,
            });
            cx.notify();
        }
    }

    fn toggle_delete(&mut self, row_index: usize, cx: &mut Context<Self>) {
        if !self.can_edit() || self.is_saving {
            return;
        }
        self.commit_active_edit(cx);
        if let TableDataState::Loaded(data) = &mut self.state {
            if data
                .rows
                .get(row_index)
                .is_some_and(|row| row.original.is_none())
            {
                data.rows.remove(row_index);
            } else if let Some(row) = data.rows.get_mut(row_index) {
                row.deleted = !row.deleted;
            }
            cx.notify();
        }
    }

    fn discard_changes(&mut self, cx: &mut Context<Self>) {
        self.editing_cell = None;
        self.save_error = None;
        if let TableDataState::Loaded(data) = &mut self.state {
            data.rows.retain(|row| row.original.is_some());
            for row in &mut data.rows {
                row.values = row.original.clone().unwrap_or_default();
                row.edited_columns.clear();
                row.deleted = false;
            }
            cx.notify();
        }
    }

    fn row_key(
        data: &LoadedTableData,
        values: &[Option<String>],
    ) -> Result<Vec<TableMutationCell>> {
        data.details
            .primary_key
            .iter()
            .map(|primary_key| {
                let index = data
                    .columns
                    .iter()
                    .position(|column| column.label.eq_ignore_ascii_case(primary_key))
                    .ok_or_else(|| {
                        anyhow!("Primary key column `{primary_key}` is not in the result")
                    })?;
                let value = values
                    .get(index)
                    .cloned()
                    .flatten()
                    .ok_or_else(|| anyhow!("Primary key column `{primary_key}` is NULL"))?;
                Ok(TableMutationCell {
                    column: primary_key.clone(),
                    value: Some(value),
                })
            })
            .collect()
    }

    fn build_changes(&self) -> Result<TableChanges> {
        let TableDataState::Loaded(data) = &self.state else {
            return Err(anyhow!("Table data is not loaded"));
        };
        if data.details.primary_key.is_empty() {
            return Err(anyhow!("The table has no primary key"));
        }

        let mut changes = TableChanges::default();
        for row in &data.rows {
            match &row.original {
                None if !row.deleted => {
                    let values = data
                        .columns
                        .iter()
                        .enumerate()
                        .filter_map(|(index, column)| {
                            let value = row.values.get(index).cloned().unwrap_or_default();
                            let use_default = !row.edited_columns.contains(&index)
                                && value.is_none()
                                && data.metadata_column(&column.label).is_some_and(|metadata| {
                                    metadata.auto_increment || metadata.default_value.is_some()
                                });
                            (!use_default).then(|| TableMutationCell {
                                column: column.label.clone(),
                                value,
                            })
                        })
                        .collect();
                    changes.inserts.push(TableInsert { values });
                }
                None => {}
                Some(original) if row.deleted => {
                    changes.deletes.push(TableRowDelete {
                        key: Self::row_key(data, original)?,
                    });
                }
                Some(original) => {
                    let values = data
                        .columns
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| original.get(*index) != row.values.get(*index))
                        .map(|(index, column)| TableMutationCell {
                            column: column.label.clone(),
                            value: row.values.get(index).cloned().unwrap_or_default(),
                        })
                        .collect::<Vec<_>>();
                    if !values.is_empty() {
                        changes.updates.push(TableRowUpdate {
                            key: Self::row_key(data, original)?,
                            values,
                        });
                    }
                }
            }
        }
        Ok(changes)
    }

    fn save_changes(&mut self, cx: &mut Context<Self>) {
        if !self.can_edit() || self.is_saving {
            return;
        }
        self.commit_active_edit(cx);
        let changes = match self.build_changes() {
            Ok(changes) => changes,
            Err(error) => {
                self.save_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        if changes.updates.is_empty() && changes.inserts.is_empty() && changes.deletes.is_empty() {
            cx.notify();
            return;
        }

        let profile = self.profile.clone();
        let table = self.table.clone();
        let credentials_provider = zed_credentials_provider::global(cx);
        self.is_saving = true;
        self.save_error = None;
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                apply_table_changes(&profile, password.as_deref(), &table, &changes).await
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok(_) => this.reload(cx),
                Err(error) => {
                    this.is_saving = false;
                    this.save_error = Some(error.to_string());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn sort_by(&mut self, column: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_pending_changes() || self.is_saving {
            self.save_error = Some("Save or discard the pending changes before sorting".into());
            cx.notify();
            return;
        }
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
        data: &LoadedTableData,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        if data.columns.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("The table returned no columns").color(Color::Muted))
                .into_any_element();
        }

        let can_edit = self.can_edit();
        let mut headers = Vec::new();
        if can_edit {
            headers.push(
                Label::new("Row")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            );
        }
        headers.extend(data.columns.iter().enumerate().map(|(index, column)| {
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.sort_by(column_name.clone(), window, cx)
                    })),
                )
                .child(
                    Label::new(column.type_name.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .into_any_element()
        }));
        let table_column_count = data.columns.len() + usize::from(can_edit);
        let mut table = Table::new(table_column_count)
            .width(px((table_column_count.max(1) * 180) as f32))
            .header(headers)
            .column_borders();
        for (row_index, row) in data.rows.iter().enumerate() {
            let mut cells = Vec::new();
            if can_edit {
                let deleted = row.deleted;
                cells.push(
                    IconButton::new(
                        format!("table-data-delete-row-{row_index}"),
                        if deleted {
                            IconName::RotateCcw
                        } else {
                            IconName::Trash
                        },
                    )
                    .icon_size(ui::IconSize::Small)
                    .icon_color(if deleted { Color::Muted } else { Color::Error })
                    .disabled(self.is_saving)
                    .tooltip(Tooltip::text(if deleted {
                        "Restore row"
                    } else {
                        "Delete row when changes are saved"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_delete(row_index, cx)))
                    .into_any_element(),
                );
            }
            cells.extend(row.values.iter().enumerate().map(|(column_index, value)| {
                if let Some(editor) = self
                    .editing_cell
                    .as_ref()
                    .filter(|editor| editor.row == row_index && editor.column == column_index)
                {
                    return div()
                        .id((
                            "table-data-editor",
                            row_index * data.columns.len() + column_index,
                        ))
                        .w_full()
                        .child(editor.input.clone())
                        .into_any_element();
                }

                let changed = row
                    .original
                    .as_ref()
                    .is_none_or(|original| original.get(column_index) != Some(value));
                let uses_default = row.original.is_none()
                    && !row.edited_columns.contains(&column_index)
                    && value.is_none()
                    && data
                        .columns
                        .get(column_index)
                        .and_then(|column| data.metadata_column(&column.label))
                        .is_some_and(|metadata| {
                            metadata.auto_increment || metadata.default_value.is_some()
                        });
                let label = if uses_default {
                    Label::new("DEFAULT")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                } else {
                    match value {
                        Some(value) => Label::new(value.clone()).size(LabelSize::Small),
                        None => Label::new("NULL")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    }
                }
                .when(row.deleted, |label| label.strikethrough());
                div()
                    .id((
                        "table-data-cell",
                        row_index * data.columns.len() + column_index,
                    ))
                    .w_full()
                    .when(row.deleted, |this| {
                        this.bg(cx.theme().status().deleted_background.opacity(0.35))
                    })
                    .when(changed && !row.deleted, |this| {
                        this.bg(if row.original.is_none() {
                            cx.theme().status().created_background.opacity(0.35)
                        } else {
                            cx.theme().status().warning_background.opacity(0.35)
                        })
                    })
                    .when(can_edit && !row.deleted && !self.is_saving, |this| {
                        this.cursor_text().on_click(cx.listener(
                            move |this, event: &ClickEvent, window, cx| {
                                if event.click_count() >= 2 {
                                    this.start_editing(row_index, column_index, window, cx);
                                }
                            },
                        ))
                    })
                    .child(label)
                    .into_any_element()
            }));
            table = table.row(cells);
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
                            data.rows.len(),
                            if data.rows.len() == 1 { "" } else { "s" },
                            data.elapsed_millis
                        ))
                        .size(LabelSize::Small),
                    )
                    .when(data.truncated, |this| {
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
        let can_edit = self.can_edit();
        let has_changes = self.has_pending_changes();
        let change_count = match &self.state {
            TableDataState::Loaded(data) => {
                let active_row_is_counted = self
                    .editing_cell
                    .as_ref()
                    .and_then(|editor| data.rows.get(editor.row))
                    .is_some_and(|row| {
                        row.deleted
                            || row
                                .original
                                .as_ref()
                                .is_none_or(|original| original != &row.values)
                    });
                data.change_count()
                    + usize::from(self.editing_cell.is_some() && !active_row_is_counted)
            }
            _ => 0,
        };
        let editing_status = match &self.state {
            TableDataState::Loaded(_) if self.profile.read_only => {
                Some("Editing disabled: connection is read-only")
            }
            TableDataState::Loaded(data) if data.details.primary_key.is_empty() => {
                Some("Editing disabled: table has no primary key")
            }
            TableDataState::Loaded(data) if data.values_truncated => {
                Some("Editing disabled: one or more cell values were truncated")
            }
            TableDataState::Loaded(_) => {
                Some("Double-click a cell to edit it; type NULL to store SQL NULL")
            }
            _ => None,
        };
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
                            .disabled(has_changes || self.is_saving)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(
                h_flex()
                    .min_h(px(36.))
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .when_some(editing_status, |this, status| {
                        this.child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("add-database-table-row", "Add Row")
                            .style(ButtonStyle::Outlined)
                            .start_icon(Icon::new(IconName::Plus))
                            .disabled(!can_edit || self.is_saving)
                            .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
                    )
                    .child(
                        Button::new("discard-database-table-changes", "Discard")
                            .style(ButtonStyle::Subtle)
                            .disabled(!has_changes || self.is_saving)
                            .on_click(cx.listener(|this, _, _, cx| this.discard_changes(cx))),
                    )
                    .child(
                        Button::new(
                            "save-database-table-changes",
                            if self.is_saving {
                                "Saving…".to_owned()
                            } else if change_count > 0 {
                                format!("Save Changes ({change_count})")
                            } else {
                                "Save Changes".to_owned()
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .start_icon(Icon::new(IconName::Check))
                        .disabled(!can_edit || !has_changes || self.is_saving)
                        .on_click(cx.listener(|this, _, _, cx| this.save_changes(cx))),
                    ),
            )
            .when_some(self.save_error.clone(), |this, error| {
                this.child(
                    div().flex_none().p_2().child(
                        Banner::new()
                            .severity(Severity::Error)
                            .wrap_content(true)
                            .child(Label::new(error).size(LabelSize::Small)),
                    ),
                )
            })
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
                TableDataState::Loaded(data) => self.render_result(&data, cx).into_any_element(),
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
