use leptos::prelude::*;

use crate::api::{Note, fetch_notes};

/// Notes list with author + tag filters.
#[component]
pub fn NotesPage() -> impl IntoView {
    let (author, set_author) = signal(String::new());
    let (tag, set_tag) = signal(String::new());

    let data = Resource::new(
        move || (author.get(), tag.get()),
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
        <section class="filters">
            <label>
                "author "
                <select on:change=move |ev| set_author.set(event_target_value(&ev))>
                    <option value="">"all"</option>
                    <option value="pia">"pia"</option>
                    <option value="apis">"apis"</option>
                    <option value="cera">"cera"</option>
                    <option value="nate">"nate"</option>
                    <option value="maggie">"maggie"</option>
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
                view! {
                    <li class="note-row">
                        <div class="note-head">
                            <span class="note-author">{n.author}</span>
                            <span class="note-title">{title}</span>
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
