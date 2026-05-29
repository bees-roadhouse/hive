//! Single-entry detail view at `/journal/:id`.
//!
//! Renders one journal entry as full prose with markdown, scaled a touch
//! larger than the feed for long-form reading. Tag chips link back to
//! `/?tag=foo` so they round-trip into the home filter.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{JournalEntry, fetch_journal_entry};
use crate::markdown::render_markdown;

#[component]
pub fn EntryPage() -> impl IntoView {
    let params = use_params_map();

    let data = Resource::new(
        move || params.read().get("id").unwrap_or_default(),
        |id| async move {
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            fetch_journal_entry(&id).await.map_err(|e| e.to_string())
        },
    );

    view! {
        <p class="entry-back">
            <a href="/">"← back to journal"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(entry) => view! { <EntryDetail entry/> }.into_any(),
                Err(_) => view! {
                    <p class="empty">"this entry isn't here"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn EntryDetail(entry: JournalEntry) -> impl IntoView {
    let when = entry.entry_date.clone().unwrap_or_default();
    let title = entry
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let tags_raw = entry.tags.clone().unwrap_or_default();
    let body_html = render_markdown(&entry.body);

    view! {
        <article class="entry-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{title}</h1>
                <p class="entry-meta">
                    <span class="entry-writer">{entry.ai}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">{when}</span>
                    {render_tag_chips(&tags_raw)}
                </p>
            </header>
            <div class="entry-body" inner_html=body_html></div>
        </article>
    }
}

/// Tag chips that link back to the home feed's `?tag=foo` filter.
fn render_tag_chips(tags: &str) -> AnyView {
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
            {chips.into_iter().map(|t| {
                let href = format!("/?tag={}", urlencode(&t));
                view! { <a class="tag" href=href>"#"{t}</a> }
            }).collect_view()}
        </span>
    }
    .into_any()
}

/// Minimal percent-encoding for the tag query value. Mirrors the one in
/// `api.rs` — kept inline here to avoid widening that module's surface.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
