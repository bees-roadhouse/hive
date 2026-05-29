use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::{Note, fetch_notes};

/// Notes list with author + tag filters.
#[component]
pub fn NotesPage() -> impl IntoView {
    let query = use_query_map();
    let author = query
        .with_untracked(|q| q.get("author"))
        .unwrap_or_default();
    let tag = query.with_untracked(|q| q.get("tag")).unwrap_or_default();

    let author_for_fetch = author.clone();
    let tag_for_fetch = tag.clone();
    let data = Resource::new(
        move || (author_for_fetch.clone(), tag_for_fetch.clone()),
        |(author, tag)| async move {
            fetch_notes(&author, &tag, 50)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <header class="canvas-header">
            <h1>"notes"</h1>
            <p class="canvas-sub">"shared free-form notes"</p>
        </header>
        <form class="filters" method="get" action="/notes">
            <label>
                "author "
                <select name="author">
                    <option value="">"all"</option>
                    <option value="pia" selected=author == "pia">"pia"</option>
                    <option value="apis" selected=author == "apis">"apis"</option>
                    <option value="cera" selected=author == "cera">"cera"</option>
                    <option value="nate" selected=author == "nate">"nate"</option>
                    <option value="maggie" selected=author == "maggie">"maggie"</option>
                </select>
            </label>
            <label>
                "tag "
                <input type="text" name="tag" placeholder="filter by tag" value=tag/>
            </label>
            <button class="filter-apply" type="submit">"apply"</button>
            <a class="filter-clear" href="/notes" rel="external">"clear"</a>
        </form>
        <section class="canvas-body">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(notes) => view! { <NoteList notes/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn NoteList(notes: Vec<Note>) -> impl IntoView {
    if notes.is_empty() {
        return view! { <p class="empty">"no notes match"</p> }.into_any();
    }
    view! {
        <ul class="note-list">
            {notes.into_iter().map(|n| {
                let title = n.title.clone().unwrap_or_else(|| "(untitled)".to_string());
                let tags = n.tags.clone().unwrap_or_default();
                let preview: String = n.body.chars().take(160).collect();
                let href = format!("/notes/{}", n.slug.clone().unwrap_or_else(|| n.id.to_string()));
                view! {
                    <li class="note-row">
                        <div class="note-head">
                            <span class="note-author">{n.author}</span>
                            <a class="note-title row-link" href=href rel="external">{title}</a>
                            <span class="note-tags">{tags}</span>
                        </div>
                        <p class="note-preview">{preview}</p>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}
