use crate::parse::{match_task_shape, strip_trailing_block_id};
use crate::{ParseError, TaskPatch};

pub fn update_task(body: &str, block_id: &str, patch: TaskPatch) -> Result<String, ParseError> {
    let line_ending_count = body.matches('\n').count();
    let lines: Vec<&str> = body.split('\n').collect();
    let mut new_lines: Vec<String> = lines.iter().map(|s| (*s).to_string()).collect();

    let mut found = false;
    for (idx, line) in lines.iter().enumerate() {
        if line_block_id(line).as_deref() == Some(block_id) {
            new_lines[idx] = apply_patch_to_line(line, &patch);
            found = true;
            break;
        }
    }

    if !found {
        return Err(ParseError::BlockIdNotFound(block_id.to_string()));
    }

    let joined = new_lines.join("\n");
    // split('\n') + join('\n') is byte-exact reconstruction of the original
    // line structure; this assertion guards future refactors.
    debug_assert_eq!(joined.matches('\n').count(), line_ending_count);
    Ok(joined)
}

pub fn assign_block_ids(body: &str, mut next_id: impl FnMut() -> String) -> String {
    let lines: Vec<&str> = body.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        if match_task_shape(line).is_some() && line_block_id(line).is_none() {
            let id = next_id();
            let trimmed_end = line.trim_end_matches([' ', '\t']);
            let trailing = &line[trimmed_end.len()..];
            out.push(format!("{} ^{}{}", trimmed_end, id, trailing));
        } else {
            out.push(line.to_string());
        }
    }

    out.join("\n")
}

fn line_block_id(line: &str) -> Option<String> {
    match_task_shape(line)?;
    strip_trailing_block_id(line).map(|(_, id)| id)
}

fn apply_patch_to_line(line: &str, patch: &TaskPatch) -> String {
    let shape = match match_task_shape(line) {
        Some(s) => s,
        None => return line.to_string(),
    };

    let indent = shape.indent.to_string();
    let bullet = shape.bullet;
    let mut checked = shape.checked;
    if let Some(c) = patch.checked {
        checked = c;
    }

    let body_with_id = shape.body;
    let (body_no_id, block_id) = match strip_trailing_block_id(body_with_id) {
        Some((stripped, id)) => (stripped, Some(id)),
        None => (body_with_id.to_string(), None),
    };

    let trimmed_end = body_no_id.trim_end_matches([' ', '\t']);
    let trailing_ws = &body_no_id[trimmed_end.len()..];
    let mut content = trimmed_end.to_string();

    if let Some(new_text) = &patch.text {
        content = rewrite_text(&content, new_text);
    }

    if let Some(owner_patch) = &patch.owner {
        content = rewrite_owner(&content, owner_patch.as_deref());
    }

    if let Some(due_patch) = &patch.due {
        let formatted = due_patch.as_ref().map(|d| d.format("%Y-%m-%d").to_string());
        content = rewrite_kv(&content, "due:", formatted.as_deref());
    }

    if let Some(pri_patch) = &patch.priority {
        content = rewrite_kv(&content, "pri:", pri_patch.as_deref());
    }

    let mark = if checked { 'x' } else { ' ' };
    let mut out = format!("{}{} [{}] {}{}", indent, bullet, mark, content, trailing_ws);
    if let Some(id) = block_id {
        out.push(' ');
        out.push('^');
        out.push_str(&id);
    }
    out
}

fn rewrite_text(content: &str, new_text: &str) -> String {
    // Replace prose words; preserve all @owner, #tag, due:, pri: tokens.
    let mut preserved: Vec<&str> = Vec::new();
    for token in content.split_whitespace() {
        if token.starts_with('@')
            || token.starts_with('#')
            || token.starts_with("due:")
            || token.starts_with("pri:")
        {
            preserved.push(token);
        }
    }
    let mut parts: Vec<String> = vec![new_text.to_string()];
    for tok in preserved {
        parts.push(tok.to_string());
    }
    parts.join(" ")
}

fn rewrite_owner(content: &str, new_owner: Option<&str>) -> String {
    // Owner = first @mention. Replace it (or remove it if clearing) and
    // pass any subsequent @mentions through unchanged.
    let tokens: Vec<&str> = content.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len() + 1);
    let mut handled_first = false;
    for token in tokens {
        if token.starts_with('@') && !handled_first {
            handled_first = true;
            if let Some(name) = new_owner {
                out.push(format!("@{}", name));
            }
        } else {
            out.push(token.to_string());
        }
    }
    if !handled_first && let Some(name) = new_owner {
        out.push(format!("@{}", name));
    }
    out.join(" ")
}

fn rewrite_kv(content: &str, prefix: &str, new_value: Option<&str>) -> String {
    let tokens: Vec<&str> = content.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len() + 1);
    let mut replaced = false;
    for token in tokens {
        if token.starts_with(prefix) {
            if !replaced {
                if let Some(v) = new_value {
                    out.push(format!("{}{}", prefix, v));
                }
                replaced = true;
            }
        } else {
            out.push(token.to_string());
        }
    }
    if !replaced && let Some(v) = new_value {
        out.push(format!("{}{}", prefix, v));
    }
    out.join(" ")
}
