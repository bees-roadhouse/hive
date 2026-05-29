//! Shared journal-entry rendering. Used by the home feed and the search
//! results page so both render in the same article visual (full markdown body
//! or, for search, the FTS5 snippet promoted into the body slot).

use leptos::prelude::*;
use pulldown_cmark::{Parser, html};

use crate::api::JournalEntry;

#[component]
pub fn EntryArticle(entry: JournalEntry) -> impl IntoView {
    let when = entry.entry_date.clone().unwrap_or_default();
    let title = entry
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let tags_raw = entry.tags.clone().unwrap_or_default();
    let body_html = render_markdown(&entry.body);

    view! {
        <li class="entry">
            <header class="entry-header">
                <h2 class="entry-title">{title}</h2>
                <p class="entry-meta">
                    <span class="entry-writer">{entry.ai}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">{when}</span>
                    {render_tag_chips(&tags_raw)}
                </p>
            </header>
            <div class="entry-body" inner_html=body_html></div>
        </li>
    }
}

/// Render markdown source to HTML. Trusted-content path ... every entry is
/// authored by us (CLI, UI compose, or an AI we run), so raw HTML passes
/// through. Add a sanitizer here if untrusted writers ever land.
pub fn render_markdown(src: &str) -> String {
    let parser = Parser::new(src);
    let mut out = String::with_capacity(src.len());
    html::push_html(&mut out, parser);
    out
}

/// Split a comma-separated tags field into rendered `#tag` chips. Returns
/// nothing if the field is empty (skips the leading separator too).
pub fn render_tag_chips(tags: &str) -> AnyView {
    let chips: Vec<String> = tags
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if chips.is_empty() {
        return ().into_any();
    }
    view! {
        <span class="entry-sep">"·"</span>
        <span class="entry-tags">
            {chips.into_iter().map(|t| view! { <span class="tag">"#"{t}</span> }).collect_view()}
        </span>
    }
    .into_any()
}
