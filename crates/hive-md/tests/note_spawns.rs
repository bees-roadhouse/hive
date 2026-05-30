use hive_md::parse;

#[test]
fn parses_triple_bracket_note_block() {
    let body = r"Some prose.

[[[note dinner plans project:home tags:food]]]
Reservations at 7.
[[[/note]]]

- [ ] unrelated task
";
    let parsed = parse(body);
    assert_eq!(parsed.note_spawns.len(), 1);
    let note = &parsed.note_spawns[0];
    assert_eq!(note.title, "dinner plans");
    assert_eq!(note.project.as_deref(), Some("home"));
    assert_eq!(note.tags.as_deref(), Some("food"));
    assert_eq!(note.body, "Reservations at 7.");
}

#[test]
fn note_block_masks_inner_checkboxes_from_task_parse() {
    let body = r"[[[note embedded task]]]
- [ ] should not become a journal task
[[[/note]]]

- [ ] real task
";
    let parsed = parse(body);
    assert_eq!(parsed.tasks.len(), 1);
    assert_eq!(parsed.tasks[0].text, "real task");
}

#[test]
fn hash_tag_stays_folksonomy_not_note_spawn() {
    let body = "Captured #food ideas for later.";
    let parsed = parse(body);
    assert!(parsed.note_spawns.is_empty());
    assert_eq!(parsed.tag_refs.len(), 1);
    assert_eq!(parsed.tag_refs[0].tag, "food");
}

#[test]
fn unclosed_note_block_is_ignored() {
    let body = "[[[note orphan]]]\nno closer";
    let parsed = parse(body);
    assert!(parsed.note_spawns.is_empty());
}
