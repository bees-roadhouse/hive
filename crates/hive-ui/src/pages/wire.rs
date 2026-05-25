use leptos::prelude::*;

use crate::api::{fetch_wire, WireEvent};

/// Wire-event list with source + severity filters and an "unacked only" toggle.
#[component]
pub fn WirePage() -> impl IntoView {
    let (source, set_source) = signal(String::new());
    let (severity, set_severity) = signal(String::new());
    let (unacked, set_unacked) = signal(false);

    let data = Resource::new(
        move || (source.get(), severity.get(), unacked.get()),
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
        <section class="filters">
            <label>
                "source "
                <input
                    type="text"
                    placeholder="e.g. cisa-kev"
                    on:input=move |ev| set_source.set(event_target_value(&ev))
                    prop:value=move || source.get()
                />
            </label>
            <label>
                "severity "
                <select on:change=move |ev| set_severity.set(event_target_value(&ev))>
                    <option value="">"any"</option>
                    <option value="critical">"critical"</option>
                    <option value="high">"high"</option>
                    <option value="medium">"medium"</option>
                    <option value="low">"low"</option>
                    <option value="info">"info"</option>
                </select>
            </label>
            <label>
                <input
                    type="checkbox"
                    on:change=move |ev| set_unacked.set(event_target_checked(&ev))
                />
                " unacked only"
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
    }.into_any()
}
