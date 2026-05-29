//! Journal feed — the home view and centerpiece of hive-ui.
//!
//! Single-column chronological stream of entries from every writer (AIs and
//! humans alike), rendered as full prose with markdown. Writer-chip filter
//! across the top, free-text tag filter. No card chrome between entries —
//! structure emerges from the prose itself.

use leptos::prelude::*;

use crate::api::{JournalEntry, fetch_journal_filtered};
use crate::pages::entry_article::EntryArticle;

/// Writers we surface as one-click chips. The "all" chip is implicit (empty
/// filter). Order is editorial — Pia first because she writes the most.
const WRITERS: &[&str] = &["pia", "apis", "nate", "maggie", "cera"];

/// Page-size for the feed fetch. Generous on purpose — the feed is the whole
/// view and we'd rather scroll than paginate at this scale.
const FEED_LIMIT: i64 = 100;

#[component]
pub fn HomePage() -> impl IntoView {
    let (writer, set_writer) = signal(String::new());
    let (tag, set_tag) = signal(String::new());

    let data = Resource::new(
        move || (writer.get(), tag.get()),
        |(writer, tag)| async move {
            fetch_journal_filtered(&writer, &tag, FEED_LIMIT)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <section class="feed-controls">
            <div class="writer-chips">
                <WriterChip label="all".to_string() value=String::new() writer set_writer/>
                {WRITERS.iter().map(|w| {
                    let label = w.to_string();
                    let value = w.to_string();
                    view! { <WriterChip label value writer set_writer/> }
                }).collect_view()}
            </div>
            <input
                class="tag-input"
                type="text"
                placeholder="filter by tag"
                on:input=move |ev| set_tag.set(event_target_value(&ev))
                prop:value=move || tag.get()
            />
        </section>

        <section class="feed">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(entries) => view! { <Feed entries/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn WriterChip(
    label: String,
    value: String,
    writer: ReadSignal<String>,
    set_writer: WriteSignal<String>,
) -> impl IntoView {
    let for_class = value.clone();
    let for_click = value.clone();
    view! {
        <button
            class:active=move || writer.get() == for_class
            on:click=move |_| set_writer.set(for_click.clone())
        >
            {label}
        </button>
    }
}

#[component]
fn Feed(entries: Vec<JournalEntry>) -> impl IntoView {
    if entries.is_empty() {
        return view! { <p class="empty">"no entries yet"</p> }.into_any();
    }
    view! {
        <ol class="feed-list">
            {entries.into_iter().map(|e| view! { <EntryArticle entry=e/> }).collect_view()}
        </ol>
    }
    .into_any()
}
