use chrono::{Datelike, Duration, Local, NaiveDate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Window, px,
};
use ui::{
    Button, ButtonStyle, Color, IconButton, IconButtonShape, IconName, Label, LabelSize, Popover,
    TintColor, prelude::*,
};
use ui_input::InputField;

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

    pub(crate) fn picker_time(self, value: &str) -> Option<String> {
        match self {
            Self::Date => None,
            Self::Timestamp => {
                Some(time_from_value(value).unwrap_or_else(|| "00:00:00".to_owned()))
            }
        }
    }

    pub(crate) fn value_from_picker(
        self,
        value: &str,
        date: NaiveDate,
        time: Option<&str>,
    ) -> String {
        let date = date.format("%Y-%m-%d").to_string();
        match self {
            Self::Date => date,
            Self::Timestamp => {
                let separator = if value.as_bytes().get(10) == Some(&b'T') {
                    'T'
                } else {
                    ' '
                };
                format!(
                    "{date}{separator}{}",
                    time.map(str::trim)
                        .filter(|time| !time.is_empty())
                        .unwrap_or("00:00:00")
                )
            }
        }
    }
}

pub(crate) fn date_from_value(value: &str) -> Option<NaiveDate> {
    value
        .get(..10)
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}

fn time_from_value(value: &str) -> Option<String> {
    date_from_value(value)?;
    let time = value.get(10..)?.trim_start_matches([' ', 'T']).trim();
    (!time.is_empty()).then(|| time.to_owned())
}

fn time_is_valid(value: &str) -> bool {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() < 5 || bytes[2] != b':' {
        return false;
    }
    let Some(hour) = parse_two_digits(&bytes[..2]) else {
        return false;
    };
    let Some(minute) = parse_two_digits(&bytes[3..5]) else {
        return false;
    };
    if hour > 23 || minute > 59 {
        return false;
    }
    if bytes.len() == 5 {
        return true;
    }

    if matches!(bytes[5], b'+' | b'-' | b'Z') {
        return timezone_is_valid(&value[5..]);
    }
    if bytes[5] != b':' || bytes.len() < 8 {
        return false;
    }
    let Some(second) = parse_two_digits(&bytes[6..8]) else {
        return false;
    };
    if second > 59 {
        return false;
    }
    let suffix = &value[8..];
    suffix.is_empty()
        || timezone_is_valid(suffix)
        || suffix.strip_prefix('.').is_some_and(|fraction| {
            let digit_count = fraction.bytes().take_while(u8::is_ascii_digit).count();
            digit_count > 0
                && (digit_count == fraction.len() || timezone_is_valid(&fraction[digit_count..]))
        })
}

fn timezone_is_valid(value: &str) -> bool {
    if value == "Z" {
        return true;
    }
    let Some(offset) = value.strip_prefix(['+', '-']) else {
        return false;
    };
    let (hour, minute) = match offset.len() {
        2 => (&offset[..2], "00"),
        4 => (&offset[..2], &offset[2..]),
        5 if offset.as_bytes()[2] == b':' => (&offset[..2], &offset[3..]),
        _ => return false,
    };
    let Some(hour) = parse_two_digits(hour.as_bytes()) else {
        return false;
    };
    let Some(minute) = parse_two_digits(minute.as_bytes()) else {
        return false;
    };
    hour <= 23 && minute <= 59
}

fn parse_two_digits(value: &[u8]) -> Option<u8> {
    (value.len() == 2 && value.iter().all(u8::is_ascii_digit))
        .then(|| (value[0] - b'0') * 10 + value[1] - b'0')
}

type SelectValue = Box<dyn Fn(NaiveDate, Option<String>, &mut Window, &mut App)>;

pub(crate) struct DatePicker {
    focus_handle: FocusHandle,
    displayed_year: i32,
    displayed_month: u32,
    selected: NaiveDate,
    kind: TemporalCellKind,
    time_input: Option<Entity<InputField>>,
    validation_error: Option<String>,
    select_value: SelectValue,
}

impl DatePicker {
    pub(crate) fn new(
        selected: NaiveDate,
        kind: TemporalCellKind,
        time: Option<String>,
        select_value: SelectValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let time_input = time.map(|time| {
            cx.new(|cx| {
                let input = InputField::new(window, cx, "HH:MM:SS").label("TIME");
                input.set_text(&time, window, cx);
                input
            })
        });
        Self {
            focus_handle: cx.focus_handle(),
            displayed_year: selected.year(),
            displayed_month: selected.month(),
            selected,
            kind,
            time_input,
            validation_error: None,
            select_value,
        }
    }

    fn move_month(&mut self, delta: i32, cx: &mut Context<Self>) {
        let month_index = self.displayed_year * 12 + self.displayed_month as i32 - 1 + delta;
        self.displayed_year = month_index.div_euclid(12);
        self.displayed_month = month_index.rem_euclid(12) as u32 + 1;
        cx.notify();
    }

    fn select_date(&mut self, date: NaiveDate, cx: &mut Context<Self>) {
        self.selected = date;
        self.displayed_year = date.year();
        self.displayed_month = date.month();
        self.validation_error = None;
        cx.notify();
    }

    fn select_today(&mut self, cx: &mut Context<Self>) {
        self.select_date(Local::now().date_naive(), cx);
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let time = self
            .time_input
            .as_ref()
            .map(|input| input.read(cx).text(cx).trim().to_owned());
        if self.kind == TemporalCellKind::Timestamp && !time.as_deref().is_some_and(time_is_valid) {
            self.validation_error =
                Some("Enter a valid time such as 14:30 or 14:30:45.123+02:00".to_owned());
            cx.notify();
            return;
        }

        (self.select_value)(self.selected, time, window, cx);
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

        Popover::new().child(
            v_flex()
                .key_context("DatabaseDatePicker")
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
                .on_action(
                    cx.listener(|this, _: &menu::Confirm, window, cx| this.apply(window, cx)),
                )
                .w(px(268.))
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
                        .on_click(cx.listener(move |this, _, _, cx| this.select_date(date, cx))),
                    )
                })))
                .when_some(self.time_input.clone(), |this, time_input| {
                    this.child(
                        div()
                            .pt_1()
                            .border_t_1()
                            .border_color(cx.theme().colors().border),
                    )
                    .child(time_input)
                })
                .when_some(self.validation_error.clone(), |this, error| {
                    this.child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                })
                .child(
                    h_flex()
                        .justify_between()
                        .child(
                            Button::new("database-date-picker-today", "Today")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| this.select_today(cx))),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("database-date-picker-cancel", "Cancel")
                                        .style(ButtonStyle::Subtle)
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                                )
                                .child(
                                    Button::new("database-date-picker-apply", "Apply")
                                        .style(ButtonStyle::Filled)
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.apply(window, cx)
                                        })),
                                ),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use gpui::TestAppContext;

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
    fn date_time_picker_round_trips_time_and_timezone() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let original = "2024-01-02 13:14:15.123+02:00";
        let time = TemporalCellKind::Timestamp.picker_time(original);
        assert_eq!(time.as_deref(), Some("13:14:15.123+02:00"));
        assert_eq!(
            TemporalCellKind::Timestamp.value_from_picker(original, date, time.as_deref()),
            "2026-07-18 13:14:15.123+02:00"
        );
        assert_eq!(
            TemporalCellKind::Timestamp.value_from_picker("", date, None),
            "2026-07-18 00:00:00"
        );
        assert_eq!(
            TemporalCellKind::Date.value_from_picker("2024-01-02", date, None),
            "2026-07-18"
        );
    }

    #[test]
    fn validates_common_jdbc_time_formats() {
        for valid in [
            "00:00",
            "23:59:59",
            "13:14:15.123",
            "13:14:15.123+02:00",
            "13:14:15Z",
            "13:14+02:00",
            "13:14:15-0530",
        ] {
            assert!(time_is_valid(valid), "expected {valid:?} to be valid");
        }
        for invalid in [
            "",
            "3:14",
            "24:00",
            "13:60",
            "13:14:60",
            "13:14:xx",
            "13:14Zgarbage",
            "13:14:15+25:00",
        ] {
            assert!(
                !time_is_valid(invalid),
                "expected {invalid:?} to be invalid"
            );
        }
    }

    #[gpui::test]
    fn selecting_a_day_waits_for_explicit_apply(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let applied = Rc::new(Cell::new(false));
        let applied_for_picker = applied.clone();
        let initial = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let selected = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let picker = cx.update(|window, cx| {
            cx.new(|cx| {
                DatePicker::new(
                    initial,
                    TemporalCellKind::Date,
                    None,
                    Box::new(move |_, _, _, _| applied_for_picker.set(true)),
                    window,
                    cx,
                )
            })
        });

        picker.update_in(cx, |picker, _, cx| picker.select_date(selected, cx));

        assert_eq!(picker.read_with(cx, |picker, _| picker.selected), selected);
        assert!(
            !applied.get(),
            "selecting a day must not apply or close the picker"
        );
    }
}
