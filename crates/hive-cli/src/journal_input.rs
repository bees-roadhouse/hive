//! Synthesize journal entries for CLI/MCP writes when journal-canonical
//! input is active (`HIVE_INPUT_MODE=shadow|enforce`, or `HIVE_JOURNAL_INPUT=1`).

const AIS: &[&str] = &["pia", "apis", "cera", "nate"];

pub fn use_journal_input() -> bool {
    match std::env::var("HIVE_INPUT_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "legacy" | "" => std::env::var("HIVE_JOURNAL_INPUT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        _ => true,
    }
}

pub fn ai_for_owner(owner: &str) -> String {
    if AIS.contains(&owner) {
        owner.to_string()
    } else {
        "nate".to_string()
    }
}

pub fn synthesize_task_add(
    owner: &str,
    project: &str,
    title: &str,
    body: Option<&str>,
    priority: Option<&str>,
    due: Option<&str>,
) -> (String, String, String) {
    let ai = ai_for_owner(owner);
    let mut line = format!("- [ ] {title} @{owner} proj:{project}");
    if let Some(p) = priority {
        line.push_str(&format!(" pri:{p}"));
    }
    if let Some(d) = due {
        line.push_str(&format!(" due:{d}"));
    }
    let mut journal_body = line;
    if let Some(b) = body.filter(|s| !s.trim().is_empty()) {
        journal_body.push_str("\n\n");
        journal_body.push_str(b.trim());
    }
    let journal_title = format!("task: {title}");
    (ai, journal_title, journal_body)
}

pub fn synthesize_note_add(
    author: &str,
    title: Option<&str>,
    body: &str,
    project: Option<&str>,
    tags: Option<&str>,
) -> (String, String, String) {
    let ai = ai_for_owner(author);
    let note_title = title.unwrap_or("note");
    let mut header = format!("#note {note_title}");
    if let Some(p) = project {
        header.push_str(&format!(" project:{p}"));
    }
    if let Some(t) = tags {
        header.push_str(&format!(" tags:{t}"));
    }
    let journal_body = format!("{header}\n\n{}", body.trim());
    let journal_title = format!("note: {note_title}");
    (ai, journal_title, journal_body)
}

pub fn synthesize_task_done(owner: &str, title: &str) -> (String, String, String) {
    let ai = ai_for_owner(owner);
    let journal_body = format!("- [x] {title} @{owner}");
    let journal_title = format!("done: {title}");
    (ai, journal_title, journal_body)
}

pub fn synthesize_task_drop(owner: &str, title: &str) -> (String, String, String) {
    let ai = ai_for_owner(owner);
    let journal_body = format!("- [-] {title} @{owner}");
    let journal_title = format!("drop: {title}");
    (ai, journal_title, journal_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_add_includes_proj_and_markers() {
        let (ai, title, body) = synthesize_task_add(
            "pia",
            "hive",
            "fix traefik",
            None,
            Some("high"),
            Some("2026-06-01"),
        );
        assert_eq!(ai, "pia");
        assert_eq!(title, "task: fix traefik");
        assert!(body.contains("proj:hive"));
        assert!(body.contains("pri:high"));
        assert!(body.contains("due:2026-06-01"));
    }

    #[test]
    fn note_add_uses_note_spawn_block() {
        let (_, title, body) = synthesize_note_add(
            "pia",
            Some("dinner"),
            "reservations at 7",
            Some("home"),
            Some("food"),
        );
        assert_eq!(title, "note: dinner");
        assert!(body.starts_with("#note dinner project:home tags:food"));
    }
}
