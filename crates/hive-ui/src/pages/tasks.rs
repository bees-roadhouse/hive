use axum::extract::{Query, State};
use axum::http::StatusCode;
use chrono::Local;
use maud::{Markup, html};
use serde::Deserialize;

use hive_db::enums::{Owner, TaskStatus};
use hive_db::queries::tasks;

use super::layout::page;
use super::{AppState, with_conn};

#[derive(Debug, Deserialize, Default)]
pub struct Q {
    pub project: Option<String>,
    pub owner: Option<Owner>,
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub all: bool,
}

pub async fn view(
    State(state): State<AppState>,
    Query(q): Query<Q>,
) -> Result<Markup, StatusCode> {
    let filters = tasks::ListFilters {
        project: q.project.clone(),
        owner: q.owner,
        status: q.status,
        all: q.all,
    };
    let rows = with_conn(&state, move |c| tasks::list(c, &filters)).await?;
    let today = Local::now().date_naive().format("%Y-%m-%d").to_string();

    let body = html! {
        h2 { "tasks " span.muted { "(" (rows.len()) ")" } }
        form.inline action="/tasks" method="get" {
            label { "owner: " }
            select name="owner" {
                option value="" { "(any)" }
                @for o in Owner::ALL {
                    option value=(o.as_str())
                           selected[q.owner.map(|v| v == *o).unwrap_or(false)]
                    { (o.as_str()) }
                }
            }
            label { "status: " }
            select name="status" {
                option value="" { "(active)" }
                @for s in TaskStatus::ALL {
                    option value=(s.as_str())
                           selected[q.status.map(|v| v == *s).unwrap_or(false)]
                    { (s.as_str()) }
                }
            }
            label { input type="checkbox" name="all" value="true" checked[q.all]; " include closed" }
            button type="submit" { "filter" }
        }

        @if rows.is_empty() {
            div.empty { "no tasks match the filter" }
        } @else {
            table {
                thead {
                    tr { th { "id" } th { "project" } th { "owner" } th { "status" } th { "pri" } th { "due" } th { "title" } }
                }
                tbody {
                    @for r in &rows {
                        tr {
                            td.row-id { "#" (r.id) }
                            td { (r.project) }
                            td { (r.owner) }
                            td { (r.status) }
                            td.muted { (r.priority.clone().unwrap_or_default()) }
                            td {
                                @match &r.due {
                                    Some(d) if d.as_str() < today.as_str() && r.status != "done" && r.status != "dropped" => {
                                        span.overdue { (d) " OVERDUE" }
                                    }
                                    Some(d) => { (d) }
                                    None => { "" }
                                }
                            }
                            td { (r.title) }
                        }
                    }
                }
            }
        }
    };
    Ok(page("tasks", "tasks", body))
}
