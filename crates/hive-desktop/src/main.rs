#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::rc::Rc;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;

    let entries = slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        JournalEntryView {
            title: "hive-desktop scaffold landed".into(),
            ai: "cera".into(),
            entry_date: "2026-05-19".into(),
        },
        JournalEntryView {
            title: "slint over tauri: efficient + native".into(),
            ai: "cera".into(),
            entry_date: "2026-05-19".into(),
        },
    ])));

    app.set_entries(entries);
    app.run()
}
