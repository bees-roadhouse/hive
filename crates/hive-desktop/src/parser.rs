//! Obsidian-style checkbox parser for journal entry bodies.
//!
//! Splits a body into segments .. plain text runs interleaved with checkbox
//! lines. Optional `^tasks-N` anchor binds a checkbox to a specific task id.
//!
//! Grammar per line:
//!   `<indent>- [<c>] <text>( ^tasks-<N>)?`
//! where `<c>` is one of ` `, `x`, `-`, `/`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckState {
    Open,
    Done,
    Dropped,
    InProgress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodySegment {
    Text(String),
    Checkbox {
        state: CheckState,
        text: String,
        task_id: Option<i64>,
    },
}

/// Parse a body into segments. Consecutive non-checkbox lines coalesce into
/// one Text segment, preserving newlines between them.
pub fn parse_body(body: &str) -> Vec<BodySegment> {
    let mut out: Vec<BodySegment> = Vec::new();
    let mut text_buf = String::new();

    for line in body.split('\n') {
        if let Some(cb) = parse_checkbox_line(line) {
            if !text_buf.is_empty() {
                // strip trailing newline accumulated by the loop
                if text_buf.ends_with('\n') {
                    text_buf.pop();
                }
                out.push(BodySegment::Text(std::mem::take(&mut text_buf)));
            }
            out.push(cb);
        } else {
            text_buf.push_str(line);
            text_buf.push('\n');
        }
    }
    if !text_buf.is_empty() {
        if text_buf.ends_with('\n') {
            text_buf.pop();
        }
        out.push(BodySegment::Text(text_buf));
    }
    out
}

/// Match `^(\s*)- \[(.)\] (.+?)( \^tasks-(\d+))?$` by hand .. no regex dep.
fn parse_checkbox_line(line: &str) -> Option<BodySegment> {
    // skip leading whitespace
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("- [")?;
    // next char is the state marker, then `] `
    let mut chars = rest.chars();
    let marker = chars.next()?;
    let after_marker: String = chars.collect();
    let body_part = after_marker.strip_prefix("] ")?;
    if body_part.is_empty() {
        return None;
    }

    let state = match marker {
        ' ' => CheckState::Open,
        'x' | 'X' => CheckState::Done,
        '-' => CheckState::Dropped,
        '/' => CheckState::InProgress,
        _ => return None,
    };

    // optional ` ^tasks-<N>` suffix
    let (text, task_id) = if let Some(idx) = body_part.rfind(" ^tasks-") {
        let head = &body_part[..idx];
        let tail = &body_part[idx + " ^tasks-".len()..];
        if let Ok(n) = tail.parse::<i64>() {
            (head.to_string(), Some(n))
        } else {
            (body_part.to_string(), None)
        }
    } else {
        (body_part.to_string(), None)
    };

    Some(BodySegment::Checkbox {
        state,
        text,
        task_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_checkbox() {
        let segs = parse_body("- [ ] do the thing");
        assert_eq!(
            segs,
            vec![BodySegment::Checkbox {
                state: CheckState::Open,
                text: "do the thing".to_string(),
                task_id: None,
            }]
        );
    }

    #[test]
    fn done_checkbox_with_anchor() {
        let segs = parse_body("- [x] shipped it ^tasks-42");
        assert_eq!(
            segs,
            vec![BodySegment::Checkbox {
                state: CheckState::Done,
                text: "shipped it".to_string(),
                task_id: Some(42),
            }]
        );
    }

    #[test]
    fn all_four_states() {
        let body = "- [ ] open\n- [x] done\n- [-] dropped\n- [/] wip";
        let segs = parse_body(body);
        let states: Vec<_> = segs
            .into_iter()
            .map(|s| match s {
                BodySegment::Checkbox { state, .. } => state,
                _ => panic!("expected checkbox"),
            })
            .collect();
        assert_eq!(
            states,
            vec![
                CheckState::Open,
                CheckState::Done,
                CheckState::Dropped,
                CheckState::InProgress,
            ]
        );
    }

    #[test]
    fn text_and_checkbox_interleaved() {
        let body = "intro line\nmore prose\n- [ ] task one\nafterword\n- [x] task two ^tasks-9";
        let segs = parse_body(body);
        assert_eq!(segs.len(), 4);
        match &segs[0] {
            BodySegment::Text(t) => assert_eq!(t, "intro line\nmore prose"),
            _ => panic!(),
        }
        match &segs[1] {
            BodySegment::Checkbox { text, task_id, .. } => {
                assert_eq!(text, "task one");
                assert_eq!(*task_id, None);
            }
            _ => panic!(),
        }
        match &segs[2] {
            BodySegment::Text(t) => assert_eq!(t, "afterword"),
            _ => panic!(),
        }
        match &segs[3] {
            BodySegment::Checkbox {
                text,
                task_id,
                state,
            } => {
                assert_eq!(text, "task two");
                assert_eq!(*task_id, Some(9));
                assert_eq!(*state, CheckState::Done);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn plain_text_only() {
        let segs = parse_body("just words\nand more words");
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            BodySegment::Text(t) => assert_eq!(t, "just words\nand more words"),
            _ => panic!(),
        }
    }

    #[test]
    fn indented_checkbox() {
        let segs = parse_body("  - [ ] nested item");
        assert_eq!(
            segs,
            vec![BodySegment::Checkbox {
                state: CheckState::Open,
                text: "nested item".to_string(),
                task_id: None,
            }]
        );
    }

    #[test]
    fn non_matching_dash_stays_text() {
        let segs = parse_body("- not a checkbox\n-[ ] also not");
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            BodySegment::Text(t) => assert_eq!(t, "- not a checkbox\n-[ ] also not"),
            _ => panic!(),
        }
    }
}
