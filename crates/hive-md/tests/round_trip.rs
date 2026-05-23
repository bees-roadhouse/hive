use hive_md::{assign_block_ids, parse, update_task, ParseError, TaskPatch};

const SAMPLE: &str = "\
# Notes for today

Some prose with @pia and #household sprinkled in.

- [ ] write the parser ^task1
- [x] ship the API @cera due:2026-05-22 ^task2
- [ ] follow up with @apis #dtc pri:high
";

#[test]
fn parses_three_tasks_with_mixed_state() {
    let parsed = parse(SAMPLE);
    assert_eq!(parsed.tasks.len(), 3);

    let t1 = &parsed.tasks[0];
    assert_eq!(t1.block_id.as_deref(), Some("task1"));
    assert!(!t1.checked);
    assert_eq!(t1.text, "write the parser");
    assert!(t1.owner.is_none());

    let t2 = &parsed.tasks[1];
    assert_eq!(t2.block_id.as_deref(), Some("task2"));
    assert!(t2.checked);
    assert_eq!(t2.owner.as_deref(), Some("cera"));
    assert_eq!(t2.raw_due.as_deref(), Some("2026-05-22"));
    assert_eq!(
        t2.due,
        Some(chrono::NaiveDate::from_ymd_opt(2026, 5, 22).unwrap())
    );

    let t3 = &parsed.tasks[2];
    assert!(t3.block_id.is_none());
    assert_eq!(t3.owner.as_deref(), Some("apis"));
    assert_eq!(t3.tags, vec!["dtc"]);
    assert_eq!(t3.priority.as_deref(), Some("high"));

    // person_refs/tag_refs come from prose lines only, not task lines.
    assert!(parsed
        .person_refs
        .iter()
        .any(|p| p.slug == "pia" && p.line_index == 2));
    assert!(parsed
        .tag_refs
        .iter()
        .any(|t| t.tag == "household" && t.line_index == 2));
    // Tokens inside task lines are surfaced via ParsedTask.persons/tags,
    // not person_refs/tag_refs.
    assert!(!parsed.person_refs.iter().any(|p| p.slug == "cera"));
    assert!(!parsed.tag_refs.iter().any(|t| t.tag == "dtc"));
}

#[test]
fn update_task_toggles_checkbox() {
    let patched = update_task(
        SAMPLE,
        "task1",
        TaskPatch {
            checked: Some(true),
            ..Default::default()
        },
    )
    .unwrap();

    let reparsed = parse(&patched);
    let t1 = reparsed
        .tasks
        .iter()
        .find(|t| t.block_id.as_deref() == Some("task1"))
        .expect("task1 still present");
    assert!(t1.checked);
    assert_eq!(t1.text, "write the parser");

    // Other lines unchanged.
    let other_lines: Vec<&str> = patched.split('\n').collect();
    assert!(other_lines.contains(&"- [x] ship the API @cera due:2026-05-22 ^task2"));
    assert!(other_lines.contains(&"- [ ] follow up with @apis #dtc pri:high"));
}

#[test]
fn update_task_changes_text_preserves_id_and_state() {
    let patched = update_task(
        SAMPLE,
        "task2",
        TaskPatch {
            text: Some("ship the new API".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let reparsed = parse(&patched);
    let t2 = reparsed
        .tasks
        .iter()
        .find(|t| t.block_id.as_deref() == Some("task2"))
        .expect("task2 still present");
    assert!(t2.checked);
    assert_eq!(t2.text, "ship the new API");
    assert_eq!(t2.owner.as_deref(), Some("cera"));
    assert_eq!(t2.raw_due.as_deref(), Some("2026-05-22"));
}

#[test]
fn assign_block_ids_fills_in_missing_ids_only() {
    let body = "\
prose line

- [ ] no id yet
some prose
- [x] also missing id
- [ ] already has id ^task9
";
    let mut counter = 100u32;
    let result = assign_block_ids(body, || {
        let id = format!("task{}", counter);
        counter += 1;
        id
    });

    let lines: Vec<&str> = result.split('\n').collect();
    assert_eq!(lines[0], "prose line");
    assert_eq!(lines[1], "");
    assert_eq!(lines[2], "- [ ] no id yet ^task100");
    assert_eq!(lines[3], "some prose");
    assert_eq!(lines[4], "- [x] also missing id ^task101");
    assert_eq!(lines[5], "- [ ] already has id ^task9");
}

#[test]
fn update_task_unknown_block_id_errors() {
    let err = update_task(SAMPLE, "task999", TaskPatch::default()).unwrap_err();
    matches!(err, ParseError::BlockIdNotFound(_));
    assert_eq!(err.to_string(), "block id task999 not found in body");
}
