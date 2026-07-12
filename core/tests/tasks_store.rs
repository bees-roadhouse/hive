// Store-level test for the fold-safe Reminders slice: tasks_update can SET and
// CHANGE a task's `due` and `project` (its list), and — because the generic
// entity_update maps a JSON null to SQL NULL (fold bind_value) — can CLEAR them
// too. All over the EXISTING `due`/`project` columns: no schema/fold change is
// exercised. Untouched fields survive, and the canonical row re-read from the
// index the fold wrote agrees.

mod common;

use hive_core::store::tasks::TaskCreate;
use hive_shared::{Priority, TaskPatch, TaskStatus};

fn seed() -> TaskCreate {
    TaskCreate {
        title: "Ship the slice".into(),
        body: "reminders + accounts".into(),
        status: TaskStatus::Todo,
        priority: Priority::Normal,
        ..Default::default()
    }
}

#[tokio::test]
async fn tasks_update_sets_changes_and_clears_due_and_project() {
    let store = common::test_store().await;
    let t = store.tasks_create(seed(), "nate").await.unwrap();
    // Fresh task has no list and no due.
    assert_eq!(t.project, None);
    assert_eq!(t.due, None);

    // SET both (Some(Some(_))). `project` auto-ensures a project by name in the
    // create path; the update path just sets the column.
    let updated = store
        .tasks_update(
            &t.id,
            TaskPatch {
                due: Some(Some("2026-07-15".into())),
                project: Some(Some("Launch".into())),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap()
        .expect("task exists");
    assert_eq!(updated.due.as_deref(), Some("2026-07-15"));
    assert_eq!(updated.project.as_deref(), Some("Launch"));
    // Untouched fields survive.
    assert_eq!(updated.title, "Ship the slice");
    assert_eq!(updated.status, TaskStatus::Todo);
    // The canonical row (re-read from the index the fold wrote) agrees.
    let got = store.tasks_get(&t.id).await.unwrap().expect("row");
    assert_eq!(got.due.as_deref(), Some("2026-07-15"));
    assert_eq!(got.project.as_deref(), Some("Launch"));

    // CHANGE both to new values.
    let updated = store
        .tasks_update(
            &t.id,
            TaskPatch {
                due: Some(Some("2026-08-01".into())),
                project: Some(Some("Roadmap".into())),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap()
        .expect("task exists");
    assert_eq!(updated.due.as_deref(), Some("2026-08-01"));
    assert_eq!(updated.project.as_deref(), Some("Roadmap"));
    let got = store.tasks_get(&t.id).await.unwrap().expect("row");
    assert_eq!(got.due.as_deref(), Some("2026-08-01"));
    assert_eq!(got.project.as_deref(), Some("Roadmap"));

    // Leaving them absent keeps them (a status-only patch touches neither).
    let updated = store
        .tasks_update(
            &t.id,
            TaskPatch {
                status: Some(TaskStatus::Doing),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap()
        .expect("task exists");
    assert_eq!(updated.status, TaskStatus::Doing);
    assert_eq!(
        updated.due.as_deref(),
        Some("2026-08-01"),
        "absent kept due"
    );
    assert_eq!(
        updated.project.as_deref(),
        Some("Roadmap"),
        "absent kept project"
    );

    // CLEAR both (Some(None) → JSON null → SQL NULL). This proves the fold's
    // update_row nulls the column rather than skipping it.
    let cleared = store
        .tasks_update(
            &t.id,
            TaskPatch {
                due: Some(None),
                project: Some(None),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap()
        .expect("task exists");
    assert_eq!(cleared.due, None, "explicit null clears due");
    assert_eq!(cleared.project, None, "explicit null clears project");
    let got = store.tasks_get(&t.id).await.unwrap().expect("row");
    assert_eq!(got.due, None, "cleared due persists to the row");
    assert_eq!(got.project, None, "cleared project persists to the row");
    assert_eq!(got.status, TaskStatus::Doing, "clearing left status alone");

    // Updating a missing task is a clean None (no panic, no record).
    assert!(store
        .tasks_update("task_missing", TaskPatch::default(), "nate")
        .await
        .unwrap()
        .is_none());
}
