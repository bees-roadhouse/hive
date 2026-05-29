//! Single-entry detail view at `/journal/:id`.
//!
//! Renders one journal entry as full prose with markdown, scaled a touch
//! larger than the feed for long-form reading. Tag chips link back to
//! `/?tag=foo` so they round-trip into the home filter.
//!
//! Below the prose: three sidecars surface emergent structure attached
//! to this entry ... tasks extracted from it (via `task_anchors`), the
//! entities it mentions (outgoing rows in the `links` table), and the
//! entries that reference it back (incoming rows). All three are
//! collapsible `<details>` blocks ... secondary chrome, no JS.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::api::{
    JournalEntry, Link, Task, fetch_journal_entry, fetch_links_incoming, fetch_links_outgoing,
    fetch_task_anchors,
};
use crate::markdown::render_markdown;

/// Bundle of everything the detail view needs in one parallel fetch:
/// the entry itself + its outgoing links + incoming backlinks + task
/// anchors. All four are awaited together via `tokio::join!` so the
/// page renders in a single round-trip's worth of latency.
#[derive(Clone, Serialize, Deserialize)]
struct DetailBundle {
    entry: JournalEntry,
    outgoing: Vec<Link>,
    incoming: Vec<Link>,
    anchored_tasks: Vec<Task>,
}

#[component]
pub fn EntryPage() -> impl IntoView {
    let params = use_params_map();

    let data = Resource::new(
        move || params.read().get("id").unwrap_or_default(),
        |id| async move {
            if id.is_empty() {
                return Err("missing id".to_string());
            }
            // Fetch the entry first; without it nothing else matters.
            // Then fan out the sidecar fetches in parallel ... each one
            // already swallows its own errors and returns empty on
            // failure, so a missing endpoint doesn't sink the page.
            let entry = fetch_journal_entry(&id).await.map_err(|e| e.to_string())?;
            let entry_id = entry.id.to_string();
            let (outgoing, incoming, anchored_tasks) = futures::join!(
                fetch_links_outgoing("journal_entries", &entry_id),
                fetch_links_incoming("journal_entries", &entry_id),
                fetch_task_anchors(&entry_id),
            );
            Ok(DetailBundle {
                entry,
                outgoing,
                incoming,
                anchored_tasks,
            })
        },
    );

    view! {
        <p class="entry-back">
            <a href="/">"← back to journal"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <EntryDetail bundle/> }.into_any(),
                Err(_) => view! {
                    <p class="empty">"this entry isn't here"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn EntryDetail(bundle: DetailBundle) -> impl IntoView {
    let DetailBundle {
        entry,
        outgoing,
        incoming,
        anchored_tasks,
    } = bundle;

    let when = entry.entry_date.clone().unwrap_or_default();
    let title = entry
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    let tags_raw = entry.tags.clone().unwrap_or_default();
    // The detail page doesn't yet have a per-entry mention resolver
    // (we'd need to join `links` with target entity titles + slugs to
    // build the full ResolvedMention map). For now we use the slug-only
    // renderer ... mention text shows the raw `@slug` / `[[type:slug]]`,
    // but hrefs are correct. Enrichment can layer on later by walking
    // the `outgoing` rows once the link API ships joined-title data.
    let body_html = render_markdown(&entry.body);

    view! {
        <article class="entry-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{title}</h1>
                <p class="entry-meta">
                    <span class="entry-writer">{entry.ai}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="entry-date">{when}</span>
                    {render_tag_chips(&tags_raw)}
                </p>
            </header>
            <div class="entry-body" inner_html=body_html></div>
            <Sidecars outgoing incoming anchored_tasks/>
        </article>
    }
}

/// Tag chips that link back to the home feed's `?tag=foo` filter.
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
                let href = format!("/?tag={}", urlencode(&t));
                view! { <a class="tag" href=href>"#"{t}</a> }
            }).collect_view()}
        </span>
    }
    .into_any()
}

/// The three sidecar `<details>` blocks. Each renders even when empty
/// ... empty state is a quiet "(none yet)" string so the structure is
/// visible to the reader without dominating the page.
#[component]
fn Sidecars(outgoing: Vec<Link>, incoming: Vec<Link>, anchored_tasks: Vec<Task>) -> impl IntoView {
    view! {
        <section class="sidecars" aria-label="emergent structure">
            <SidecarTasks tasks=anchored_tasks/>
            <SidecarMentions links=outgoing/>
            <SidecarBacklinks links=incoming/>
        </section>
    }
}

#[component]
fn SidecarTasks(tasks: Vec<Task>) -> impl IntoView {
    if tasks.is_empty() {
        return view! {
            <details class="sidecar" open>
                <summary><h2 class="panel-section-title">"Tasks extracted from this entry"</h2></summary>
                <p class="sidecar-empty">"(no anchored tasks yet)"</p>
            </details>
        }
        .into_any();
    }
    view! {
        <details class="sidecar" open>
            <summary><h2 class="panel-section-title">"Tasks extracted from this entry"</h2></summary>
            <ul class="sidecar-list sidecar-tasks">
                {tasks.into_iter().map(|t| {
                    let status_class = format!("sidecar-badge status-{}", t.status);
                    let href = format!("/tasks/{}", t.id);
                    view! {
                        <li class="sidecar-row">
                            <span class=status_class>{t.status}</span>
                            <span class="sidecar-owner">{t.owner}</span>
                            <a class="sidecar-title-text" href=href>{t.title}</a>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </details>
    }
    .into_any()
}

#[component]
fn SidecarMentions(links: Vec<Link>) -> impl IntoView {
    if links.is_empty() {
        return view! {
            <details class="sidecar" open>
                <summary><h2 class="panel-section-title">"Mentioned in this entry"</h2></summary>
                <p class="sidecar-empty">"(no mentions resolved)"</p>
            </details>
        }
        .into_any();
    }
    // Group by target_table so each kind gets its own subhead.
    let mut groups: std::collections::BTreeMap<String, Vec<Link>> =
        std::collections::BTreeMap::new();
    for l in links {
        groups.entry(l.target_table.clone()).or_default().push(l);
    }
    view! {
        <details class="sidecar" open>
            <summary><h2 class="panel-section-title">"Mentioned in this entry"</h2></summary>
            <ul class="sidecar-list sidecar-mentions">
                {groups.into_iter().map(|(table, rows)| {
                    let label = pretty_table(&table).to_string();
                    view! {
                        <li class="sidecar-group">
                            <span class="sidecar-group-label">{label}</span>
                            <ul class="sidecar-group-list">
                                {rows.into_iter().map(|l| view! {
                                    <li class="sidecar-row">{render_link_target(&l)}</li>
                                }).collect_view()}
                            </ul>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </details>
    }
    .into_any()
}

#[component]
fn SidecarBacklinks(links: Vec<Link>) -> impl IntoView {
    if links.is_empty() {
        return view! {
            <details class="sidecar">
                <summary><h2 class="panel-section-title">"Backlinks"</h2></summary>
                <p class="sidecar-empty">"(no inbound references)"</p>
            </details>
        }
        .into_any();
    }
    view! {
        <details class="sidecar">
            <summary><h2 class="panel-section-title">"Backlinks (entries that reference this one)"</h2></summary>
            <ul class="sidecar-list sidecar-backlinks">
                {links.into_iter().map(|l| {
                    let table = pretty_table(&l.source_table).to_string();
                    view! {
                        <li class="sidecar-row">
                            <span class="sidecar-group-label">{table}</span>
                            {render_link_source(&l)}
                        </li>
                    }
                }).collect_view()}
            </ul>
        </details>
    }
    .into_any()
}

/// Render the "target" side of a link row: a link to the target entity
/// using its slug when known (cleaner URL), falling back to its UUID.
/// Display text prefers `target_title` over the UUID for the same reason.
fn render_link_target(l: &Link) -> AnyView {
    let route = table_route(&l.target_table);
    let slug = l.target_slug.clone().filter(|s| !s.is_empty());
    let href = match slug.as_deref() {
        Some(s) => format!("/{route}/{s}"),
        None => format!("/{route}/{}", l.target_id),
    };
    let label = l
        .target_title
        .clone()
        .filter(|s| !s.is_empty())
        .or(slug)
        .unwrap_or_else(|| l.target_id.to_string());
    view! { <a class="sidecar-title-text" href=href>{label}</a> }.into_any()
}

/// Render the "source" side of a link row (used by backlinks).
fn render_link_source(l: &Link) -> AnyView {
    let route = table_route(&l.source_table);
    let slug = l.source_slug.clone().filter(|s| !s.is_empty());
    let href = match slug.as_deref() {
        Some(s) => format!("/{route}/{s}"),
        None => format!("/{route}/{}", l.source_id),
    };
    let label = l
        .source_title
        .clone()
        .filter(|s| !s.is_empty())
        .or(slug)
        .unwrap_or_else(|| l.source_id.to_string());
    view! { <a class="sidecar-title-text" href=href>{label}</a> }.into_any()
}

/// `journal_entries` → "journal", `tasks` → "tasks", etc. The DB-side
/// names are plural snake_case; the UI routes are the same minus the
/// `_entries` suffix.
fn table_route(table: &str) -> &str {
    match table {
        "journal_entries" => "journal",
        "tasks" => "tasks",
        "notes" => "notes",
        "wire_events" => "wire",
        "events" => "events",
        "people" => "people",
        other => other,
    }
}

/// Human label for the section subheads ... "tasks" stays "tasks", but
/// `journal_entries` reads better as "Journal entries".
fn pretty_table(table: &str) -> &str {
    match table {
        "journal_entries" => "Journal entries",
        "tasks" => "Tasks",
        "notes" => "Notes",
        "wire_events" => "Wire events",
        "events" => "Events",
        "people" => "People",
        other => other,
    }
}

/// Minimal percent-encoding for the tag query value. Mirrors the one in
/// `api.rs` — kept inline here to avoid widening that module's surface.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
