//! Single-note detail at `/notes/:slug` (UUID or slug).
//!
//! Title + author + tags meta, body rendered through the mention-aware
//! markdown renderer, mentions/backlinks sidecars below.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::api::{Link, Note, fetch_links_incoming, fetch_links_outgoing, fetch_note_by_slug};
use crate::markdown::render_markdown_with;
use crate::pages::sidecar::{EntitySidecars, build_mention_context};

#[derive(Clone, Serialize, Deserialize)]
struct NoteBundle {
    note: Note,
    outgoing: Vec<Link>,
    incoming: Vec<Link>,
}

#[component]
pub fn NoteDetailPage() -> impl IntoView {
    let params = use_params_map();
    let data = Resource::new(
        move || params.read().get("slug").unwrap_or_default(),
        |slug| async move {
            if slug.is_empty() {
                return Err("missing slug".to_string());
            }
            let note = fetch_note_by_slug(&slug).await.map_err(|e| e.to_string())?;
            let note_id = note.id.to_string();
            let (outgoing, incoming) = futures::join!(
                fetch_links_outgoing("notes", &note_id),
                fetch_links_incoming("notes", &note_id),
            );
            Ok(NoteBundle {
                note,
                outgoing,
                incoming,
            })
        },
    );

    view! {
        <p class="entry-back">
            <a href="/notes">"← back to notes"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <NoteDetail bundle/> }.into_any(),
                Err(_) => view! {
                    <p class="empty">"this note isn't here"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn NoteDetail(bundle: NoteBundle) -> impl IntoView {
    let NoteBundle {
        note,
        outgoing,
        incoming,
    } = bundle;
    let ctx = build_mention_context(&outgoing);
    let body_html = render_markdown_with(&note.body, &ctx);
    let title = note
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let tags_raw = note.tags.clone().unwrap_or_default();

    view! {
        <article class="entry-detail note-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{title}</h1>
                <p class="entry-meta">
                    <span class="sidecar-owner">{note.author}</span>
                    {render_tag_chips(&tags_raw)}
                </p>
            </header>
            <div class="entry-body" inner_html=body_html></div>
            <EntitySidecars
                outgoing=outgoing
                incoming=incoming
                entity_label="note"
            />
        </article>
    }
}

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
                view! { <span class="tag">"#"{t}</span> }
            }).collect_view()}
        </span>
    }
    .into_any()
}
