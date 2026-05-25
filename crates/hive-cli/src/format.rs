//! Output formatting helpers for hive-cli.
//!
//! Mirrors python's `_rows_to_dicts`, `_print_json`, and the inline
//! column-alignment patterns in each cmd_*_list helper. The width of each
//! column is `max(len(header), max(len(cell) over rows))` to match python's
//! `max(len("header"), max(len(row[k]) for r in rows))` calculation.

use std::io::{self, Write};

use serde::Serialize;

/// Render a timestamp string from the API for human-facing output. hive-api
/// sends TIMESTAMPTZ fields as ISO-8601 (e.g. `2026-05-25T13:43:00Z`); collapse
/// to `YYYY-MM-DD HH:MM:SS` to match python's `show`/`list` column shape. An
/// absent/null field renders empty (mirrors python's `row["closed_at"] or ""`).
pub fn fmt_ts_opt(ts: &Option<String>) -> String {
    let Some(raw) = ts.as_deref().filter(|s| !s.is_empty()) else {
        return String::new();
    };
    // Best-effort: ISO-8601 -> "date time" by swapping the 'T' and dropping the
    // zone/fraction. Pass through unchanged if it doesn't look like ISO.
    let no_zone = raw
        .split_once('.')
        .map(|(head, _)| head)
        .unwrap_or(raw)
        .trim_end_matches('Z');
    match no_zone.split_once('T') {
        Some((date, time)) => format!("{date} {time}"),
        None => raw.to_string(),
    }
}

/// One column in a tabular print.
pub struct Column<'a, T> {
    pub header: &'a str,
    /// Compute the cell text for a row. Right-padded to width.
    pub get: Box<dyn Fn(&T) -> String + 'a>,
}

/// A trailing (unsized, flush-to-end) column: `(label, cell-getter)`. Rendered
/// after the last sized column with no width, mirroring how python prints
/// `... title` / `... tags` at end-of-line.
pub type Trailing<'a, T> = (&'a str, Box<dyn Fn(&T) -> String + 'a>);

impl<'a, T> Column<'a, T> {
    pub fn new<F>(header: &'a str, get: F) -> Self
    where
        F: Fn(&T) -> String + 'a,
    {
        Column {
            header,
            get: Box::new(get),
        }
    }
}

/// Print a table of `rows` with `cols`, mirroring python's:
///
/// ```python
/// header = f"{'id':<{id_w}}  {'project':<{proj_w}}  ..."
/// print(header)
/// print("-" * len(header))
/// for r in rows:
///     print(...)
/// ```
///
/// `trailing_col_label` is rendered after the last sized column with no
/// width; mirrors how python prints `... title` and `... tags` flush at
/// end-of-line.
pub fn print_table<T>(cols: &[Column<'_, T>], rows: &[T], trailing: Option<Trailing<'_, T>>) {
    if rows.is_empty() {
        return;
    }
    // Compute widths.
    let mut widths: Vec<usize> = cols
        .iter()
        .map(|c| {
            let mut w = c.header.len();
            for r in rows {
                let cell = (c.get)(r);
                if cell.len() > w {
                    w = cell.len();
                }
            }
            w
        })
        .collect();
    // Edge case: if a column ends up zero (all empty + header empty), the
    // python format string would still emit a 0-padded slot; preserve that.
    for w in widths.iter_mut() {
        if *w == 0 {
            *w = 1;
        }
    }

    // Header
    let mut header = String::new();
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            header.push_str("  ");
        }
        header.push_str(&pad_right(c.header, widths[i]));
    }
    if let Some((label, _)) = &trailing {
        header.push_str("  ");
        header.push_str(label);
    }
    println!("{header}");
    println!("{}", "-".repeat(header.len()));

    for r in rows {
        let mut line = String::new();
        for (i, c) in cols.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(&pad_right(&(c.get)(r), widths[i]));
        }
        if let Some((_, f)) = &trailing {
            line.push_str("  ");
            line.push_str(&f(r));
        }
        println!("{line}");
    }
}

pub fn pad_right(s: &str, w: usize) -> String {
    if s.len() >= w {
        s.to_string()
    } else {
        let mut out = String::with_capacity(w);
        out.push_str(s);
        for _ in 0..(w - s.len()) {
            out.push(' ');
        }
        out
    }
}

/// Truncate to `n` chars with a trailing ellipsis. Mirrors python `truncate`.
pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let cut = n.saturating_sub(1);
        format!("{}{}", &s[..cut], "...")
    }
}

/// Print a value as JSON (indent=2, ensure_ascii=False equivalent ... serde
/// produces UTF-8 by default). Trailing newline matches python's
/// `sys.stdout.write("\n")` after `json.dump`.
pub fn print_json<T: Serialize>(value: &T) -> Result<(), serde_json::Error> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let s = serde_json::to_string_pretty(value)?;
    handle.write_all(s.as_bytes()).ok();
    handle.write_all(b"\n").ok();
    Ok(())
}
