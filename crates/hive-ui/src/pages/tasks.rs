use leptos::prelude::*;

use crate::api::{fetch_tasks, Task};

/// Task list with owner + status filters and an "include closed" toggle.
#[component]
pub fn TasksPage() -> impl IntoView {
    let (owner, set_owner) = signal(String::new());
    let (status, set_status) = signal(String::new());
    let (all, set_all) = signal(false);

    let data = Resource::new(
        move || (owner.get(), status.get(), all.get()),
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
        <section class="filters">
            <label>
                "owner "
                <select on:change=move |ev| set_owner.set(event_target_value(&ev))>
                    <option value="">"all"</option>
                    <option value="pia">"pia"</option>
                    <option value="apis">"apis"</option>
                    <option value="cera">"cera"</option>
                    <option value="nate">"nate"</option>
                    <option value="maggie">"maggie"</option>
                </select>
            </label>
            <label>
                "status "
                <select on:change=move |ev| set_status.set(event_target_value(&ev))>
                    <option value="">"any"</option>
                    <option value="open">"open"</option>
                    <option value="in_progress">"in_progress"</option>
                    <option value="blocked">"blocked"</option>
                    <option value="done">"done"</option>
                    <option value="dropped">"dropped"</option>
                </select>
            </label>
            <label>
                <input
                    type="checkbox"
                    on:change=move |ev| set_all.set(event_target_checked(&ev))
                />
                " include closed"
            </label>
        </section>
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
                view! {
                    <li class="task-row">
                        <span class=status_class>{t.status}</span>
                        <span class="task-owner">{t.owner}</span>
                        <span class="task-title">
                            {t.title}
                            <span class="task-meta">{project}" "{priority}</span>
                        </span>
                    </li>
                }
            }).collect_view()}
        </ul>
    }.into_any()
}
