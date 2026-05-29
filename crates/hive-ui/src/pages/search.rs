//! FTS5 search page at `/journal/search?q=...`. Renders matches in the same
//! `EntryArticle` visual as the feed; the snippet from the search endpoint
//! lands in the body slot (highlight markers promoted to `<mark>` in the api
//! helper). Empty `q` => prompt; no results => "nothing matches".

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::{JournalEntry, fetch_journal_search};
use crate::pages::entry_article::EntryArticle;

const SEARCH_LIMIT: i64 = 50;

#[component]
pub fn SearchPage() -> impl IntoView {
    let query = use_query_map();
    let q = Memo::new(move |_| query.with(|p| p.get("q").unwrap_or_default()));

    let data = Resource::new(
        move || q.get(),
        |q| async move {
            if q.trim().is_empty() {
                return Ok(Vec::new());
            }
            fetch_journal_search(&q, SEARCH_LIMIT)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <SearchHeader q/>
        <section class="feed">
            {move || {
                let current = q.get();
                if current.trim().is_empty() {
                    return view! {
                        <p class="empty">"search the journal"</p>
                    }.into_any();
                }
                view! {
                    <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                        {move || data.get().map(|res| match res {
                            Ok(entries) => view! { <SearchResults entries q=q.get()/> }.into_any(),
                            Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                        })}
                    </Suspense>
                }
                .into_any()
            }}
        </section>
    }
}

#[component]
fn SearchHeader(q: Memo<String>) -> impl IntoView {
    view! {
        <header class="search-header">
            {move || {
                let current = q.get();
                if current.trim().is_empty() {
                    view! {
                        <form class="search-form" method="get" action="/journal/search">
                            <input
                                class="search-input"
                                type="search"
                                name="q"
                                placeholder="search the journal"
                                aria-label="search the journal"
                                autofocus
                            />
                        </form>
                    }.into_any()
                } else {
                    view! {
                        <div class="search-summary">
                            <span class="search-label">"search:"</span>
                            <span class="search-q">"\""{current}"\""</span>
                            <a class="search-clear" href="/">"× clear"</a>
                        </div>
                    }.into_any()
                }
            }}
        </header>
    }
}

#[component]
fn SearchResults(entries: Vec<JournalEntry>, q: String) -> impl IntoView {
    if entries.is_empty() {
        return view! {
            <p class="empty">"nothing matches \""{q}"\""</p>
        }
        .into_any();
    }
    view! {
        <ol class="feed-list">
            {entries.into_iter().map(|e| view! {
                <EntryArticle entry=e current_writer=String::new()/>
            }).collect_view()}
        </ol>
    }
    .into_any()
}
