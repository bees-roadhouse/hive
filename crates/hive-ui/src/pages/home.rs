use axum::extract::State;
use axum::http::StatusCode;
use maud::{Markup, html};

use hive_db::queries::{journal, projects, tasks};

use super::layout::page;
use super::{AppState, with_conn};

pub async fn view(State(state): State<AppState>) -> Result<Markup, StatusCode> {
    let (open_tasks, recent_journal, project_list) = with_conn(&state, |c| {
        let open = tasks::list(c, &tasks::ListFilters { all: false, ..Default::default() })?;
        let recent = journal::list(
            c,
            &journal::ListFilters {
                limit: Some(8),
                ..Default::default()
            },
        )?;
        let projs = projects::list(c, None)?;
        Ok::<_, hive_db::Error>((open, recent, projs))
    })
    .await?;

    let body = html! {
        section {
            h2 { "open tasks" " " span.muted { "(" (open_tasks.len()) ")" } }
            @if open_tasks.is_empty() {
                div.empty { "no open tasks" }
            } @else {
                table {
                    thead { tr { th { "id" } th { "project" } th { "owner" } th { "status" } th { "title" } } }
                    tbody {
                        @for t in open_tasks.iter().take(20) {
                            tr {
                                td.row-id { "#" (t.id) }
                                td { (t.project) }
                                td { (t.owner) }
                                td { (t.status) }
                                td { (t.title) }
                            }
                        }
                    }
                }
            }
        }
        section style="margin-top: 1.5rem;" {
            h2 { "recent journal" }
            @if recent_journal.is_empty() {
                div.empty { "no journal entries" }
            } @else {
                table {
                    thead { tr { th { "id" } th { "date" } th { "ai" } th { "title" } th { "tags" } } }
                    tbody {
                        @for j in &recent_journal {
                            tr {
                                td.row-id { "#" (j.id) }
                                td { (j.entry_date) }
                                td { (j.ai) }
                                td { (j.title.clone().unwrap_or_default()) }
                                td.tags { (j.tags.clone().unwrap_or_default()) }
                            }
                        }
                    }
                }
            }
        }
        section style="margin-top: 1.5rem;" {
            h2 { "projects" }
            @if project_list.is_empty() {
                div.empty { "no projects" }
            } @else {
                table {
                    thead { tr { th { "name" } th { "owner" } th { "status" } th { "description" } } }
                    tbody {
                        @for p in &project_list {
                            tr {
                                td { (p.name) }
                                td { (p.owner) }
                                td { (p.status) }
                                td.muted { (p.description.clone().unwrap_or_default()) }
                            }
                        }
                    }
                }
            }
        }
    };

    Ok(page("home", "home", body))
}
