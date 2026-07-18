use std::{cell::Cell, rc::Rc};

use chrono::{Datelike, Duration, Local, NaiveDate};
use gpui::{App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Render, Window, px};
use ui::{
    Button, ButtonStyle, Color, IconButton, IconButtonShape, IconName, Label, LabelSize, Popover,
    TintColor, prelude::*,
};

const JDBC_DATE: i32 = 91;
const JDBC_TIMESTAMP: i32 = 93;
const JDBC_TIMESTAMP_WITH_TIMEZONE: i32 = 2014;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TemporalCellKind {
    Date,
    Timestamp,
}

impl TemporalCellKind {
    pub(crate) fn from_metadata(jdbc_type: i32, type_name: &str) -> Option<Self> {
        match jdbc_type {
            JDBC_DATE => Some(Self::Date),
            JDBC_TIMESTAMP | JDBC_TIMESTAMP_WITH_TIMEZONE => Some(Self::Timestamp),
            _ => {
                let normalized = type_name.to_ascii_uppercase();
                if normalized.contains("TIMESTAMP") || normalized.contains("DATETIME") {
                    Some(Self::Timestamp)
                } else if normalized == "DATE"
                    || normalized.starts_with("DATE(")
                    || normalized.starts_with("DATE ")
                {
                    Some(Self::Date)
                } else {
                    None
                }
            }
        }
    }

    pub(crate) fn value_with_date(self, value: &str, date: NaiveDate) -> String {
        let date = date.format("%Y-%m-%d").to_string();
        match self {
            Self::Date => date,
            Self::Timestamp => value
                .get(10..)
                .filter(|_| date_from_value(value).is_some())
                .map_or_else(
                    || format!("{date} 00:00:00"),
                    |suffix| format!("{date}{suffix}"),
                ),
        }
    }
}

pub(crate) fn date_from_value(value: &str) -> Option<NaiveDate> {
    value
        .get(..10)
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}

type SelectDate = Box<dyn Fn(NaiveDate, &mut Window, &mut App)>;

pub(crate) struct DatePicker {
    focus_handle: FocusHandle,
    displayed_year: i32,
    displayed_month: u32,
    selected: NaiveDate,
    open: Rc<Cell<bool>>,
    select_date: SelectDate,
}

impl DatePicker {
    pub(crate) fn new(
        selected: NaiveDate,
        open: Rc<Cell<bool>>,
        select_date: SelectDate,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            displayed_year: selected.year(),
            displayed_month: selected.month(),
            selected,
            open,
            select_date,
        }
    }

    fn move_month(&mut self, delta: i32, cx: &mut Context<Self>) {
        let month_index = self.displayed_year * 12 + self.displayed_month as i32 - 1 + delta;
        self.displayed_year = month_index.div_euclid(12);
        self.displayed_month = month_index.rem_euclid(12) as u32 + 1;
        cx.notify();
    }

    fn select(&mut self, date: NaiveDate, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = date;
        (self.select_date)(date, window, cx);
        cx.emit(DismissEvent);
    }

    fn month_label(&self) -> String {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        format!(
            "{} {}",
            MONTHS[self.displayed_month as usize - 1],
            self.displayed_year
        )
    }
}

impl Drop for DatePicker {
    fn drop(&mut self) {
        self.open.set(false);
    }
}

impl Focusable for DatePicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for DatePicker {}

impl Render for DatePicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let first_day = NaiveDate::from_ymd_opt(self.displayed_year, self.displayed_month, 1)
            .expect("the displayed calendar month is valid");
        let grid_start =
            first_day - Duration::days(first_day.weekday().num_days_from_monday().into());
        let today = Local::now().date_naive();

        Popover::new().child(
            v_flex()
                .key_context("DatabaseDatePicker")
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
                .w(px(252.))
                .gap_1()
                .px_2()
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .child(
                            IconButton::new(
                                "database-date-picker-previous-month",
                                IconName::ChevronLeft,
                            )
                            .shape(IconButtonShape::Square)
                            .aria_label("Previous month")
                            .on_click(cx.listener(|this, _, _, cx| this.move_month(-1, cx))),
                        )
                        .child(Label::new(self.month_label()).size(LabelSize::Small))
                        .child(
                            IconButton::new(
                                "database-date-picker-next-month",
                                IconName::ChevronRight,
                            )
                            .shape(IconButtonShape::Square)
                            .aria_label("Next month")
                            .on_click(cx.listener(|this, _, _, cx| this.move_month(1, cx))),
                        ),
                )
                .child(div().grid().grid_cols(7).children(
                    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map(|day| {
                        div()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Label::new(day).size(LabelSize::XSmall).color(Color::Muted))
                    }),
                ))
                .child(div().grid().grid_cols(7).children((0..42).map(|offset| {
                    let date = grid_start + Duration::days(offset);
                    let in_displayed_month = date.month() == self.displayed_month;
                    div().size(px(32.)).child(
                        Button::new(
                            ("database-date-picker-day", offset as u32),
                            date.day().to_string(),
                        )
                        .full_width()
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .when(!in_displayed_month, |button| button.color(Color::Muted))
                        .toggle_state(date == self.selected)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.select(date, window, cx)),
                        ),
                    )
                })))
                .child(
                    h_flex().justify_end().child(
                        Button::new("database-date-picker-today", "Today")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select(today, window, cx)
                            })),
                    ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_standard_and_fallback_temporal_types() {
        assert_eq!(
            TemporalCellKind::from_metadata(JDBC_DATE, "anything"),
            Some(TemporalCellKind::Date)
        );
        assert_eq!(
            TemporalCellKind::from_metadata(JDBC_TIMESTAMP, "anything"),
            Some(TemporalCellKind::Timestamp)
        );
        assert_eq!(
            TemporalCellKind::from_metadata(1111, "DateTime64(3)"),
            Some(TemporalCellKind::Timestamp)
        );
        assert_eq!(TemporalCellKind::from_metadata(12, "varchar"), None);
    }

    #[test]
    fn replacing_timestamp_date_preserves_time_and_timezone() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        assert_eq!(
            TemporalCellKind::Timestamp.value_with_date("2024-01-02 13:14:15.123+02:00", date),
            "2026-07-18 13:14:15.123+02:00"
        );
        assert_eq!(
            TemporalCellKind::Timestamp.value_with_date("", date),
            "2026-07-18 00:00:00"
        );
        assert_eq!(
            TemporalCellKind::Date.value_with_date("2024-01-02", date),
            "2026-07-18"
        );
    }
}
