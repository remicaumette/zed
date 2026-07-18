use std::{cell::Cell, collections::HashSet, rc::Rc};

use anyhow::{Context as _, Result, anyhow};
use chrono::Local;
use database::{
    ConnectionId, ConnectionProfile, MetadataColumn, MetadataForeignKey, MetadataTable,
    QueryColumn, QueryResult, TableChanges, TableInsert, TableMetadataDetails, TableMutationCell,
    TableRowDelete, TableRowUpdate, apply_table_changes, browse_table, describe_table,
};
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    PromptLevel, Render, Subscription, Task, WeakEntity, Window, px,
};
use project::Project;
use ui::{
    Banner, Button, ButtonStyle, Color, ContextMenu, Icon, IconButton, IconButtonShape, IconName,
    Label, LabelSize, PopoverMenu, Severity, Table, prelude::*, right_click_menu,
};
use ui_input::{ErasedEditorEvent, InputField};
use workspace::{
    Item, Workspace,
    item::{ItemBufferKind, SaveOptions},
};

use crate::date_picker::{DatePicker, TemporalCellKind, date_from_value};

const TABLE_DATA_PAGE_SIZE: u32 = 100;

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
    values_truncated: bool,
    has_more_rows: bool,
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
            values_truncated: result.values_truncated,
            has_more_rows: result.has_more_rows,
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
    original: Option<String>,
    edited: Rc<Cell<bool>>,
    date_picker_open: Rc<Cell<bool>>,
    _input_subscription: Subscription,
    _focus_out_subscription: Subscription,
}

#[derive(Clone)]
struct RelationTarget {
    table: MetadataTable,
    where_clause: String,
    filters: Vec<TableMutationCell>,
}

#[derive(Clone)]
enum TableNavigation {
    Reload {
        page: u32,
    },
    Sort {
        sort: Option<(String, SortDirection)>,
        expression: String,
    },
    Relation(RelationTarget),
}

pub(crate) struct TableDataView {
    profile: ConnectionProfile,
    table: MetadataTable,
    workspace: WeakEntity<Workspace>,
    where_clause: Entity<InputField>,
    order_by: Entity<InputField>,
    sort: Option<(String, SortDirection)>,
    relation_filters: Vec<TableMutationCell>,
    relation_where_display: Option<String>,
    page: u32,
    selected_row: Option<usize>,
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
        workspace: WeakEntity<Workspace>,
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
            workspace,
            where_clause,
            order_by,
            sort: None,
            relation_filters: Vec::new(),
            relation_where_display: None,
            page: 0,
            selected_row: None,
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
        self.editing_cell
            .as_ref()
            .is_some_and(|editor| editor.edited.get())
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
        self.reload(cx);
    }

    fn request_navigation(
        &mut self,
        navigation: TableNavigation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_saving {
            return;
        }
        if !matches!(&navigation, TableNavigation::Relation(_))
            && self.relation_where_display.as_deref()
                != Some(self.where_clause.read(cx).text(cx).as_str())
        {
            self.relation_filters.clear();
            self.relation_where_display = None;
        }
        self.commit_active_edit(cx);
        if !self.has_pending_changes() {
            self.perform_navigation(navigation, window, cx);
            return;
        }

        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard pending table changes?",
            Some("This navigation reloads or replaces the current result set."),
            &["Discard Changes", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await? == 0 {
                this.update_in(cx, |this, window, cx| {
                    this.discard_changes(cx);
                    this.perform_navigation(navigation, window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach();
    }

    fn perform_navigation(
        &mut self,
        navigation: TableNavigation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match navigation {
            TableNavigation::Reload { page } => {
                self.page = page;
                self.selected_row = None;
                self.reload(cx);
            }
            TableNavigation::Sort { sort, expression } => {
                self.sort = sort;
                self.page = 0;
                self.selected_row = None;
                self.order_by
                    .update(cx, |input, cx| input.set_text(&expression, window, cx));
                self.reload(cx);
            }
            TableNavigation::Relation(relation) => {
                self.open_relation(relation, window, cx);
            }
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let where_clause = self.where_clause.read(cx).text(cx);
        let relation_filters = self.relation_filters.clone();
        let effective_where_clause = relation_filters.is_empty().then_some(where_clause);
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
        let offset = self.page.saturating_mul(TABLE_DATA_PAGE_SIZE);
        let credentials_provider = zed_credentials_provider::global(cx);
        self.state = TableDataState::Loading;
        self.editing_cell = None;
        self.selected_row = None;
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
                    effective_where_clause.as_deref(),
                    Some(&order_by),
                    &relation_filters,
                    offset,
                    TABLE_DATA_PAGE_SIZE,
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
        let value = if editor.edited.get() {
            Some(text)
        } else {
            editor.original
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
            let input = InputField::new(window, cx, "Enter a value").label_min_width(px(80.));
            input.set_text(value.as_deref().unwrap_or_default(), window, cx);
            input
        });
        let edited = Rc::new(Cell::new(false));
        let edited_for_subscription = edited.clone();
        let view = cx.weak_entity();
        let input_editor = input.read(cx).editor().clone();
        let input_subscription = input_editor.subscribe(
            Box::new(move |event, _, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    edited_for_subscription.set(true);
                    view.update(cx, |_, cx| cx.notify()).ok();
                }
            }),
            window,
            cx,
        );
        let focus_handle = input.focus_handle(cx);
        let date_picker_open = Rc::new(Cell::new(false));
        let date_picker_open_for_focus = date_picker_open.clone();
        let view_for_focus = cx.weak_entity();
        let focus_out_subscription =
            cx.on_focus_out(&focus_handle, window, move |_, _, window, _cx| {
                let date_picker_open = date_picker_open_for_focus.clone();
                let view = view_for_focus.clone();
                window.on_next_frame(move |window, _| {
                    window.on_next_frame(move |_, cx| {
                        if !date_picker_open.get() {
                            view.update(cx, |this, cx| {
                                let same_cell = this.editing_cell.as_ref().is_some_and(|editor| {
                                    editor.row == row && editor.column == column
                                });
                                if same_cell {
                                    this.commit_active_edit(cx);
                                    cx.notify();
                                }
                            })
                            .ok();
                        }
                    });
                });
            });
        self.editing_cell = Some(ActiveCellEditor {
            row,
            column,
            input,
            original: value,
            edited,
            date_picker_open,
            _input_subscription: input_subscription,
            _focus_out_subscription: focus_out_subscription,
        });
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    fn set_cell_null(&mut self, row_index: usize, column_index: usize, cx: &mut Context<Self>) {
        if !self.can_edit() || self.is_saving {
            return;
        }
        self.commit_active_edit(cx);
        if let TableDataState::Loaded(data) = &mut self.state
            && let Some(row) = data.rows.get_mut(row_index)
            && !row.deleted
            && let Some(cell) = row.values.get_mut(column_index)
        {
            *cell = None;
            row.edited_columns.insert(column_index);
            self.selected_row = Some(row_index);
            cx.notify();
        }
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
            self.selected_row = data.rows.len().checked_sub(1);
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
                self.selected_row = self.selected_row.and_then(|selected| {
                    if selected == row_index {
                        None
                    } else if selected > row_index {
                        Some(selected - 1)
                    } else {
                        Some(selected)
                    }
                });
            } else if let Some(row) = data.rows.get_mut(row_index) {
                row.deleted = !row.deleted;
                self.selected_row = Some(row_index);
            }
            cx.notify();
        }
    }

    fn toggle_selected_row(&mut self, cx: &mut Context<Self>) {
        if let Some(row_index) = self.selected_row {
            self.toggle_delete(row_index, cx);
        }
    }

    fn discard_changes(&mut self, cx: &mut Context<Self>) {
        self.editing_cell = None;
        self.selected_row = None;
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
        let task = self.save_changes_task(cx);
        self.task = Some(cx.spawn(async move |_, _| {
            if let Err(error) = task.await {
                log::error!("failed to save database table changes: {error:#}");
            }
        }));
    }

    fn save_changes_task(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if !self.can_edit() || self.is_saving {
            return Task::ready(Ok(()));
        }
        self.commit_active_edit(cx);
        let changes = match self.build_changes() {
            Ok(changes) => changes,
            Err(error) => {
                self.save_error = Some(error.to_string());
                cx.notify();
                return Task::ready(Err(error));
            }
        };
        if changes.updates.is_empty() && changes.inserts.is_empty() && changes.deletes.is_empty() {
            cx.notify();
            return Task::ready(Ok(()));
        }

        let profile = self.profile.clone();
        let table = self.table.clone();
        let credentials_provider = zed_credentials_provider::global(cx);
        self.is_saving = true;
        self.save_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let password =
                    Self::saved_password(&profile, credentials_provider.as_ref(), cx).await?;
                apply_table_changes(&profile, password.as_deref(), &table, &changes).await
            }
            .await;
            match result {
                Ok(_) => {
                    this.update(cx, |this, cx| this.reload(cx))?;
                    Ok(())
                }
                Err(error) => {
                    let message = error.to_string();
                    this.update(cx, |this, cx| {
                        this.is_saving = false;
                        this.save_error = Some(message);
                        cx.notify();
                    })?;
                    Err(error)
                }
            }
        })
    }

    fn sort_by(&mut self, column: String, window: &mut Window, cx: &mut Context<Self>) {
        let sort = match &self.sort {
            Some((current, SortDirection::Ascending)) if current == &column => {
                Some((column, SortDirection::Descending))
            }
            Some((current, SortDirection::Descending)) if current == &column => None,
            _ => Some((column, SortDirection::Ascending)),
        };

        let expression = sort
            .as_ref()
            .map(|(column, direction)| self.order_expression(column, *direction))
            .unwrap_or_default();
        self.request_navigation(TableNavigation::Sort { sort, expression }, window, cx);
    }

    fn relation_for_cell(
        &self,
        data: &LoadedTableData,
        row_index: usize,
        column_index: usize,
    ) -> Option<RelationTarget> {
        if data.values_truncated {
            return None;
        }
        let column = data.columns.get(column_index)?;
        let foreign_key = data.details.foreign_keys.iter().find(|foreign_key| {
            foreign_key
                .columns
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&column.label))
        })?;
        let row = data.rows.get(row_index)?;
        let (where_clause, filters) =
            Self::relation_where_clause(data, row, foreign_key, &self.table)?;
        Some(RelationTarget {
            table: MetadataTable {
                catalog: foreign_key.referenced_catalog.clone(),
                schema: foreign_key.referenced_schema.clone(),
                name: foreign_key.referenced_table.clone(),
                table_type: "TABLE".into(),
                identifier_quote: self.table.identifier_quote.clone(),
            },
            where_clause,
            filters,
        })
    }

    fn relation_where_clause(
        data: &LoadedTableData,
        row: &EditableRow,
        foreign_key: &MetadataForeignKey,
        source_table: &MetadataTable,
    ) -> Option<(String, Vec<TableMutationCell>)> {
        if foreign_key.columns.len() != foreign_key.referenced_columns.len() {
            return None;
        }
        let predicates = foreign_key
            .columns
            .iter()
            .zip(&foreign_key.referenced_columns)
            .map(|(column, referenced_column)| {
                let index = data
                    .columns
                    .iter()
                    .position(|candidate| candidate.label.eq_ignore_ascii_case(column))?;
                let value = row.values.get(index)?.as_ref()?;
                let value = value.replace('\'', "''");
                Some(format!(
                    "{} = '{value}'",
                    Self::quote_identifier(
                        referenced_column,
                        source_table.identifier_quote.as_deref()
                    )
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let filters = foreign_key
            .columns
            .iter()
            .zip(&foreign_key.referenced_columns)
            .map(|(column, referenced_column)| {
                let index = data
                    .columns
                    .iter()
                    .position(|candidate| candidate.label.eq_ignore_ascii_case(column))?;
                Some(TableMutationCell {
                    column: referenced_column.clone(),
                    value: row.values.get(index)?.clone(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some((predicates.join(" AND "), filters))
    }

    fn quote_identifier(identifier: &str, quote: Option<&str>) -> String {
        if let Some(quote) = quote {
            format!(
                "{quote}{}{quote}",
                identifier.replace(quote, &format!("{quote}{quote}"))
            )
        } else {
            identifier.to_owned()
        }
    }

    fn view_relation(
        &mut self,
        row_index: usize,
        column_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let relation = match &self.state {
            TableDataState::Loaded(data) => self.relation_for_cell(data, row_index, column_index),
            _ => None,
        };
        if let Some(relation) = relation {
            self.request_navigation(TableNavigation::Relation(relation), window, cx);
        }
    }

    fn open_relation(
        &mut self,
        relation: RelationTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile = self.profile.clone();
        let workspace_handle = self.workspace.clone();
        let source_view = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace_handle.upgrade() else {
                source_view
                    .update(cx, |this, cx| {
                        this.save_error = Some("The workspace is no longer available".into());
                        cx.notify();
                    })
                    .ok();
                return;
            };
            workspace.update(cx, |workspace, cx| {
                let view = cx.new(|cx| {
                    TableDataView::new(profile, relation.table, workspace_handle, window, cx)
                });
                view.update(cx, |view, cx| {
                    view.where_clause.update(cx, |input, cx| {
                        input.set_text(&relation.where_clause, window, cx)
                    });
                    view.relation_filters = relation.filters;
                    view.relation_where_display = Some(relation.where_clause);
                    view.reload(cx);
                });
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            });
        });
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
        let headers = data
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
            .collect::<Vec<_>>();
        let table_column_count = data.columns.len();
        let mut table = Table::new(table_column_count)
            .width(px((table_column_count.max(1) * 180) as f32))
            .header(headers)
            .column_borders();
        for (row_index, row) in data.rows.iter().enumerate() {
            let cells = row
                .values
                .iter()
                .enumerate()
                .map(|(column_index, value)| {
                    if let Some(editor) = self
                        .editing_cell
                        .as_ref()
                        .filter(|editor| editor.row == row_index && editor.column == column_index)
                    {
                        let temporal_kind = data
                            .columns
                            .get(column_index)
                            .and_then(|column| data.metadata_column(&column.label))
                            .and_then(|metadata| {
                                TemporalCellKind::from_metadata(
                                    metadata.jdbc_type,
                                    &metadata.type_name,
                                )
                            });
                        let mut editor_element = h_flex()
                            .id((
                                "table-data-editor",
                                row_index * data.columns.len() + column_index,
                            ))
                            .w_full()
                            .min_w_0()
                            .child(div().min_w_0().flex_1().child(editor.input.clone()));

                        if let Some(temporal_kind) = temporal_kind {
                            let picker_label = match temporal_kind {
                                TemporalCellKind::Date => "Choose date",
                                TemporalCellKind::Timestamp => "Choose date and time",
                            };
                            let input_for_picker = editor.input.clone();
                            let edited = editor.edited.clone();
                            let date_picker_open = editor.date_picker_open.clone();
                            let date_picker_open_for_menu = date_picker_open.clone();
                            let date_picker_open_for_open = date_picker_open;
                            let view = cx.weak_entity();
                            editor_element = editor_element.child(
                                PopoverMenu::new((
                                    "database-table-date-picker",
                                    row_index * data.columns.len() + column_index,
                                ))
                                .trigger(
                                    IconButton::new(
                                        (
                                            "database-table-date-picker-trigger",
                                            row_index * data.columns.len() + column_index,
                                        ),
                                        IconName::Clock,
                                    )
                                    .shape(IconButtonShape::Square)
                                    .aria_label(picker_label),
                                )
                                .anchor(gpui::Anchor::TopRight)
                                .on_open(Rc::new(move |_, _| date_picker_open_for_open.set(true)))
                                .menu(move |window, cx| {
                                    let current_value = input_for_picker.read(cx).text(cx);
                                    let selected = date_from_value(&current_value)
                                        .unwrap_or_else(|| Local::now().date_naive());
                                    let time = temporal_kind.picker_time(&current_value);
                                    let input = input_for_picker.clone();
                                    let edited = edited.clone();
                                    let view = view.clone();
                                    let date_picker_open = date_picker_open_for_menu.clone();
                                    Some(cx.new(|cx| {
                                        DatePicker::new(
                                            selected,
                                            temporal_kind,
                                            time,
                                            date_picker_open,
                                            Box::new(move |date, time, window, cx| {
                                                let current_value = input.read(cx).text(cx);
                                                let value = temporal_kind.value_from_picker(
                                                    &current_value,
                                                    date,
                                                    time.as_deref(),
                                                );
                                                input.update(cx, |input, cx| {
                                                    input.set_text(&value, window, cx)
                                                });
                                                edited.set(true);
                                                view.update(cx, |_, cx| cx.notify()).ok();
                                            }),
                                            window,
                                            cx,
                                        )
                                    }))
                                }),
                            );
                        }

                        return editor_element.into_any_element();
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
                    let cell = div()
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
                        .into_any_element();

                    let relation_available = self
                        .relation_for_cell(data, row_index, column_index)
                        .is_some();
                    let can_row_action = can_edit && !self.is_saving;
                    let nullable = data
                        .columns
                        .get(column_index)
                        .and_then(|column| data.metadata_column(&column.label))
                        .is_some_and(|column| column.nullable);
                    let can_set_null = can_row_action && !row.deleted && nullable;
                    let deleted = row.deleted;
                    let this = cx.weak_entity();
                    right_click_menu((
                        "database-table-cell-menu",
                        row_index * data.columns.len() + column_index,
                    ))
                    .trigger(move |_, _, _| cell)
                    .maybe_menu(move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.selected_row = Some(row_index);
                            cx.notify();
                        })
                        .ok();
                        if !can_row_action && !relation_available {
                            return None;
                        }

                        let set_null_view = this.clone();
                        let delete_view = this.clone();
                        let relation_view = this.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            if can_set_null {
                                menu = menu.entry("Set NULL", None, {
                                    let set_null_view = set_null_view.clone();
                                    move |_, cx| {
                                        set_null_view
                                            .update(cx, |this, cx| {
                                                this.set_cell_null(row_index, column_index, cx)
                                            })
                                            .ok();
                                    }
                                });
                            }
                            if can_row_action {
                                menu = menu.entry(
                                    if deleted { "Restore Row" } else { "Delete Row" },
                                    None,
                                    {
                                        let delete_view = delete_view.clone();
                                        move |_, cx| {
                                            delete_view
                                                .update(cx, |this, cx| {
                                                    this.toggle_delete(row_index, cx)
                                                })
                                                .ok();
                                        }
                                    },
                                );
                            }
                            if relation_available {
                                if can_row_action {
                                    menu = menu.separator();
                                }
                                menu = menu.entry("View Relation", None, {
                                    let relation_view = relation_view.clone();
                                    move |window, cx| {
                                        relation_view
                                            .update(cx, |this, cx| {
                                                this.view_relation(
                                                    row_index,
                                                    column_index,
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .ok();
                                    }
                                });
                            }
                            menu
                        }))
                    })
                    .into_any_element()
                })
                .collect::<Vec<_>>();
            table = table.row(cells);
        }

        let selected_row = self.selected_row;
        let table_view = cx.weak_entity();
        table = table.map_row(move |(row_index, row), _, cx| {
            let table_view = table_view.clone();
            row.when(selected_row == Some(row_index), |row| {
                row.bg(cx.theme().colors().element_selected.opacity(0.55))
            })
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                table_view
                    .update(cx, |this, cx| {
                        this.selected_row = Some(row_index);
                        cx.notify();
                    })
                    .ok();
            })
            .into_any_element()
        });

        let offset = self.page.saturating_mul(TABLE_DATA_PAGE_SIZE) as usize;
        let first_row = (!data.rows.is_empty()).then_some(offset + 1);
        let last_row = offset + data.rows.len();
        let previous_page = self.page.saturating_sub(1);
        let next_page = self.page.saturating_add(1);

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
                            "{} · Page {} · {} ms",
                            first_row
                                .map(|first| format!("Rows {first}–{last_row}"))
                                .unwrap_or_else(|| "No rows".into()),
                            self.page.saturating_add(1),
                            data.elapsed_millis
                        ))
                        .size(LabelSize::Small),
                    )
                    .when(data.values_truncated, |this| {
                        this.child(
                            Label::new("Some cell values were truncated")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("database-table-previous-page", "Previous")
                            .style(ButtonStyle::Subtle)
                            .disabled(self.page == 0 || self.is_saving)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_navigation(
                                    TableNavigation::Reload {
                                        page: previous_page,
                                    },
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("database-table-next-page", "Next")
                            .style(ButtonStyle::Subtle)
                            .disabled(!data.has_more_rows || self.is_saving)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_navigation(
                                    TableNavigation::Reload { page: next_page },
                                    window,
                                    cx,
                                )
                            })),
                    ),
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

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, _cx: &App) -> bool {
        self.has_pending_changes()
    }

    fn can_save(&self, _cx: &App) -> bool {
        self.can_edit() && self.has_pending_changes() && !self.is_saving
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.save_changes_task(cx)
    }

    fn reload(
        &mut self,
        _project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.discard_changes(cx);
        TableDataView::reload(self, cx);
        Task::ready(Ok(()))
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
                    + usize::from(
                        self.editing_cell
                            .as_ref()
                            .is_some_and(|editor| editor.edited.get())
                            && !active_row_is_counted,
                    )
            }
            _ => 0,
        };
        let selected_row_deleted = self.selected_row.and_then(|row_index| match &self.state {
            TableDataState::Loaded(data) => data.rows.get(row_index).map(|row| row.deleted),
            _ => None,
        });
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
                Some("Select a row for row actions; double-click a cell to edit it")
            }
            _ => None,
        };
        v_flex()
            .id("database-table-data")
            .key_context("DatabaseTableData")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                let where_focused = this
                    .where_clause
                    .focus_handle(cx)
                    .contains_focused(window, cx);
                let order_focused = this.order_by.focus_handle(cx).contains_focused(window, cx);
                if where_focused || order_focused {
                    cx.stop_propagation();
                    this.request_navigation(TableNavigation::Reload { page: 0 }, window, cx);
                }
            }))
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
                        Button::new("refresh-database-table-data", "Refresh")
                            .style(ButtonStyle::Outlined)
                            .start_icon(Icon::new(IconName::RefreshTitle))
                            .disabled(self.is_saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.request_navigation(
                                    TableNavigation::Reload { page: this.page },
                                    window,
                                    cx,
                                )
                            })),
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
                        Button::new(
                            "delete-selected-database-table-row",
                            if selected_row_deleted == Some(true) {
                                "Restore Row"
                            } else {
                                "Delete Row"
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(if selected_row_deleted == Some(true) {
                            IconName::RotateCcw
                        } else {
                            IconName::Trash
                        }))
                        .disabled(!can_edit || selected_row_deleted.is_none() || self.is_saving)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_selected_row(cx))),
                    )
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
