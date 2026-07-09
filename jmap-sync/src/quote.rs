//! Quoted-reply stripping for embedding input. Without this, every message in
//! a thread embeds near-identically and recall returns one conversation N
//! times. Runs before EMBEDDING only — stored `body_text` is never modified.
//!
//! Heuristics, applied to the tail of the message first (quotes accumulate at
//! the bottom), then signatures/footers. The safety valve: if stripping
//! removes >90% of the non-whitespace content, the original text wins (never
//! embed an empty husk).
//!
//! No mature crate does this; the corpus in `tests/corpus/` is the contract.
//! Add a case by adding a `NN_name.in.txt` / `NN_name.out.txt` pair.

use std::borrow::Cow;

/// Strip quoted replies, forwarded blocks, signatures, and newsletter footers
/// from `text`, returning the original when stripping would remove nearly
/// everything.
pub fn strip_quoted(text: &str) -> Cow<'_, str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![true; lines.len()];

    mark_quote_blocks(&lines, &mut keep);
    mark_attribution_lines(&lines, &mut keep);
    mark_header_blocks(&lines, &mut keep);
    mark_signature(&lines, &mut keep);
    mark_newsletter_footer(&lines, &mut keep);

    if keep.iter().all(|k| *k) {
        return Cow::Borrowed(text);
    }

    let stripped: String = lines
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(l, _)| *l)
        .collect::<Vec<_>>()
        .join("\n");
    let stripped = collapse_blank_runs(stripped.trim());

    // Safety valve: refuse to strip the message down to (nearly) nothing.
    let original_weight = non_ws_len(text);
    if original_weight > 0 && non_ws_len(&stripped) * 10 < original_weight {
        return Cow::Borrowed(text);
    }
    Cow::Owned(stripped)
}

fn non_ws_len(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `> `-quoted blocks at any nesting depth, tolerant of blank lines
/// interleaved inside a quoted run.
fn mark_quote_blocks(lines: &[&str], keep: &mut [bool]) {
    let is_quote = |l: &str| l.trim_start().starts_with('>');
    let mut i = 0;
    while i < lines.len() {
        if !is_quote(lines[i]) {
            i += 1;
            continue;
        }
        // Extend the block through interleaved blank lines while the next
        // non-blank line is still quoted.
        let start = i;
        let mut end = i;
        let mut j = i;
        while j < lines.len() {
            if is_quote(lines[j]) {
                end = j;
                j += 1;
            } else if lines[j].trim().is_empty() {
                j += 1;
            } else {
                break;
            }
        }
        for k in keep.iter_mut().take(end + 1).skip(start) {
            *k = false;
        }
        i = end + 1;
    }
}

/// Attribution lines that introduce a quote: "On <...> wrote:" (possibly
/// wrapped over two lines), Gmail/Apple Mail forms. Only dropped when a
/// quoted block or nothing follows.
fn mark_attribution_lines(lines: &[&str], keep: &mut [bool]) {
    for i in 0..lines.len() {
        let t = lines[i].trim();
        let starts_like_attribution = (t.starts_with("On ")
            || t.starts_with("Am ")
            || t.starts_with("Le ")
            || t.starts_with("El "))
            && t.len() < 200;
        if !starts_like_attribution {
            continue;
        }
        // The "wrote:" may sit on this line or the next (client wrapping).
        let joined_next = if i + 1 < lines.len() {
            format!("{} {}", t, lines[i + 1].trim())
        } else {
            t.to_string()
        };
        let on_this = t.ends_with("wrote:") || t.ends_with("schrieb:") || t.ends_with("écrit :");
        let on_wrapped = !on_this
            && (joined_next.ends_with("wrote:")
                || joined_next.ends_with("schrieb:")
                || joined_next.ends_with("écrit :"))
            && joined_next.len() < 250;
        if !(on_this || on_wrapped) {
            continue;
        }
        let attribution_end = if on_wrapped { i + 1 } else { i };
        // Require the attribution to actually introduce quoted/removed
        // content (or end the message) — an "On Monday we wrote:" mid-prose
        // followed by kept text stays.
        let mut j = attribution_end + 1;
        while j < lines.len() && lines[j].trim().is_empty() {
            j += 1;
        }
        let followed_by_removed = j >= lines.len() || !keep[j];
        if followed_by_removed {
            for k in keep.iter_mut().take(attribution_end + 1).skip(i) {
                *k = false;
            }
        }
    }
}

/// Outlook-style top-post header blocks and forwarded-message banners: cut
/// from the marker to the end of the message.
fn mark_header_blocks(lines: &[&str], keep: &mut [bool]) {
    const HEADER_KEYS: [&str; 6] = ["From:", "Sent:", "To:", "Subject:", "Date:", "Cc:"];
    for i in 0..lines.len() {
        let t = lines[i].trim();
        let original_marker = t.starts_with("-----Original Message-----")
            || t.starts_with("-----Ursprüngliche Nachricht-----");
        let forwarded_banner = t.starts_with("---------- Forwarded message")
            || t.starts_with("Begin forwarded message");
        // The bare Outlook separator: a long underscore rule directly above a
        // From:/Sent:/To: run.
        let underscore_rule = t.len() >= 20 && t.chars().all(|c| c == '_');
        // A consecutive run of 3+ header-looking lines also counts even
        // without a rule above it.
        let header_run = HEADER_KEYS.iter().any(|k| t.starts_with(k))
            && (i + 2 < lines.len()
                && HEADER_KEYS
                    .iter()
                    .any(|k| lines[i + 1].trim().starts_with(k))
                && HEADER_KEYS
                    .iter()
                    .any(|k| lines[i + 2].trim().starts_with(k)));
        let underscore_marks = underscore_rule
            && i + 1 < lines.len()
            && HEADER_KEYS
                .iter()
                .any(|k| lines[i + 1].trim().starts_with(k));
        if original_marker || forwarded_banner || header_run || underscore_marks {
            for k in keep.iter_mut().skip(i) {
                *k = false;
            }
            return;
        }
    }
}

/// `-- ` signature marker (RFC 3676) and common mobile closers: drop from the
/// LAST marker to the end, but only when what follows is signature-sized
/// (≤ 15 lines) — a legitimate `--` early in a long message survives.
fn mark_signature(lines: &[&str], keep: &mut [bool]) {
    const MAX_SIGNATURE_LINES: usize = 15;
    let marker = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, raw)| {
            let t = raw.trim_end();
            let sig_marker = t == "--" || raw.starts_with("-- ");
            let mobile_closer = {
                let tt = t.trim();
                tt == "Sent from my iPhone"
                    || tt == "Sent from my iPad"
                    || tt == "Sent from my Android"
                    || tt.starts_with("Sent from Mail for Windows")
                    || tt.starts_with("Get Outlook for ")
            };
            sig_marker || mobile_closer
        })
        .map(|(i, _)| i);
    if let Some(i) = marker {
        if lines.len() - i <= MAX_SIGNATURE_LINES {
            for k in keep.iter_mut().skip(i) {
                *k = false;
            }
        }
    }
}

/// Newsletter footers: a trailing block (last ~15 lines) containing
/// unsubscribe / preference-management boilerplate. Cut from the first
/// boilerplate line to the end.
fn mark_newsletter_footer(lines: &[&str], keep: &mut [bool]) {
    const MARKERS: [&str; 6] = [
        "unsubscribe",
        "manage preferences",
        "manage your preferences",
        "view in browser",
        "view this email in your browser",
        "update your email preferences",
    ];
    let window_start = lines.len().saturating_sub(15);
    for (i, line) in lines.iter().enumerate().skip(window_start) {
        let lower = line.to_lowercase();
        if MARKERS.iter().any(|m| lower.contains(m)) {
            for k in keep.iter_mut().skip(i) {
                *k = false;
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_returns_borrowed() {
        let text = "Just a normal message.\n\nWith two paragraphs.";
        assert!(matches!(strip_quoted(text), Cow::Borrowed(_)));
    }

    #[test]
    fn safety_valve_keeps_quote_only_messages() {
        let text = "> the entire message\n> is one big quote\n> with nothing new";
        assert_eq!(strip_quoted(text), text);
    }

    #[test]
    fn strips_trailing_quote_and_attribution() {
        let text = "Sounds good, see you then.\n\nOn Tue, Jul 7, 2026 at 9:12 AM Maggie <maggie@example.test> wrote:\n> are we still on for thursday?\n> let me know";
        assert_eq!(strip_quoted(text), "Sounds good, see you then.");
    }
}
