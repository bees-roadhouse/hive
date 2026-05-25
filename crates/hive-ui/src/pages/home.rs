use leptos::prelude::*;

use crate::api::{JournalEntry, api_base, fetch_journal};

#[component]
pub fn HomePage() -> impl IntoView {
    let data = OnceResource::new(async move { load_recent().await });

    view! {
        <header class="canvas-header">
            <h1>"journal-canvas v0"</h1>
            <p class="canvas-sub">"recent journal entries from " {api_base().to_string()}</p>
        </header>
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

async fn load_recent() -> Result<Vec<JournalEntry>, String> {
    fetch_journal(5).await.map_err(|e| e.to_string())
}

#[component]
fn EntryList(entries: Vec<JournalEntry>) -> impl IntoView {
    if entries.is_empty() {
        return view! { <p class="empty">"no entries yet"</p> }.into_any();
    }
    view! {
        <ul class="journal-list">
            {entries.into_iter().map(|e| {
                let when = e.entry_date.clone().unwrap_or_default();
                let title = e.title.clone().unwrap_or_else(|| "(untitled)".to_string());
                view! {
                    <li class="journal-entry">
                        <span class="entry-date">{when}</span>
                        <span class="entry-ai">{e.ai}</span>
                        <span class="entry-title">{title}</span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}
