//! RawMessage -> NormalizedMessage: body selection (plaintext-first,
//! html2text for HTML-only messages), snippet, and timestamp normalization.
//! Raw HTML never leaves this module.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::client::{RawAddr, RawMessage};
use crate::{Address, BodySource, NormalizedMessage, SyncError};

/// Render width for html2text. Mail bodies are stored, not displayed, so the
/// only goal is "no artificial wrapping" — pick a width no real line exceeds.
const HTML_RENDER_WIDTH: usize = 5000;
const SNIPPET_CHARS: usize = 140;

/// Normalize an RFC3339/ISO-8601 timestamp to the exact
/// `%Y-%m-%dT%H:%M:%S%.3fZ` shape hive's `now_iso` produces. Every ordering
/// comparison in hive is lexicographic TEXT, and `...:00Z` sorts differently
/// from `...:00.000Z` — so every timestamp crosses this boundary before it
/// leaves the crate.
pub fn normalize_iso_millis(value: &str) -> Result<String, SyncError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|e| SyncError::Protocol(format!("unparseable timestamp {value:?}: {e}")))?;
    Ok(parsed
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub(crate) fn iso_from_epoch(epoch: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(epoch, 0)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn address(a: RawAddr) -> Address {
    Address {
        email: a.email,
        name: a.name,
    }
}

fn snip(text: &str) -> String {
    let mut out = String::with_capacity(SNIPPET_CHARS + 1);
    for (i, ch) in text.chars().enumerate() {
        if i >= SNIPPET_CHARS {
            out.push('…');
            break;
        }
        out.push(if ch == '\n' { ' ' } else { ch });
    }
    out.trim().to_string()
}

pub(crate) fn normalize(raw: RawMessage) -> NormalizedMessage {
    let mut parse_error: Option<String> = None;

    let (body_text, body_source) = if !raw.text_parts.is_empty() {
        (raw.text_parts.join("\n\n"), BodySource::Plain)
    } else if !raw.html_parts.is_empty() {
        let mut rendered = Vec::with_capacity(raw.html_parts.len());
        for html in &raw.html_parts {
            match html2text::from_read(html.as_bytes(), HTML_RENDER_WIDTH) {
                Ok(text) => rendered.push(text),
                Err(e) => {
                    parse_error = Some(format!("html2text: {e}"));
                }
            }
        }
        (rendered.join("\n\n"), BodySource::Html2text)
    } else {
        (String::new(), BodySource::Plain)
    };

    let mut body_text = body_text.trim_end().to_string();
    if raw.truncated {
        body_text.push_str("\n\n[truncated]");
    }

    // received_at is the authoritative sort key. A message the server hands
    // us without one is defective; store it at epoch with a parse error
    // rather than wedging the page (per-message failure isolation).
    let received_at = raw
        .received_at_epoch
        .or(raw.sent_at_epoch)
        .and_then(iso_from_epoch)
        .unwrap_or_else(|| {
            parse_error = Some("missing receivedAt".into());
            "1970-01-01T00:00:00.000Z".to_string()
        });

    let mut from = raw.from.into_iter();
    let first_from = from.next();

    let snippet = raw
        .preview
        .as_deref()
        .map(snip)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| snip(&body_text));

    NormalizedMessage {
        jmap_id: raw.jmap_id,
        thread_id: raw.thread_id,
        message_id_hdr: raw.message_id,
        in_reply_to: raw.in_reply_to,
        references: raw.references,
        from_addr: first_from
            .as_ref()
            .map(|a| a.email.clone())
            .unwrap_or_default(),
        from_name: first_from.and_then(|a| a.name),
        to: raw.to.into_iter().map(address).collect(),
        cc: raw.cc.into_iter().map(address).collect(),
        reply_to: raw.reply_to.into_iter().map(address).collect(),
        subject: raw.subject.unwrap_or_default(),
        sent_at: raw.sent_at_epoch.and_then(iso_from_epoch),
        received_at,
        mailbox_ids: raw.mailbox_ids,
        keywords: raw.keywords,
        body_text,
        body_source,
        snippet,
        size: raw.size,
        attachments: raw.attachments,
        parse_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_second_precision_to_millis() {
        assert_eq!(
            normalize_iso_millis("2026-07-09T12:00:00Z").unwrap(),
            "2026-07-09T12:00:00.000Z"
        );
    }

    #[test]
    fn normalizes_offsets_to_utc() {
        assert_eq!(
            normalize_iso_millis("2026-07-09T12:00:00-04:00").unwrap(),
            "2026-07-09T16:00:00.000Z"
        );
    }

    #[test]
    fn keeps_millis_and_truncates_finer_precision() {
        assert_eq!(
            normalize_iso_millis("2026-07-09T12:00:00.123456Z").unwrap(),
            "2026-07-09T12:00:00.123Z"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(normalize_iso_millis("not a time").is_err());
    }

    #[test]
    fn lexicographic_ordering_holds_across_shapes() {
        // The trap this exists for: "...:00Z" vs "...:00.000Z" sort
        // differently as strings. After normalization both are millis-shaped.
        let a = normalize_iso_millis("2026-07-09T12:00:00Z").unwrap();
        let b = normalize_iso_millis("2026-07-09T12:00:01Z").unwrap();
        assert!(a < b);
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn snip_flattens_newlines_and_caps() {
        let s = snip(&format!("line one\nline two {}", "x".repeat(300)));
        assert!(s.starts_with("line one line two"));
        assert!(s.chars().count() <= SNIPPET_CHARS + 1);
        assert!(s.ends_with('…'));
    }
}
