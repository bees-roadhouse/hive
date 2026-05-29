//! Slug derivation + collision resolution.
//!
//! One rule across every sluggable table (people, events, tasks, notes,
//! journal_entries) so the mention resolver (`@slug` / `[[type:slug]]`) has
//! a single regex: `^[a-z][a-z0-9_-]*$`.
//!
//! The derivation matches the SQL backfill in migration 0012: lowercase,
//! non-alnum → '-', collapse runs, trim '-' from ends, prefix `<fallback>-`
//! if empty or starts with a digit. Collisions get resolved with a numeric
//! suffix loop (`-2`, `-3`, ...) by the caller, hitting the UNIQUE
//! constraint each iteration.

/// Derive a base slug from a free-form title. Non-alnum collapses to `-`,
/// runs collapse (the `+` in the regex), and `-` is trimmed from both ends.
/// Returns `fallback` if the result would be empty or start with a digit.
///
/// `fallback` should already be a valid slug shape (e.g. `"task"`,
/// `"event"`, `"note"`, `"entry"`); we don't sanitize it.
pub fn derive_slug(title: &str, fallback: &str) -> String {
    let lower = title.to_ascii_lowercase();
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
        if trimmed.is_empty() {
            fallback.to_string()
        } else {
            format!("{fallback}-{trimmed}")
        }
    } else {
        trimmed
    }
}

/// Append `-2`, `-3`, ... to `base` until `is_free` reports the candidate is
/// not taken. The caller supplies the existence check ... usually a SELECT 1
/// against the target table's UNIQUE index. Bounded to 1000 attempts so a
/// pathological table can't spin forever.
pub async fn resolve_collision<F, Fut>(base: &str, mut is_free: F) -> String
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut candidate = base.to_string();
    let mut counter: u32 = 1;
    while !is_free(candidate.clone()).await {
        counter += 1;
        candidate = format!("{base}-{counter}");
        if counter > 1000 {
            // Give up cleanly ... the caller will hit the DB UNIQUE constraint
            // and surface a real conflict rather than wedge in a tight loop.
            break;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_dashes() {
        assert_eq!(
            derive_slug("Fix Traefik ReadTimeout", "task"),
            "fix-traefik-readtimeout"
        );
    }

    #[test]
    fn collapses_runs_and_trims() {
        assert_eq!(derive_slug("  Hello!!  World  ", "x"), "hello-world");
    }

    #[test]
    fn empty_falls_back() {
        assert_eq!(derive_slug("", "task"), "task");
        assert_eq!(derive_slug("!!!", "task"), "task");
    }

    #[test]
    fn leading_digit_gets_prefix() {
        assert_eq!(derive_slug("2026 plan", "event"), "event-2026-plan");
    }

    #[test]
    fn unicode_becomes_dashes() {
        // We're ASCII-only on purpose — slugs are human-typeable in URL bars.
        assert_eq!(derive_slug("café", "x"), "caf");
    }
}
