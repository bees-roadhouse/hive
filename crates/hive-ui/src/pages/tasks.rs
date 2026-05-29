use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::api::{Task, fetch_tasks};

/// Task list with owner + status filters and an "include closed" toggle.
#[component]
pub fn TasksPage() -> impl IntoView {
    let query = use_query_map();
    let owner = query.with_untracked(|q| q.get("owner")).unwrap_or_default();
    let status = query
        .with_untracked(|q| q.get("status"))
        .unwrap_or_default();
    let all_raw = query.with_untracked(|q| q.get("all")).unwrap_or_default();
    let all = matches!(all_raw.as_str(), "true" | "1" | "on");

    let owner_for_fetch = owner.clone();
    let status_for_fetch = status.clone();
    let data = Resource::new(
        move || (owner_for_fetch.clone(), status_for_fetch.clone(), all),
        |(owner, status, all)| async move {
            fetch_tasks(&owner, &status, all)
                .await
                .map_err(|e| e.to_string())
        },
    );

    view! {
        <header class="canvas-header">
            <h1>"tasks"</h1>
            <p class="canvas-sub">"shared task tracker"</p>
        </header>
        <form class="filters" method="get" action="/tasks">
            <label>
                "owner "
                <select name="owner">
                    <option value="">"all"</option>
                    <option value="pia" selected=owner == "pia">"pia"</option>
                    <option value="apis" selected=owner == "apis">"apis"</option>
                    <option value="cera" selected=owner == "cera">"cera"</option>
                    <option value="nate" selected=owner == "nate">"nate"</option>
                    <option value="maggie" selected=owner == "maggie">"maggie"</option>
                </select>
            </label>
            <label>
                "status "
                <select name="status">
                    <option value="">"any"</option>
                    <option value="open" selected=status == "open">"open"</option>
                    <option value="in_progress" selected=status == "in_progress">"in_progress"</option>
                    <option value="blocked" selected=status == "blocked">"blocked"</option>
                    <option value="done" selected=status == "done">"done"</option>
                    <option value="dropped" selected=status == "dropped">"dropped"</option>
                </select>
            </label>
            <label>
                <input type="checkbox" name="all" value="true" checked=all/>
                " include closed"
            </label>
            <button class="filter-apply" type="submit">"apply"</button>
            <a class="filter-clear" href="/tasks" rel="external">"clear"</a>
        </form>
        <section class="canvas-body">
            <Suspense fallback=move || view! { <p class="loading">"loading..."</p> }>
                {move || data.get().map(|result| match result {
                    Ok(tasks) => view! { <TaskList tasks/> }.into_any(),
                    Err(msg) => view! { <p class="error">"error: " {msg}</p> }.into_any(),
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn TaskList(tasks: Vec<Task>) -> impl IntoView {
    if tasks.is_empty() {
        return view! { <p class="empty">"no tasks match"</p> }.into_any();
    }
    view! {
        <ul class="task-list">
            {tasks.into_iter().map(|t| {
                let priority = t.priority.clone().unwrap_or_default();
                let project = t.project.clone().unwrap_or_default();
                let status_class = format!("task-status status-{}", t.status);
                let href = format!("/tasks/{}", t.slug.clone().unwrap_or_else(|| t.id.to_string()));
                view! {
                    <li class="task-row">
                        <span class=status_class>{t.status}</span>
                        <span class="task-owner">{t.owner}</span>
                        <span class="task-title">
                            <a class="row-link" href=href rel="external">{t.title}</a>
                            <span class="task-meta">{project}" "{priority}</span>
                        </span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}
