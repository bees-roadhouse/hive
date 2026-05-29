//! URL-driven side panel ... CSS-only toggle.
//!
//! Open state: `?side=open` mounts a fixed `<aside>` plus a click-to-dismiss
//! backdrop. Closed state: only the small "≡" toggle in the top bar renders.
//!
//! Because hive-ui is SSR-only with no WASM bundle, the toggle is a plain
//! `<a>` link that navigates between the open URL and the close URL. Both
//! URLs preserve every other query param (writer, q, tag, ...) so the user
//! doesn't lose their place when they open the panel.
//!
//! The panel reads `?writer=...` and scopes tasks + notes to that author.
//! Wire events aren't writer-scoped ... they show all.

use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_query_map};

use crate::api::{Note, Task, WireEvent, fetch_notes, fetch_tasks, fetch_wire};

/// Top-bar toggle. Open links to the same path with `side=open` added; close
/// links to the same path with `side` dropped. Always renders ... when the
/// panel is open the same link reads as "× close".
#[component]
pub fn PanelToggle() -> impl IntoView {
    let query = use_query_map();
    let location = use_location();

    let toggle = move || {
        let path = location.pathname.get();
        let mut params = query.get();
        let is_open = params.get("side").as_deref() == Some("open");
        let (label, href) = if is_open {
            params.remove("side");
            ("× close".to_string(), build_url(&path, &params))
        } else {
            params.replace("side", "open".to_string());
            ("≡".to_string(), build_url(&path, &params))
        };
        view! { <a class="hive-panel-toggle" href=href rel="external" aria-label="toggle side panel">{label}</a> }
    };

    view! { {toggle} }
}

/// The panel + backdrop. Only renders markup when `?side=open` is in the URL;
/// otherwise nothing lands in the DOM (keeps the feed clean when closed).
#[component]
pub fn SidePanel() -> impl IntoView {
    let query = use_query_map();
    let location = use_location();

    move || {
        let params = query.get();
        if params.get("side").as_deref() != Some("open") {
            return ().into_any();
        }
        let path = location.pathname.get();
        let writer = params.get("writer").unwrap_or_default();
        let mut close_params = params.clone();
        close_params.remove("side");
        let close_url = build_url(&path, &close_params);

        view! {
            <a class="panel-backdrop" href=close_url.clone() rel="external" aria-label="close side panel"></a>
            <aside class="hive-panel" aria-label="side panel">
                <header class="panel-header">
                    <h1 class="panel-title">"hive"</h1>
                    <a class="panel-close" href=close_url rel="external">"× close"</a>
                </header>
                <PanelTasks writer=writer.clone()/>
                <PanelNotes writer=writer/>
                <PanelWire/>
            </aside>
        }
        .into_any()
    }
}

#[component]
fn PanelTasks(writer: String) -> impl IntoView {
    let w = writer.clone();
    let data = Resource::new(
        move || w.clone(),
        |w| async move {
            fetch_tasks(&w, "open", false)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <details class="panel-section" open>
            <summary><h2 class="panel-section-title">"tasks"</h2></summary>
            <Suspense fallback=move || view! { <p class="panel-loading">"loading..."</p> }>
                {move || data.get().map(|res| match res {
                    Ok(rows) => view! { <PanelTaskList tasks=rows/> }.into_any(),
                    Err(msg) => view! { <p class="panel-error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </details>
    }
}

#[component]
fn PanelTaskList(tasks: Vec<Task>) -> impl IntoView {
    if tasks.is_empty() {
        return view! { <p class="panel-empty">"no open tasks"</p> }.into_any();
    }
    view! {
        <ul class="panel-list">
            {tasks.into_iter().map(|t| {
                let status_class = format!("panel-badge status-{}", t.status);
                let due = t.due.clone().unwrap_or_default();
                let href = format!("/tasks/{}", t.slug.clone().unwrap_or_else(|| t.id.to_string()));
                view! {
                    <li class="panel-row">
                        <span class=status_class>{t.status}</span>
                        <span class="panel-owner">{t.owner}</span>
                        <a class="panel-title-text" href=href rel="external">{t.title}</a>
                        {(!due.is_empty()).then(|| view! { <span class="panel-due">{due}</span> })}
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

#[component]
fn PanelNotes(writer: String) -> impl IntoView {
    let w = writer.clone();
    let data = Resource::new(
        move || w.clone(),
        |w| async move { fetch_notes(&w, "", 10).await.map_err(|e| e.to_string()) },
    );

    view! {
        <details class="panel-section" open>
            <summary><h2 class="panel-section-title">"notes"</h2></summary>
            <Suspense fallback=move || view! { <p class="panel-loading">"loading..."</p> }>
                {move || data.get().map(|res| match res {
                    Ok(rows) => view! { <PanelNoteList notes=rows/> }.into_any(),
                    Err(msg) => view! { <p class="panel-error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </details>
    }
}

#[component]
fn PanelNoteList(notes: Vec<Note>) -> impl IntoView {
    if notes.is_empty() {
        return view! { <p class="panel-empty">"no notes"</p> }.into_any();
    }
    view! {
        <ul class="panel-list">
            {notes.into_iter().map(|n| {
                let title = n.title.clone().unwrap_or_else(|| "(untitled)".to_string());
                let preview: String = n.body.chars().take(140).collect();
                let href = format!("/notes/{}", n.slug.clone().unwrap_or_else(|| n.id.to_string()));
                view! {
                    <li class="panel-row panel-row-stack">
                        <div class="panel-row-head">
                            <span class="panel-owner">{n.author}</span>
                            <a class="panel-title-text" href=href rel="external">{title}</a>
                        </div>
                        <p class="panel-preview">{preview}</p>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

#[component]
fn PanelWire() -> impl IntoView {
    let data = Resource::new(
        || (),
        |_| async move {
            fetch_wire("", "", false, 10)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <details class="panel-section" open>
            <summary><h2 class="panel-section-title">"wire"</h2></summary>
            <Suspense fallback=move || view! { <p class="panel-loading">"loading..."</p> }>
                {move || data.get().map(|res| match res {
                    Ok(rows) => view! { <PanelWireList events=rows/> }.into_any(),
                    Err(msg) => view! { <p class="panel-error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </details>
    }
}

#[component]
fn PanelWireList(events: Vec<WireEvent>) -> impl IntoView {
    if events.is_empty() {
        return view! { <p class="panel-empty">"nothing on the wire"</p> }.into_any();
    }
    view! {
        <ul class="panel-list">
            {events.into_iter().map(|e| {
                let sev = e.severity.clone().unwrap_or_else(|| "info".to_string());
                let sev_class = format!("panel-badge sev-{}", sev);
                let title_view = match e.url.clone() {
                    Some(href) if !href.is_empty() => view! {
                        <a class="panel-title-text" href=href target="_blank" rel="noopener noreferrer">{e.title.clone()}</a>
                    }.into_any(),
                    _ => view! { <span class="panel-title-text">{e.title.clone()}</span> }.into_any(),
                };
                view! {
                    <li class="panel-row">
                        <span class=sev_class>{sev}</span>
                        <span class="panel-owner">{e.source}</span>
                        {title_view}
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

/// Rebuild a URL from a path + ParamsMap. Empty query => path alone (no
/// trailing `?`).
fn build_url(path: &str, params: &leptos_router::params::ParamsMap) -> String {
    let qs = params.to_query_string();
    if qs.is_empty() || qs == "?" {
        path.to_string()
    } else {
        format!("{}{}", path, qs)
    }
}
