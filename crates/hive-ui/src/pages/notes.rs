use axum::extract::{Query, State};
use axum::http::StatusCode;
use maud::{Markup, html};
use serde::Deserialize;

use hive_db::enums::Author;
use hive_db::queries::notes;

use super::layout::page;
use super::{AppState, with_conn};

#[derive(Debug, Deserialize, Default)]
pub struct Q {
    pub author: Option<Author>,
    pub project: Option<String>,
    pub tag: Option<String>,
}

pub async fn view(
    State(state): State<AppState>,
    Query(q): Query<Q>,
) -> Result<Markup, StatusCode> {
    let filters = notes::ListFilters {
        author: q.author,
        project: q.project.clone(),
        tag: q.tag.clone(),
        limit: Some(100),
    };
    let rows = with_conn(&state, move |c| notes::list(c, &filters)).await?;

    let body = html! {
        h2 { "notes " span.muted { "(" (rows.len()) ")" } }
        @if rows.is_empty() {
            div.empty { "no notes" }
        } @else {
            table {
                thead { tr { th { "id" } th { "author" } th { "project" } th { "title" } th { "tags" } } }
                tbody {
                    @for r in &rows {
                        tr {
                            td.row-id { "#" (r.id) }
                            td { (r.author) }
                            td { (r.project.clone().unwrap_or_default()) }
                            td { (r.title.clone().unwrap_or_default()) }
                            td.tags { (r.tags.clone().unwrap_or_default()) }
                        }
                    }
                }
            }
        }
    };
    Ok(page("notes", "notes", body))
}
