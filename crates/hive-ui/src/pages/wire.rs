use axum::extract::{Query, State};
use axum::http::StatusCode;
use maud::{Markup, html};
use serde::Deserialize;

use hive_db::enums::Severity;
use hive_db::queries::wire;

use super::layout::page;
use super::{AppState, with_conn};

#[derive(Debug, Deserialize, Default)]
pub struct Q {
    pub source: Option<String>,
    pub severity: Option<Severity>,
    #[serde(default)]
    pub unack: bool,
}

pub async fn view(
    State(state): State<AppState>,
    Query(q): Query<Q>,
) -> Result<Markup, StatusCode> {
    let filters = wire::ListFilters {
        source: q.source.clone(),
        severity: q.severity,
        unacknowledged: q.unack,
        limit: Some(100),
    };
    let rows = with_conn(&state, move |c| wire::list(c, &filters)).await?;

    let body = html! {
        h2 { "wire events " span.muted { "(" (rows.len()) ")" } }
        @if rows.is_empty() {
            div.empty { "no wire events" }
        } @else {
            table {
                thead { tr { th { "id" } th { "source" } th { "sev" } th { "ack" } th { "affects" } th { "title" } } }
                tbody {
                    @for r in &rows {
                        tr {
                            td.row-id { "#" (r.id) }
                            td { (r.source) }
                            td { (r.severity.clone().unwrap_or_default()) }
                            td.muted { @if r.acknowledged { "yes" } @else { "no" } }
                            td.muted { (r.affects.clone().unwrap_or_default()) }
                            td {
                                @if let Some(url) = &r.url {
                                    a href=(url) target="_blank" rel="noopener" { (r.title) }
                                } @else {
                                    (r.title)
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(page("wire", "wire", body))
}
