//! Events list at `/events` and single-event detail at `/events/:slug`.
//!
//! The list filters by tag (text input) and limit-caps at 100 ... the
//! events table is date-anchored, so the natural ordering is the API's
//! default (sorted by `starts_at`).
//!
//! Detail page: title + when + where + (optional) body rendered through
//! the mention-aware markdown, plus the standard sidecars.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::api::{
    Event, Link, fetch_event_by_slug, fetch_events, fetch_links_incoming, fetch_links_outgoing,
};
use crate::markdown::render_markdown_with;
use crate::pages::sidecar::{EntitySidecars, build_mention_context};

#[component]
pub fn EventsPage() -> impl IntoView {
    let (tag, set_tag) = signal(String::new());

    let data = Resource::new(
        move || tag.get(),
        |tag| async move { fetch_events(&tag, 100).await.map_err(|e| e.to_string()) },
    );

    view! {
        <header class="canvas-header">
            <h1>"events"</h1>
            <p class="canvas-sub">"date-anchored events ... upcoming and past"</p>
        </header>
        <section class="filters">
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
                    Ok(events) => view! { <EventList events/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn EventList(events: Vec<Event>) -> impl IntoView {
    if events.is_empty() {
        return view! { <p class="empty">"no events"</p> }.into_any();
    }
    view! {
        <ul class="event-list">
            {events.into_iter().map(|e| {
                let when = format_when(&e.starts_at, e.ends_at.as_deref());
                let href = format!("/events/{}", e.slug);
                let location = e.location.clone().unwrap_or_default();
                let tags = e.tags.clone().unwrap_or_default();
                view! {
                    <li class="event-row">
                        <span class="event-when">{when}</span>
                        <a class="event-title" href=href>{e.title}</a>
                        <span class="event-where">{location}</span>
                        <span class="event-tags">{tags}</span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

/// Format the starts_at (+ optional ends_at) for display. ISO-8601 is the
/// wire format; we trim seconds and the timezone offset for a calmer look.
fn format_when(starts_at: &str, ends_at: Option<&str>) -> String {
    let s = trim_iso(starts_at);
    match ends_at {
        Some(end) if !end.is_empty() => {
            let e = trim_iso(end);
            // If same day, show just the end time; otherwise full range.
            if s.len() >= 10 && e.len() >= 10 && s[..10] == e[..10] {
                format!("{s} → {}", &e[11..])
            } else {
                format!("{s} → {e}")
            }
        }
        _ => s,
    }
}

fn trim_iso(s: &str) -> String {
    // `2026-05-29T14:00:00Z` → `2026-05-29 14:00`. Best-effort: leave
    // unrecognized shapes alone.
    if s.len() >= 16 && s.as_bytes().get(10) == Some(&b'T') {
        format!("{} {}", &s[..10], &s[11..16])
    } else {
        s.to_string()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct EventBundle {
    event: Event,
    outgoing: Vec<Link>,
    incoming: Vec<Link>,
}

#[component]
pub fn EventDetailPage() -> impl IntoView {
    let params = use_params_map();
    let data = Resource::new(
        move || params.read().get("slug").unwrap_or_default(),
        |slug| async move {
            if slug.is_empty() {
                return Err("missing slug".to_string());
            }
            let event = fetch_event_by_slug(&slug)
                .await
                .map_err(|e| e.to_string())?;
            let event_id = event.id.to_string();
            let (outgoing, incoming) = tokio::join!(
                fetch_links_outgoing("events", &event_id),
                fetch_links_incoming("events", &event_id),
            );
            Ok(EventBundle {
                event,
                outgoing,
                incoming,
            })
        },
    );

    view! {
        <p class="entry-back">
            <a href="/events">"← back to events"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <EventDetail bundle/> }.into_any(),
                Err(_) => view! {
                    <p class="empty">"this event isn't here"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn EventDetail(bundle: EventBundle) -> impl IntoView {
    let EventBundle {
        event,
        outgoing,
        incoming,
    } = bundle;
    let ctx = build_mention_context(&outgoing);
    let body_html = event
        .body
        .as_deref()
        .map(|b| render_markdown_with(b, &ctx))
        .unwrap_or_default();
    let has_body = !body_html.is_empty();
    let when = format_when(&event.starts_at, event.ends_at.as_deref());
    let location = event.location.clone().unwrap_or_default();
    let tags = event.tags.clone().unwrap_or_default();

    view! {
        <article class="entry-detail event-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{event.title}</h1>
                <p class="entry-meta">
                    <span class="entry-date">{when}</span>
                    {(!location.is_empty()).then(|| view! {
                        <>
                            <span class="entry-sep">"·"</span>
                            <span class="entry-date">{location}</span>
                        </>
                    })}
                    {(!tags.is_empty()).then(|| view! {
                        <>
                            <span class="entry-sep">"·"</span>
                            <span class="entry-tags">{tags}</span>
                        </>
                    })}
                </p>
            </header>
            {has_body.then(|| view! {
                <div class="entry-body" inner_html=body_html></div>
            })}
            <EntitySidecars
                outgoing=outgoing
                incoming=incoming
                entity_label="event"
            />
        </article>
    }
}
