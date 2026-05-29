//! People directory + detail.
//!
//! `/people` lists humans (kind = `human`). The companion `/ai` listing
//! lives in `pages/ai.rs` and renders the AI side off the same `people`
//! row shape; split-ai is migrating AIs into their own table, after which
//! `/ai` will read from there.
//!
//! `/people/:slug` shows a single person ... display name, recent journal
//! activity by them (when the slug matches a writer), and the standard
//! mentions/backlinks sidecars. Slugs are unique across both kinds, so
//! the detail page lookup succeeds whether the row is an AI or a human;
//! we always route AIs through `/ai/<slug>` for canonical URLs but the
//! page renders either kind correctly.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::api::{
    JournalEntry, Link, Person, fetch_journal_filtered, fetch_links_incoming, fetch_links_outgoing,
    fetch_people, fetch_person,
};
use crate::pages::sidecar::{EntitySidecars, build_mention_context};

#[component]
pub fn PeopleListPage() -> impl IntoView {
    PeopleListBy("human".into())
}

#[allow(non_snake_case)]
fn PeopleListBy(kind: String) -> impl IntoView {
    let kind_for_fetch = kind.clone();
    let data = Resource::new(
        move || kind_for_fetch.clone(),
        |kind| async move { fetch_people(&kind).await.map_err(|e| e.to_string()) },
    );
    let heading = if kind == "ai" { "ai" } else { "people" };
    let sub = if kind == "ai" {
        "AI residents of the hive"
    } else {
        "humans the hive knows about"
    };
    view! {
        <header class="canvas-header">
            <h1>{heading}</h1>
            <p class="canvas-sub">{sub}</p>
        </header>
        <section class="canvas-body">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(people) => view! { <PeopleList people=people kind=kind.clone()/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn PeopleList(people: Vec<Person>, kind: String) -> impl IntoView {
    if people.is_empty() {
        return view! { <p class="empty">"none yet"</p> }.into_any();
    }
    // Route AIs to /ai/<slug>, humans to /people/<slug>. When the listing
    // is filtered by `kind`, every row hits the same prefix; this also
    // keeps the unfiltered case sane.
    let route_for = move |row_kind: &str| -> &'static str {
        match row_kind {
            "ai" => "ai",
            _ => "people",
        }
    };
    let _ = kind; // currently informational only
    view! {
        <ul class="people-list">
            {people.into_iter().map(|p| {
                let href = format!("/{}/{}", route_for(&p.kind), p.slug);
                let kind_label = p.kind.clone();
                view! {
                    <li class="person-row">
                        <span class="person-kind">{kind_label}</span>
                        <a class="person-name" href=href>{p.display_name}</a>
                        <span class="person-slug">{p.slug}</span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

/// Bundle the detail page needs in one parallel fetch.
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
                // best-effort: empty on error so a slug that isn't a writer
                // still renders a clean detail page.
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
                Err(_) => view! {
                    <p class="empty">"no such person"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

/// `/ai/:slug` reuses the person detail page ... AIs and humans share the
/// `people` row shape until split-ai lands. The back-link header points
/// at `/ai` instead of `/people`.
#[component]
pub fn AiDetailPage() -> impl IntoView {
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
            <a href="/ai">"← back to ai"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <PersonDetail bundle/> }.into_any(),
                Err(_) => view! {
                    <p class="empty">"no such handle"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

/// `/ai` listing ... humans live in `/people` so the two views split off
/// the same `people` table by kind for now.
#[component]
pub fn AiListPage() -> impl IntoView {
    PeopleListBy("ai".into())
}

#[component]
fn PersonDetail(bundle: PersonBundle) -> impl IntoView {
    let PersonBundle {
        person,
        recent_journal,
        outgoing,
        incoming,
    } = bundle;

    let kind_label = person.kind.clone();
    let slug = person.slug.clone();
    let notes = person.notes.clone();
    let _ctx = build_mention_context(&outgoing); // not yet plumbed into person prose; kept for parity

    view! {
        <article class="entry-detail person-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{person.display_name}</h1>
                <p class="entry-meta">
                    <span class="entry-writer">{kind_label}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">"@"{slug}</span>
                </p>
            </header>
            {notes.map(|n| view! {
                <div class="person-notes">{n}</div>
            })}
            <RecentJournal entries=recent_journal/>
            <EntitySidecars
                outgoing=outgoing
                incoming=incoming
                entity_label="profile"
            />
        </article>
    }
}

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
