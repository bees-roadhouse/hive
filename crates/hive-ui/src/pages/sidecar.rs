//! Shared "Mentioned in this <entity>" / "Backlinks" sidecar widgets, used
//! by every entity-detail page. The journal detail page (`pages/entry.rs`)
//! kept its own copy historically; this module is the common path forward.
//!
//! Visual language mirrors the journal sidecars: collapsible `<details>`
//! blocks with subdued chrome, dotted-row dividers, "Mentioned" open by
//! default, "Backlinks" closed by default.

use leptos::prelude::*;

use crate::api::Link;

/// The two sidecars beneath an entity's prose. `entity_label` is the kind
/// word used in the summary ("note", "task", "event", "person"). It's
/// purely cosmetic ... the visual structure is identical across kinds.
#[component]
pub fn EntitySidecars(
    /// Outgoing links from this entity (the things it mentions).
    outgoing: Vec<Link>,
    /// Incoming links targeting this entity (the entities that mention it).
    incoming: Vec<Link>,
    /// The word used in the "Mentioned in this <entity>" summary.
    #[prop(into)]
    entity_label: String,
) -> impl IntoView {
    view! {
        <section class="sidecars" aria-label="emergent structure">
            <SidecarMentions links=outgoing entity_label=entity_label/>
            <SidecarBacklinks links=incoming/>
        </section>
    }
}

#[component]
fn SidecarMentions(links: Vec<Link>, entity_label: String) -> impl IntoView {
    let title = format!("Mentioned in this {entity_label}");
    if links.is_empty() {
        return view! {
            <details class="sidecar" open>
                <summary><h2 class="panel-section-title">{title}</h2></summary>
                <p class="sidecar-empty">"(no mentions resolved)"</p>
            </details>
        }
        .into_any();
    }
    let mut groups: std::collections::BTreeMap<String, Vec<Link>> =
        std::collections::BTreeMap::new();
    for l in links {
        groups.entry(l.target_table.clone()).or_default().push(l);
    }
    view! {
        <details class="sidecar" open>
            <summary><h2 class="panel-section-title">{title}</h2></summary>
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
/// Display text prefers `target_title` over the slug for the same reason.
pub fn render_link_target(l: &Link) -> AnyView {
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
        .or_else(|| slug.clone())
        .unwrap_or_else(|| l.target_id.to_string());
    view! { <a class="sidecar-title-text" href=href>{label}</a> }.into_any()
}

/// Render the "source" side of a link row (used by backlinks).
pub fn render_link_source(l: &Link) -> AnyView {
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
        .or_else(|| slug.clone())
        .unwrap_or_else(|| l.source_id.to_string());
    view! { <a class="sidecar-title-text" href=href>{label}</a> }.into_any()
}

/// `journal_entries` → "journal", `tasks` → "tasks", etc. The DB-side
/// names are plural snake_case; the UI routes are the same minus the
/// `_entries` suffix.
pub fn table_route(table: &str) -> &str {
    match table {
        "journal_entries" => "journal",
        "tasks" => "tasks",
        "notes" => "notes",
        "wire_events" => "wire",
        "events" => "events",
        "people" => "people",
        "ai" => "ai",
        other => other,
    }
}

/// Human label for the section subheads ... "tasks" stays "tasks", but
/// `journal_entries` reads better as "Journal entries".
pub fn pretty_table(table: &str) -> &str {
    match table {
        "journal_entries" => "Journal entries",
        "tasks" => "Tasks",
        "notes" => "Notes",
        "wire_events" => "Wire events",
        "events" => "Events",
        "people" => "People",
        "ai" => "AIs",
        other => other,
    }
}

/// Build the resolution map for the markdown renderer from a vec of
/// outgoing `Link` rows. Each row contributes an entry keyed by the raw
/// token shape it would appear as in the source prose ... `@slug`,
/// `[[type:slug]]`. When the same slug appears as both, the typed shape
/// wins (it's more specific).
///
/// The resolver writes one link per resolved mention, so this map ends
/// up the same size as the relevant subset of `links`.
pub fn build_mention_context(links: &[Link]) -> crate::markdown::MentionContext {
    use crate::markdown::{MentionContext, ResolvedMention};
    let mut ctx = MentionContext::empty();
    for l in links {
        let Some(target_slug) = l.target_slug.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let display = l
            .target_title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| target_slug.to_string());
        let route = table_route(&l.target_table);
        let href = format!("/{route}/{target_slug}");
        let kind_class = match l.target_table.as_str() {
            "journal_entries" => "mention-journal".to_string(),
            "tasks" => "mention-task".to_string(),
            "notes" => "mention-note".to_string(),
            "events" => "mention-event".to_string(),
            "people" => "mention-person".to_string(),
            "ai" => "mention-ai".to_string(),
            other => format!("mention-{other}"),
        };
        let resolved = ResolvedMention {
            href,
            display,
            kind_class,
        };
        // Map both possible raw token shapes that point at this target:
        //   - `[[<type>:<slug>]]` (typed wikilink)
        //   - `@<slug>` (people / ai short form)
        let type_key = match l.target_table.as_str() {
            "tasks" => "task",
            "notes" => "note",
            "events" => "event",
            "journal_entries" => "journal",
            "people" => "person",
            "ai" => "ai",
            other => other,
        };
        ctx.resolved
            .insert(format!("[[{type_key}:{target_slug}]]"), resolved.clone());
        if matches!(l.target_table.as_str(), "people" | "ai") {
            ctx.resolved
                .insert(format!("@{target_slug}"), resolved.clone());
        }
        // Title-based fuzzy resolver also writes mentions for `[[Title]]`
        // shapes. The resolver records the raw inner string in the link
        // row's `note` field when it can; otherwise the bracketed slug
        // shape above covers most cases.
    }
    ctx
}
