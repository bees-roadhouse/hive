//! Shared journal-entry rendering. Used by the home feed and the search
//! results page so both render in the same article visual: full markdown
//! body for the feed, FTS5 snippet (with `<mark>` highlights) for search.
//!
//! Each entry's title links to its detail page at `/journal/:id`. Tag chips
//! are click-through filters that preserve the current writer filter on the
//! feed.

use leptos::prelude::*;

use crate::api::JournalEntry;
use crate::markdown::render_markdown;

#[component]
pub fn EntryArticle(entry: JournalEntry, current_writer: String) -> impl IntoView {
    let when = entry.entry_date.clone().unwrap_or_default();
    let title = entry
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let tags_raw = entry.tags.clone().unwrap_or_default();
    let body_html = render_markdown(&entry.body);
    let entry_href = format!("/journal/{}", entry.id);

    view! {
        <li class="entry">
            <header class="entry-header">
                <h2 class="entry-title">
                    <a class="entry-title-link" href=entry_href rel="external">{title}</a>
                </h2>
                <p class="entry-meta">
                    <span class="entry-writer">{entry.ai}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">{when}</span>
                    {render_tag_chips(&tags_raw, &current_writer)}
                </p>
            </header>
            <div class="entry-body" inner_html=body_html></div>
        </li>
    }
}

/// Split a comma-separated tags field into rendered `#tag` chips. Each chip
/// is a click-through link that filters the feed by that tag while preserving
/// the current writer. Returns nothing if the field is empty.
pub fn render_tag_chips(tags: &str, current_writer: &str) -> AnyView {
    let chips: Vec<String> = tags
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if chips.is_empty() {
        return ().into_any();
    }
    let writer = current_writer.to_string();
    view! {
        <span class="entry-sep">"·"</span>
        <span class="entry-tags">
            {chips.into_iter().map(|t| {
                let href = build_query("/", &writer, &t);
                view! { <a class="tag" href=href rel="external">"#"{t}</a> }
            }).collect_view()}
        </span>
    }
    .into_any()
}

/// Build a `/path?writer=...&tag=...` URL, skipping empty params and
/// percent-encoding values. Used by the in-entry tag chips to round-trip
/// back into the home feed with the right filter active.
pub fn build_query(path: &str, writer: &str, tag: &str) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(2);
    if !writer.is_empty() {
        parts.push(format!("writer={}", urlencode(writer)));
    }
    if !tag.is_empty() {
        parts.push(format!("tag={}", urlencode(tag)));
    }
    if parts.is_empty() {
        path.to_string()
    } else {
        format!("{}?{}", path, parts.join("&"))
    }
}

/// Minimal percent-encoding for query values. Matches the encoder in
/// `api.rs` so this module stays self-contained.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
