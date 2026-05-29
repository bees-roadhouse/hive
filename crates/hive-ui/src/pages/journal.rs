use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::{JournalEntry, fetch_journal_filtered};

/// Full journal list with an `ai` + `tag` filter. Filters are URL-driven so
/// they work as SSR round-trips instead of calling the wasm-side fetch stubs.
#[component]
pub fn JournalPage() -> impl IntoView {
    let query = use_query_map();
    let ai = query.with_untracked(|q| q.get("ai")).unwrap_or_default();
    let tag = query.with_untracked(|q| q.get("tag")).unwrap_or_default();

    let ai_for_fetch = ai.clone();
    let tag_for_fetch = tag.clone();
    let data = Resource::new(
        move || (ai_for_fetch.clone(), tag_for_fetch.clone()),
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
        <form class="filters" method="get" action="/journal">
            <label>
                "ai "
                <select name="ai">
                    <option value="">"all"</option>
                    <option value="pia" selected=ai == "pia">"pia"</option>
                    <option value="apis" selected=ai == "apis">"apis"</option>
                    <option value="cera" selected=ai == "cera">"cera"</option>
                    <option value="nate" selected=ai == "nate">"nate"</option>
                    <option value="maggie" selected=ai == "maggie">"maggie"</option>
                </select>
            </label>
            <label>
                "tag "
                <input type="text" name="tag" placeholder="filter by tag" value=tag/>
            </label>
            <button class="filter-apply" type="submit">"apply"</button>
            <a class="filter-clear" href="/journal" rel="external">"clear"</a>
        </form>
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
                let href = format!("/journal/{}", e.slug.clone().unwrap_or_else(|| e.id.to_string()));
                view! {
                    <li class="journal-entry">
                        <span class="entry-date">{when}</span>
                        <span class="entry-ai">{e.ai}</span>
                        <span class="entry-title">
                            <a class="row-link" href=href rel="external">{title}</a>
                            <span class="entry-tags">{tags}</span>
                        </span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}
