#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod db;
mod parser;

use std::rc::Rc;

use chrono::{Datelike, Local, Timelike};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::parser::{BodySegment, CheckState};

slint::include_modules!();

fn day_greeting() -> String {
    let prefix = match Local::now().hour() {
        5..=11 => "good morning",
        12..=17 => "good afternoon",
        18..=21 => "good evening",
        _ => "still up",
    };
    format!("{}, nate.", prefix)
}

fn day_subtitle() -> String {
    let now = Local::now();
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "monday",
        chrono::Weekday::Tue => "tuesday",
        chrono::Weekday::Wed => "wednesday",
        chrono::Weekday::Thu => "thursday",
        chrono::Weekday::Fri => "friday",
        chrono::Weekday::Sat => "saturday",
        chrono::Weekday::Sun => "sunday",
    };
    let day = now.day();
    let ordinal = match day {
        1 | 21 | 31 => "st",
        2 | 22 => "nd",
        3 | 23 => "rd",
        _ => "th",
    };
    let month = match now.month() {
        1 => "january", 2 => "february", 3 => "march", 4 => "april",
        5 => "may", 6 => "june", 7 => "july", 8 => "august",
        9 => "september", 10 => "october", 11 => "november", 12 => "december",
        _ => unreachable!(),
    };
    format!("{}, the {}{} of {}", weekday, day, ordinal, month)
}

fn to_task_rows(fetched: Vec<db::TaskFetched>) -> Vec<TaskRow> {
    fetched
        .into_iter()
        .map(|t| TaskRow {
            id: SharedString::from(format!("tasks:{}", t.id)),
            title: SharedString::from(t.title),
            due_label: SharedString::from(t.due_label),
            overdue: t.overdue,
        })
        .collect()
}

fn to_wire_rows(fetched: Vec<db::WireFetched>) -> Vec<WireRow> {
    fetched
        .into_iter()
        .map(|w| WireRow {
            title: SharedString::from(w.title),
            source: SharedString::from(w.source),
        })
        .collect()
}

fn to_task_fulls(fetched: Vec<db::TaskFullFetched>) -> Vec<TaskFull> {
    fetched
        .into_iter()
        .map(|t| TaskFull {
            id: t.id as i32,
            title: SharedString::from(t.title),
            body: SharedString::from(t.body),
            project: SharedString::from(t.project),
            owner: SharedString::from(t.owner),
            priority: SharedString::from(t.priority),
            status: SharedString::from(t.status),
            due_label: SharedString::from(t.due_label),
            block_reason: SharedString::from(t.block_reason),
        })
        .collect()
}

fn check_state_to_int(s: &CheckState) -> i32 {
    match s {
        CheckState::Open => 0,
        CheckState::Done => 1,
        CheckState::Dropped => 2,
        CheckState::InProgress => 3,
    }
}

fn to_body_segments(body: &str) -> Vec<BodySegmentSlint> {
    parser::parse_body(body)
        .into_iter()
        .map(|seg| match seg {
            BodySegment::Text(t) => BodySegmentSlint {
                is_checkbox: false,
                check_state: 0,
                text: SharedString::from(t),
                task_id: 0,
            },
            BodySegment::Checkbox {
                state,
                text,
                task_id,
            } => BodySegmentSlint {
                is_checkbox: true,
                check_state: check_state_to_int(&state),
                text: SharedString::from(text),
                task_id: task_id.unwrap_or(0) as i32,
            },
        })
        .collect()
}

fn to_journal_entries(fetched: Vec<db::JournalFetched>) -> Vec<JournalEntry> {
    fetched
        .into_iter()
        .map(|j| {
            let segments = to_body_segments(&j.body);
            JournalEntry {
                id: j.id,
                ai: SharedString::from(j.ai),
                when_label: SharedString::from(j.when_label),
                title: SharedString::from(j.title),
                body: SharedString::from(j.body),
                body_segments: ModelRc::from(Rc::new(VecModel::from(segments))),
                tags_label: SharedString::from(j.tags_label),
                related_label: SharedString::from(j.related_label),
            }
        })
        .collect()
}

fn main() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;

    app.set_greeting(day_greeting().into());
    app.set_day_subtitle(day_subtitle().into());

    let tasks = to_task_rows(db::fetch_today_tasks());
    let wire = to_wire_rows(db::fetch_wire(5));
    let journal = to_journal_entries(db::fetch_journal(20));
    let task_fulls = to_task_fulls(db::fetch_all_tasks());

    app.set_today_tasks(ModelRc::from(Rc::new(VecModel::from(tasks))));
    app.set_today_wire(ModelRc::from(Rc::new(VecModel::from(wire))));
    app.set_journal_entries(ModelRc::from(Rc::new(VecModel::from(journal))));
    app.set_task_entries(ModelRc::from(Rc::new(VecModel::from(task_fulls))));

    // checkbox click .. POST mark-done, refetch journal on 2xx.
    let weak = app.as_weak();
    app.on_checkbox_clicked(move |task_id| {
        if task_id == 0 {
            eprintln!("hive-desktop: checkbox clicked with no binding");
            return;
        }
        if db::mark_task_done(task_id as i64) {
            if let Some(app) = weak.upgrade() {
                let refreshed = to_journal_entries(db::fetch_journal(20));
                app.set_journal_entries(ModelRc::from(Rc::new(VecModel::from(refreshed))));
            }
        }
    });

    app.run()
}
