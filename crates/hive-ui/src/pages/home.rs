//! Journal feed — the home view and centerpiece of hive-ui.
//!
//! Single-column chronological stream of entries from every writer (AIs and
//! humans alike), rendered as full prose with markdown. Writer-chip filter
//! across the top, free-text tag filter. No card chrome between entries —
//! structure emerges from the prose itself.
//!
//! Filters are URL-query-driven (`?writer=<>&tag=<>`) so they work with
//! JavaScript disabled. hive-ui ships SSR-only — there's no WASM bundle, no
//! hydration ... `on:click` handlers and signal updates are dead in the
//! browser. Chips are anchor links; the tag filter is a GET form. Every
//! navigation is a full server round-trip rendering the filtered HTML.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;
use pulldown_cmark::{Parser, html};

use crate::api::{JournalEntry, fetch_journal_filtered};

/// Writers we surface as one-click chips. The "all" chip is implicit (no
/// `writer` param). Order is editorial — Pia first because she writes the most.
const WRITERS: &[&str] = &["pia", "apis", "nate", "maggie", "cera"];

/// Page-size for the feed fetch. Generous on purpose — the feed is the whole
/// view and we'd rather scroll than paginate at this scale.
const FEED_LIMIT: i64 = 100;

#[component]
pub fn HomePage() -> impl IntoView {
    // Read the URL query params at SSR time. `use_query_map` returns a Memo,
    // but on the server it's evaluated once per render — we snapshot the
    // current values into owned Strings and pass them down.
    let query = use_query_map();
    let writer = query
        .with_untracked(|q| q.get("writer"))
        .unwrap_or_default();
    let tag = query.with_untracked(|q| q.get("tag")).unwrap_or_default();

    let writer_for_fetch = writer.clone();
    let tag_for_fetch = tag.clone();
    let data = Resource::new(
        move || (writer_for_fetch.clone(), tag_for_fetch.clone()),
        |(writer, tag)| async move {
            fetch_journal_filtered(&writer, &tag, FEED_LIMIT)
                .await
                .map_err(|e| e.to_string())
        },
    );

    let any_filter_active = !writer.is_empty() || !tag.is_empty();
    let writer_for_chips = writer.clone();
    let tag_for_chips = tag.clone();
    let writer_for_form = writer.clone();
    let tag_for_form = tag.clone();
    let writer_for_entries = writer.clone();

    view! {
        <section class="feed-controls">
            <div class="writer-chips">
                <WriterChip
                    label="all".to_string()
                    value=String::new()
                    current_writer=writer_for_chips.clone()
                    current_tag=tag_for_chips.clone()
                />
                {WRITERS.iter().map(|w| {
                    let label = w.to_string();
                    let value = w.to_string();
                    let current_writer = writer_for_chips.clone();
                    let current_tag = tag_for_chips.clone();
                    view! {
                        <WriterChip label value current_writer current_tag/>
                    }
                }).collect_view()}
            </div>
            <form class="tag-form" method="get" action="/">
                <input type="hidden" name="writer" value=writer_for_form/>
                <input
                    class="tag-input"
                    type="text"
                    name="tag"
                    placeholder="filter by tag"
                    value=tag_for_form
                />
            </form>
            {any_filter_active.then(|| view! {
                <a class="clear-filters" href="/">"× clear filters"</a>
            })}
        </section>

        <section class="feed">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || {
                    let current_writer = writer_for_entries.clone();
                    data.get().map(move |result| match result {
                        Ok(entries) => view! {
                            <Feed entries current_writer=current_writer.clone()/>
                        }.into_any(),
                        Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                    })
                }}
            </Suspense>
        </section>
    }
}

/// Writer-filter chip. Renders as an anchor pointing at `/?writer=<value>&tag=<current_tag>`
/// so the click does a full GET round-trip; no JS required. The "all" chip
/// drops the `writer` param entirely (passes `value=""`).
#[component]
fn WriterChip(
    label: String,
    value: String,
    current_writer: String,
    current_tag: String,
) -> impl IntoView {
    let active = current_writer == value;
    let href = build_query("/", &value, &current_tag);
    view! {
        <a class="chip" class:active=active href=href>{label}</a>
    }
}

#[component]
fn Feed(entries: Vec<JournalEntry>, current_writer: String) -> impl IntoView {
    if entries.is_empty() {
        return view! { <p class="empty">"no entries yet"</p> }.into_any();
    }
    view! {
        <ol class="feed-list">
            {entries.into_iter().map(|e| view! {
                <EntryArticle entry=e current_writer=current_writer.clone()/>
            }).collect_view()}
        </ol>
    }
    .into_any()
}

#[component]
fn EntryArticle(entry: JournalEntry, current_writer: String) -> impl IntoView {
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
                    {render_tag_chips(&tags_raw, &current_writer)}
                </p>
            </header>
            <div class="entry-body" inner_html=body_html></div>
        </li>
    }
}

/// Render markdown source to HTML. Trusted-content path — we let raw HTML
/// pass through because every entry is written by us (CLI, UI compose, or
/// an AI we run). Add a sanitizer here if untrusted writers ever land.
fn render_markdown(src: &str) -> String {
    let parser = Parser::new(src);
    let mut out = String::with_capacity(src.len());
    html::push_html(&mut out, parser);
    out
}

/// Split a comma-separated tags field into rendered `#tag` chips. Each chip
/// is a click-through link that filters the feed by that tag while preserving
/// the current writer. Returns nothing if the field is empty.
fn render_tag_chips(tags: &str, current_writer: &str) -> AnyView {
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
                view! { <a class="tag" href=href>"#"{t}</a> }
            }).collect_view()}
        </span>
    }
    .into_any()
}

/// Build a `/path?writer=...&tag=...` URL, skipping empty params and
/// percent-encoding values. Keeps the resulting URL minimal — `/` for the
/// all-clear case, `/?tag=foo` when only tag is set, etc.
fn build_query(path: &str, writer: &str, tag: &str) -> String {
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
/// `api.rs` so we stay self-contained here without re-exporting.
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
