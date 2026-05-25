use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};

use crate::{ParsedBody, ParsedTask, PersonRef, TagRef};

pub fn parse(body: &str) -> ParsedBody {
    let today = Local::now().date_naive();
    parse_with_today(body, today)
}

pub(crate) fn parse_with_today(body: &str, today: NaiveDate) -> ParsedBody {
    let mut tasks = Vec::new();
    let mut person_refs = Vec::new();
    let mut tag_refs = Vec::new();

    for (line_index, line) in body.split('\n').enumerate() {
        for slug in collect_persons(line) {
            person_refs.push(PersonRef { slug, line_index });
        }
        for tag in collect_tags(line) {
            tag_refs.push(TagRef { tag, line_index });
        }

        if let Some(task) = parse_task_line(line, line_index, today) {
            tasks.push(task);
        }
    }

    ParsedBody {
        tasks,
        person_refs,
        tag_refs,
    }
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

fn collect_persons(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    if match_task_shape(line).is_some() {
        return out;
    }
    for token in tokenize(line) {
        if let Some(rest) = token.strip_prefix('@')
            && is_slug(rest)
        {
            out.push(rest.to_string());
        }
    }
    out
}

fn collect_tags(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    if match_task_shape(line).is_some() {
        return out;
    }
    for token in tokenize(line) {
        if let Some(rest) = token.strip_prefix('#')
            && is_slug(rest)
        {
            out.push(rest.to_string());
        }
    }
    out
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
