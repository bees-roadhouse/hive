use leptos::prelude::*;

use crate::api::{JournalEntry, fetch_journal_filtered};

/// Full journal list with an `ai` + `tag` filter. The filter signals drive a
/// `Resource` that re-fetches whenever either changes.
#[component]
pub fn JournalPage() -> impl IntoView {
    let (ai, set_ai) = signal(String::new());
    let (tag, set_tag) = signal(String::new());

    let data = Resource::new(
        move || (ai.get(), tag.get()),
        |(ai, tag)| async move {
            fetch_journal_filtered(&ai, &tag, 50)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <header class="canvas-header">
            <h1>"journal"</h1>
            <p class="canvas-sub">"all entries from the hive"</p>
        </header>
        <section class="filters">
            <label>
                "ai "
                <select on:change=move |ev| set_ai.set(event_target_value(&ev))>
                    <option value="">"all"</option>
                    <option value="pia">"pia"</option>
                    <option value="apis">"apis"</option>
                    <option value="cera">"cera"</option>
                    <option value="nate">"nate"</option>
                </select>
            </label>
            <label>
                "tag "
                <input
                    type="text"
                    placeholder="filter by tag"
                    on:input=move |ev| set_tag.set(event_target_value(&ev))
                    prop:value=move || tag.get()
                />
            </label>
        </section>
        <section class="canvas-body">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(entries) => view! { <EntryList entries/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn EntryList(entries: Vec<JournalEntry>) -> impl IntoView {
    if entries.is_empty() {
        return view! { <p class="empty">"no entries match"</p> }.into_any();
    }
    view! {
        <ul class="journal-list">
            {entries.into_iter().map(|e| {
                let when = e.entry_date.clone().unwrap_or_default();
                let title = e.title.clone().unwrap_or_else(|| "(untitled)".to_string());
                let tags = e.tags.clone().unwrap_or_default();
                view! {
                    <li class="journal-entry">
                        <span class="entry-date">{when}</span>
                        <span class="entry-ai">{e.ai}</span>
                        <span class="entry-title">
                            {title}
                            <span class="entry-tags">{tags}</span>
                        </span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}
