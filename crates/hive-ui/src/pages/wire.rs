use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::{WireEvent, fetch_wire};

/// Wire-event list with source + severity filters and an "unacked only" toggle.
/// URL-driven for SSR reliability.
#[component]
pub fn WirePage() -> impl IntoView {
    let query = use_query_map();
    let source = query
        .with_untracked(|q| q.get("source"))
        .unwrap_or_default();
    let severity = query
        .with_untracked(|q| q.get("severity"))
        .unwrap_or_default();
    let unacked_raw = query
        .with_untracked(|q| q.get("unacknowledged"))
        .unwrap_or_default();
    let unacked = matches!(unacked_raw.as_str(), "true" | "1" | "on");

    let source_for_fetch = source.clone();
    let severity_for_fetch = severity.clone();
    let data = Resource::new(
        move || {
            (
                source_for_fetch.clone(),
                severity_for_fetch.clone(),
                unacked,
            )
        },
        |(source, severity, unacked)| async move {
            fetch_wire(&source, &severity, unacked, 50)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <header class="canvas-header">
            <h1>"wire"</h1>
            <p class="canvas-sub">"situational-awareness events"</p>
        </header>
        <form class="filters" method="get" action="/wire">
            <label>
                "source "
                <input type="text" name="source" placeholder="e.g. cisa-kev" value=source/>
            </label>
            <label>
                "severity "
                <select name="severity">
                    <option value="">"any"</option>
                    <option value="critical" selected=severity == "critical">"critical"</option>
                    <option value="high" selected=severity == "high">"high"</option>
                    <option value="medium" selected=severity == "medium">"medium"</option>
                    <option value="low" selected=severity == "low">"low"</option>
                    <option value="info" selected=severity == "info">"info"</option>
                </select>
            </label>
            <label>
                <input type="checkbox" name="unacknowledged" value="true" checked=unacked/>
                " unacked only"
            </label>
            <button class="filter-apply" type="submit">"apply"</button>
            <a class="filter-clear" href="/wire" rel="external">"clear"</a>
        </form>
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
fn EventList(events: Vec<WireEvent>) -> impl IntoView {
    if events.is_empty() {
        return view! { <p class="empty">"no events match"</p> }.into_any();
    }
    view! {
        <ul class="wire-list">
            {events.into_iter().map(|e| {
                let severity = e.severity.clone().unwrap_or_else(|| "info".to_string());
                let sev_class = format!("wire-sev sev-{}", severity);
                let affects = e.affects.clone().unwrap_or_default();
                let url = e.url.clone();
                let acked = e.acknowledged;
                view! {
                    <li class="wire-row">
                        <span class=sev_class>{severity}</span>
                        <span class="wire-source">{e.source}</span>
                        <span class="wire-title">
                            {match url {
                                Some(u) => view! {
                                    <a href=u target="_blank" rel="noreferrer">{e.title.clone()}</a>
                                }.into_any(),
                                None => view! { {e.title.clone()} }.into_any(),
                            }}
                            <span class="wire-meta">
                                {affects}
                                {move || if acked { " · acked" } else { "" }}
                            </span>
                        </span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}
