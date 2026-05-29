//! Humans and AIs are different first-class entities (migration 0013).
//!
//! `/people` and `/people/:slug` cover humans only (nate, maggie).
//! `/ai` and `/ai/:slug` cover the AI directory (pia, apis, cera).
//!
//! Each detail view fans out three parallel fetches: the entity itself,
//! recent journal entries by that slug (when the slug is a writer), and
//! the outgoing + incoming `links` rows so the sidecars beneath the
//! profile show "mentioned in this entity" and "backlinks."

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::api::{
    Ai, JournalEntry, Link, Person, fetch_ai_by_slug, fetch_ai_list, fetch_journal_filtered,
    fetch_links_incoming, fetch_links_outgoing, fetch_people, fetch_person,
};
use crate::pages::sidecar::{EntitySidecars, build_mention_context};

// ── /people (humans) ─────────────────────────────────────────────────────────

#[component]
pub fn PeopleListPage() -> impl IntoView {
    let data = Resource::new(
        || (),
        |_| async move { fetch_people().await.map_err(|e| e.to_string()) },
    );
    view! {
        <header class="canvas-header">
            <h1>"people"</h1>
            <p class="canvas-sub">"humans the hive knows about"</p>
        </header>
        <section class="canvas-body">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(people) => view! { <PersonList people/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn PersonList(people: Vec<Person>) -> impl IntoView {
    if people.is_empty() {
        return view! { <p class="empty">"none yet"</p> }.into_any();
    }
    view! {
        <ul class="people-list">
            {people.into_iter().map(|p| {
                let href = format!("/people/{}", p.slug);
                view! {
                    <li class="person-row">
                        <span class="person-kind">"person"</span>
                        <a class="person-name" href=href>{p.display_name}</a>
                        <span class="person-slug">{p.slug}</span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

#[derive(Clone, Serialize, Deserialize)]
struct PersonBundle {
    person: Person,
    recent_journal: Vec<JournalEntry>,
    outgoing: Vec<Link>,
    incoming: Vec<Link>,
}

#[component]
pub fn PersonDetailPage() -> impl IntoView {
    let params = use_params_map();
    let data = Resource::new(
        move || params.read().get("slug").unwrap_or_default(),
        |slug| async move {
            if slug.is_empty() {
                return Err("missing slug".to_string());
            }
            let person = fetch_person(&slug).await.map_err(|e| e.to_string())?;
            let person_id = person.id.to_string();
            let (recent_journal, outgoing, incoming) = tokio::join!(
                fetch_journal_filtered(&person.slug, "", 10),
                fetch_links_outgoing("people", &person_id),
                fetch_links_incoming("people", &person_id),
            );
            Ok(PersonBundle {
                person,
                recent_journal: recent_journal.unwrap_or_default(),
                outgoing,
                incoming,
            })
        },
    );

    view! {
        <p class="entry-back">
            <a href="/people">"← back to people"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <PersonDetail bundle/> }.into_any(),
                Err(_) => view! { <p class="empty">"no such person"</p> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn PersonDetail(bundle: PersonBundle) -> impl IntoView {
    let PersonBundle {
        person,
        recent_journal,
        outgoing,
        incoming,
    } = bundle;

    let slug = person.slug.clone();
    let notes = person.notes.clone();
    let _ctx = build_mention_context(&outgoing); // mention context for future prose render

    view! {
        <article class="entry-detail person-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{person.display_name}</h1>
                <p class="entry-meta">
                    <span class="entry-writer">"person"</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">"@"{slug}</span>
                </p>
            </header>
            {notes.map(|n| view! { <div class="person-notes">{n}</div> })}
            <RecentJournal entries=recent_journal/>
            <EntitySidecars
                outgoing=outgoing
                incoming=incoming
                entity_label="profile"
            />
        </article>
    }
}

// ── /ai (AIs) ────────────────────────────────────────────────────────────────

#[component]
pub fn AiListPage() -> impl IntoView {
    let data = Resource::new(
        || (),
        |_| async move { fetch_ai_list().await.map_err(|e| e.to_string()) },
    );
    view! {
        <header class="canvas-header">
            <h1>"ai"</h1>
            <p class="canvas-sub">"AI residents of the hive"</p>
        </header>
        <section class="canvas-body">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(ais) => view! { <AiList ais/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn AiList(ais: Vec<Ai>) -> impl IntoView {
    if ais.is_empty() {
        return view! { <p class="empty">"none yet"</p> }.into_any();
    }
    view! {
        <ul class="people-list">
            {ais.into_iter().map(|a| {
                let href = format!("/ai/{}", a.slug);
                view! {
                    <li class="person-row">
                        <span class="person-kind">{a.kind}</span>
                        <a class="person-name" href=href>{a.display_name}</a>
                        <span class="person-slug">{a.slug}</span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

#[derive(Clone, Serialize, Deserialize)]
struct AiBundle {
    ai: Ai,
    recent_journal: Vec<JournalEntry>,
    outgoing: Vec<Link>,
    incoming: Vec<Link>,
}

#[component]
pub fn AiDetailPage() -> impl IntoView {
    let params = use_params_map();
    let data = Resource::new(
        move || params.read().get("slug").unwrap_or_default(),
        |slug| async move {
            if slug.is_empty() {
                return Err("missing slug".to_string());
            }
            let ai = fetch_ai_by_slug(&slug).await.map_err(|e| e.to_string())?;
            let ai_id = ai.id.to_string();
            let (recent_journal, outgoing, incoming) = tokio::join!(
                fetch_journal_filtered(&ai.slug, "", 10),
                fetch_links_outgoing("ai", &ai_id),
                fetch_links_incoming("ai", &ai_id),
            );
            Ok(AiBundle {
                ai,
                recent_journal: recent_journal.unwrap_or_default(),
                outgoing,
                incoming,
            })
        },
    );

    view! {
        <p class="entry-back">
            <a href="/ai">"← back to ai"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <AiDetail bundle/> }.into_any(),
                Err(_) => view! { <p class="empty">"no such handle"</p> }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn AiDetail(bundle: AiBundle) -> impl IntoView {
    let AiBundle {
        ai,
        recent_journal,
        outgoing,
        incoming,
    } = bundle;

    let slug = ai.slug.clone();
    let kind = ai.kind.clone();
    let notes = ai.notes.clone();
    let _ctx = build_mention_context(&outgoing);

    view! {
        <article class="entry-detail person-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{ai.display_name}</h1>
                <p class="entry-meta">
                    <span class="entry-writer">{kind}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">"@"{slug}</span>
                </p>
            </header>
            {notes.map(|n| view! { <div class="person-notes">{n}</div> })}
            <RecentJournal entries=recent_journal/>
            <EntitySidecars
                outgoing=outgoing
                incoming=incoming
                entity_label="profile"
            />
        </article>
    }
}

// ── shared bits ──────────────────────────────────────────────────────────────

#[component]
fn RecentJournal(entries: Vec<JournalEntry>) -> impl IntoView {
    if entries.is_empty() {
        return ().into_any();
    }
    view! {
        <section class="person-recent">
            <h2 class="panel-section-title">"Recent journal entries"</h2>
            <ul class="sidecar-list">
                {entries.into_iter().map(|e| {
                    let href = format!("/journal/{}", e.id);
                    let when = e.entry_date.unwrap_or_default();
                    let title = e.title.unwrap_or_else(|| "(untitled)".to_string());
                    view! {
                        <li class="sidecar-row">
                            <span class="entry-date">{when}</span>
                            <a class="sidecar-title-text" href=href>{title}</a>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </section>
    }
    .into_any()
}
