//! Single-task detail at `/tasks/:slug` (UUID or slug).
//!
//! Quick-meta at the top (status badge + owner + project + due), the
//! task body rendered through the mention-aware markdown renderer, and
//! the standard mentions/backlinks sidecars below.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use serde::{Deserialize, Serialize};

use crate::api::{Link, Task, fetch_links_incoming, fetch_links_outgoing, fetch_task_by_slug};
use crate::markdown::render_markdown_with;
use crate::pages::sidecar::{EntitySidecars, build_mention_context};

#[derive(Clone, Serialize, Deserialize)]
struct TaskBundle {
    task: Task,
    outgoing: Vec<Link>,
    incoming: Vec<Link>,
}

#[component]
pub fn TaskDetailPage() -> impl IntoView {
    let params = use_params_map();
    let data = Resource::new(
        move || params.read().get("slug").unwrap_or_default(),
        |slug| async move {
            if slug.is_empty() {
                return Err("missing slug".to_string());
            }
            let task = fetch_task_by_slug(&slug).await.map_err(|e| e.to_string())?;
            let task_id = task.id.to_string();
            let (outgoing, incoming) = futures::join!(
                fetch_links_outgoing("tasks", &task_id),
                fetch_links_incoming("tasks", &task_id),
            );
            Ok(TaskBundle {
                task,
                outgoing,
                incoming,
            })
        },
    );

    view! {
        <p class="entry-back">
            <a href="/tasks">"← back to tasks"</a>
        </p>
        <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
            {move || data.get().map(|result| match result {
                Ok(bundle) => view! { <TaskDetail bundle/> }.into_any(),
                Err(_) => view! {
                    <p class="empty">"this task isn't here"</p>
                }.into_any(),
            })}
        </Suspense>
    }
}

#[component]
fn TaskDetail(bundle: TaskBundle) -> impl IntoView {
    let TaskBundle {
        task,
        outgoing,
        incoming,
    } = bundle;

    let ctx = build_mention_context(&outgoing);
    let body_html = task
        .body
        .as_deref()
        .map(|b| render_markdown_with(b, &ctx))
        .unwrap_or_default();
    let has_body = !body_html.is_empty();
    let status_class = format!("sidecar-badge status-{}", task.status);
    let project = task.project.clone().unwrap_or_default();
    let priority = task.priority.clone().unwrap_or_default();
    let due = task.due.clone().unwrap_or_default();

    view! {
        <article class="entry-detail task-detail">
            <header class="entry-detail-header">
                <h1 class="entry-detail-title">{task.title}</h1>
                <p class="entry-meta">
                    <span class=status_class>{task.status}</span>
                    <span class="entry-sep">"·"</span>
                    <span class="sidecar-owner">{task.owner}</span>
                    {(!project.is_empty()).then(|| view! {
                        <>
                            <span class="entry-sep">"·"</span>
                            <span class="entry-date">{project}</span>
                        </>
                    })}
                    {(!priority.is_empty()).then(|| view! {
                        <>
                            <span class="entry-sep">"·"</span>
                            <span class="entry-date">"p:"{priority}</span>
                        </>
                    })}
                    {(!due.is_empty()).then(|| view! {
                        <>
                            <span class="entry-sep">"·"</span>
                            <span class="entry-date">"due "{due}</span>
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
                entity_label="task"
            />
        </article>
    }
}
