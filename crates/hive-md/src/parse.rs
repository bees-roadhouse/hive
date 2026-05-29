use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::{EntityMention, MentionKind, ParsedBody, ParsedTask, PersonRef, TagRef, TypedKind};

pub fn parse(body: &str) -> ParsedBody {
    let today = Local::now().date_naive();
    parse_with_today(body, today)
}

pub(crate) fn parse_with_today(body: &str, today: NaiveDate) -> ParsedBody {
    let mut tasks = Vec::new();
    let mut person_refs = Vec::new();
    let mut tag_refs = Vec::new();
    let mut entity_mentions = Vec::new();

    // Identify byte ranges that are inside code spans or fenced/indented code
    // blocks. People paste shell commands with `@` and `[[` in them ... if we
    // scan inside those, we'd hallucinate mentions. Code regions are inviolate.
    let code_ranges = code_byte_ranges(body);

    // Line-index lookup: for each byte offset in `body`, what line is it on?
    // Cheap to compute up-front; let us classify any byte offset.
    let line_starts = compute_line_starts(body);

    for (line_index, line) in body.split('\n').enumerate() {
        // Task-line parsing is line-based and intentionally not gated by the
        // code-range mask: a `- [ ]` line inside a fenced block isn't really a
        // task in markdown semantics, but the existing tests + journal canvas
        // grammar treats every `- [ ]` line in the body as a task. Keep that
        // behavior; the universal mention pipeline is the only thing that
        // strictly respects code regions.
        if let Some(task) = parse_task_line(line, line_index, today) {
            tasks.push(task);
        }
    }

    // Persons / tags / mentions: scan the whole body byte-stream, skip code
    // regions, attribute each find to its line.
    scan_outside_code(
        body,
        &code_ranges,
        &line_starts,
        |line_index, line, offset_in_line| {
            // `line` here is the substring of `body` that lies on `line_index` and
            // OUTSIDE any code region. It can be a partial line if a code span
            // sits in the middle. `offset_in_line` is the start of this fragment
            // relative to the start of the line ... not used for the current
            // collectors but kept for symmetry / future use.
            let _ = offset_in_line;

            // Skip person/tag collection on lines that look like task shapes:
            // the existing contract surfaces those via ParsedTask.persons/tags,
            // not the prose-level person_refs / tag_refs.
            let line_is_task = match_task_shape_global(body, line_index, &line_starts).is_some();

            if !line_is_task {
                for slug in collect_persons_in(line) {
                    if !person_refs
                        .iter()
                        .any(|p: &PersonRef| p.slug == slug && p.line_index == line_index)
                    {
                        person_refs.push(PersonRef {
                            slug: slug.clone(),
                            line_index,
                        });
                    }
                }
                for tag in collect_tags_in(line) {
                    if !tag_refs
                        .iter()
                        .any(|t: &TagRef| t.tag == tag && t.line_index == line_index)
                    {
                        tag_refs.push(TagRef { tag, line_index });
                    }
                }
            }

            // Entity mentions DO surface from task lines and prose alike. A `@pia`
            // on a task line is still a reference to pia ... the links projection
            // should record it.
            collect_entity_mentions_in(line, line_index, &mut entity_mentions);
        },
    );

    ParsedBody {
        tasks,
        person_refs,
        tag_refs,
        entity_mentions,
    }
}

/// Byte ranges inside `body` that fall in a code span or a fenced/indented
/// code block. Sorted, non-overlapping, half-open `[start, end)`.
fn code_byte_ranges(body: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let opts = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(body, opts).into_offset_iter();
    let mut fenced_start: Option<usize> = None;

    for (event, range) in parser {
        match event {
            // Inline `code`.
            Event::Code(_) => {
                ranges.push((range.start, range.end));
            }
            // Fenced ``` or indented code block: the inner Text events carry
            // the actual content, but it's simpler to mask the whole block
            // including the fences via the Start/End offsets.
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_)))
            | Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => {
                fenced_start = Some(range.start);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = fenced_start.take() {
                    ranges.push((start, range.end));
                }
            }
            _ => {}
        }
    }

    ranges.sort_by_key(|r| r.0);
    // Merge any touching/overlapping ranges (pulldown-cmark shouldn't emit
    // overlaps, but be defensive).
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if let Some(last) = merged.last_mut()
            && r.0 <= last.1
        {
            last.1 = last.1.max(r.1);
            continue;
        }
        merged.push(r);
    }
    merged
}

fn compute_line_starts(body: &str) -> Vec<usize> {
    let mut out = vec![0usize];
    for (i, b) in body.bytes().enumerate() {
        if b == b'\n' {
            out.push(i + 1);
        }
    }
    out
}

fn line_of_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

/// Iterate over every byte range of `body` that is OUTSIDE the code mask,
/// split into per-line fragments. For each fragment, invoke `f(line_index,
/// fragment_text, offset_in_line)`.
fn scan_outside_code(
    body: &str,
    code_ranges: &[(usize, usize)],
    line_starts: &[usize],
    mut f: impl FnMut(usize, &str, usize),
) {
    // Walk the inverse of `code_ranges` to get the "prose" spans.
    let mut cursor = 0usize;
    let len = body.len();
    let mut prose_spans: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in code_ranges {
        if cursor < s {
            prose_spans.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if cursor < len {
        prose_spans.push((cursor, len));
    }

    for (s, e) in prose_spans {
        // Slice safety: code byte ranges come from pulldown-cmark which
        // operates on `&str` ... boundaries are on char boundaries. Same for
        // line_starts (split on '\n' which is single-byte).
        let mut pos = s;
        while pos < e {
            let line_idx = line_of_offset(line_starts, pos);
            let line_start = line_starts[line_idx];
            let line_end = line_starts.get(line_idx + 1).copied().unwrap_or(len);
            // Strip the trailing '\n' from this line's slice.
            let line_end_no_nl = if line_end > line_start && body.as_bytes()[line_end - 1] == b'\n'
            {
                line_end - 1
            } else {
                line_end
            };
            let frag_end = e.min(line_end_no_nl);
            if pos < frag_end {
                let fragment = &body[pos..frag_end];
                let offset_in_line = pos - line_start;
                f(line_idx, fragment, offset_in_line);
            }
            // Advance past this line. If we hit the line_end (including its
            // newline) and still have room in the prose span, jump to the
            // next line.
            pos = line_end.min(e);
            if pos == line_end_no_nl && pos < line_end && pos < e {
                pos = line_end;
            }
        }
    }
}

/// Re-implement the task-shape probe against a line identified by its index in
/// the byte-indexed `line_starts` table. Used to decide whether prose-level
/// person/tag collection should skip a line.
fn match_task_shape_global(body: &str, line_index: usize, line_starts: &[usize]) -> Option<()> {
    let start = *line_starts.get(line_index)?;
    let end = line_starts
        .get(line_index + 1)
        .copied()
        .unwrap_or(body.len());
    let line = &body[start..end];
    let line = line.strip_suffix('\n').unwrap_or(line);
    match_task_shape(line).map(|_| ())
}

fn collect_persons_in(fragment: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (start, slug) in find_at_slugs(fragment) {
        // `@@slug` is escaping: the second `@` makes the first literal.
        if start > 0 && fragment.as_bytes()[start - 1] == b'@' {
            continue;
        }
        out.push(slug);
    }
    out
}

fn collect_tags_in(fragment: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in fragment.split_whitespace() {
        let token = token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        if let Some(rest) = token.strip_prefix('#')
            && is_slug(rest)
        {
            out.push(rest.to_string());
        }
    }
    out
}

fn collect_entity_mentions_in(fragment: &str, line_index: usize, out: &mut Vec<EntityMention>) {
    // @slug mentions (people).
    for (start, slug) in find_at_slugs(fragment) {
        if start > 0 && fragment.as_bytes()[start - 1] == b'@' {
            continue; // `@@slug` escape
        }
        out.push(EntityMention {
            kind: MentionKind::Person,
            raw: format!("@{slug}"),
            slug,
            line_index,
        });
    }

    // [[...]] wikilinks: typed `[[task:abc]]` or fuzzy `[[abc]]`.
    let bytes = fragment.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find matching `]]`.
            if let Some(close) = find_close_wikilink(fragment, i + 2) {
                let inner = &fragment[i + 2..close];
                let raw = &fragment[i..close + 2];
                if let Some(mention) = parse_wikilink_inner(inner, raw, line_index) {
                    out.push(mention);
                }
                i = close + 2;
                continue;
            }
        }
        i += 1;
    }
}

fn find_close_wikilink(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b']' {
            return Some(i);
        }
        // Nested `[[` aborts ... not a valid wikilink.
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            return None;
        }
        i += 1;
    }
    None
}

fn parse_wikilink_inner(inner: &str, raw: &str, line_index: usize) -> Option<EntityMention> {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((kind_str, body_str)) = trimmed.split_once(':') {
        let kind_str = kind_str.trim();
        let body_str = body_str.trim();
        let typed = match kind_str {
            "task" => TypedKind::Task,
            "note" => TypedKind::Note,
            "event" => TypedKind::Event,
            "journal" => TypedKind::Journal,
            _ => return None,
        };
        if body_str.is_empty() {
            return None;
        }
        // Typed mentions accept either a literal slug or a title; the
        // resolver normalizes the latter via the same derive_slug rule the
        // sluggable tables use on insert.
        let slug = if is_slug(body_str) {
            body_str.to_string()
        } else {
            slugify_title(body_str)?
        };
        Some(EntityMention {
            kind: MentionKind::Typed(typed),
            raw: raw.to_string(),
            slug,
            line_index,
        })
    } else {
        // Fuzzy wikilink: accept any printable inner text (no newlines, no
        // nested `[[`, both already enforced by `find_close_wikilink`). If
        // the input is already a slug, keep it byte-exact; otherwise treat
        // it as a title and slugify with the shared rule.
        let slug = if is_slug(trimmed) {
            trimmed.to_string()
        } else {
            slugify_title(trimmed)?
        };
        Some(EntityMention {
            kind: MentionKind::Fuzzy,
            raw: raw.to_string(),
            slug,
            line_index,
        })
    }
}

/// Lowercase, non-alnum → '-', collapse runs, trim '-' from ends. Returns
/// None if the result would be empty or start with a digit (the slug
/// constraint at the schema level). Matches `hive_db::slug::derive_slug`
/// but kept local so `hive-md` stays dependency-free of `hive-db`.
fn slugify_title(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_dash = false;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() || trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        None
    } else {
        Some(trimmed)
    }
}

/// Find every `@slug` in `fragment` and return (byte-offset-of-`@`, slug).
/// Slug is the longest run of `[a-z0-9_-]` after the `@`, starting with a
/// lowercase letter. Trailing punctuation (period, comma, etc.) is stripped.
fn find_at_slugs(fragment: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let bytes = fragment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            // Slug must start with a-z.
            let slug_start = i + 1;
            if slug_start < bytes.len() && bytes[slug_start].is_ascii_lowercase() {
                let mut end = slug_start + 1;
                while end < bytes.len() && is_slug_byte(bytes[end]) {
                    end += 1;
                }
                let slug = &fragment[slug_start..end];
                // Strip a single trailing hyphen/underscore if it's punctuation
                // ish ... but the slug regex `[a-z0-9_-]*` allows them. We DO
                // want to strip explicit punctuation that isn't part of the
                // slug regex: that's already handled because is_slug_byte
                // returns false for `.`, `,`, etc.
                if is_slug(slug) {
                    out.push((i, slug.to_string()));
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_slug_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
}

pub(crate) struct TaskLineShape<'a> {
    pub indent: &'a str,
    pub bullet: char,
    pub checked: bool,
    pub body: &'a str,
}

pub(crate) fn match_task_shape(line: &str) -> Option<TaskLineShape<'_>> {
    let indent_end = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    let indent = &line[..indent_end];
    let rest = &line[indent_end..];

    let mut chars = rest.chars();
    let bullet = chars.next()?;
    if bullet != '-' && bullet != '*' && bullet != '+' {
        return None;
    }
    if chars.next() != Some(' ') {
        return None;
    }
    if chars.next() != Some('[') {
        return None;
    }
    let mark = chars.next()?;
    let checked = match mark {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    if chars.next() != Some(']') {
        return None;
    }
    if chars.next() != Some(' ') {
        return None;
    }

    let consumed = indent.len() + 6;
    Some(TaskLineShape {
        indent,
        bullet,
        checked,
        body: &line[consumed..],
    })
}

fn parse_task_line(line: &str, line_index: usize, today: NaiveDate) -> Option<ParsedTask> {
    let shape = match_task_shape(line)?;

    let mut text = shape.body.to_string();
    let mut block_id = None;
    let mut owner = None;
    let mut due = None;
    let mut raw_due = None;
    let mut priority = None;
    let mut tags = Vec::new();
    let mut persons = Vec::new();

    if let Some((stripped, id)) = strip_trailing_block_id(&text) {
        block_id = Some(id);
        text = stripped;
    }

    for token in tokenize(&text) {
        if let Some(rest) = token.strip_prefix('@') {
            if is_slug(rest) {
                if owner.is_none() {
                    owner = Some(rest.to_string());
                }
                persons.push(rest.to_string());
            }
        } else if let Some(rest) = token.strip_prefix('#') {
            if is_slug(rest) {
                tags.push(rest.to_string());
            }
        } else if let Some(rest) = token.strip_prefix("due:") {
            if raw_due.is_none() {
                raw_due = Some(rest.to_string());
                due = resolve_due(rest, today);
            }
        } else if let Some(rest) = token.strip_prefix("pri:")
            && priority.is_none()
            && matches!(rest, "high" | "medium" | "low")
        {
            priority = Some(rest.to_string());
        }
    }

    let text = strip_marker_tokens(&text);

    Some(ParsedTask {
        block_id,
        line_index,
        text,
        checked: shape.checked,
        owner,
        due,
        raw_due,
        priority,
        tags,
        persons,
    })
}

pub(crate) fn tokenize(s: &str) -> impl Iterator<Item = &str> {
    s.split_whitespace()
}

fn strip_marker_tokens(s: &str) -> String {
    let kept: Vec<&str> = s
        .split_whitespace()
        .filter(|tok| {
            !(tok.starts_with('@')
                || tok.starts_with('#')
                || tok.starts_with("due:")
                || tok.starts_with("pri:"))
        })
        .collect();
    kept.join(" ")
}

pub(crate) fn is_slug(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub(crate) fn strip_trailing_block_id(text: &str) -> Option<(String, String)> {
    let trimmed_end = text.trim_end_matches([' ', '\t']);
    let trailing = &text[trimmed_end.len()..];

    let last_space = trimmed_end.rfind([' ', '\t'])?;
    let token = &trimmed_end[last_space + 1..];
    let rest = token.strip_prefix("^task")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let id = token[1..].to_string();
    let stripped = format!("{}{}", &trimmed_end[..last_space], trailing);
    Some((stripped, id))
}

fn resolve_due(raw: &str, today: NaiveDate) -> Option<NaiveDate> {
    if let Ok(d) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Some(d);
    }
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "today" => return Some(today),
        "tomorrow" => return Some(today + Duration::days(1)),
        _ => {}
    }
    let (target_name, next) = if let Some(rest) = lower.strip_prefix("next-") {
        (rest, true)
    } else {
        (lower.as_str(), false)
    };
    let weekday = parse_weekday(target_name)?;
    Some(advance_to_weekday(today, weekday, next))
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn advance_to_weekday(today: NaiveDate, target: Weekday, force_next_week: bool) -> NaiveDate {
    let current = today.weekday().num_days_from_monday() as i64;
    let want = target.num_days_from_monday() as i64;
    let mut delta = want - current;
    if force_next_week {
        if delta <= 0 {
            delta += 7;
        }
        delta += 7;
    } else if delta < 0 {
        delta += 7;
    }
    today + Duration::days(delta)
}
