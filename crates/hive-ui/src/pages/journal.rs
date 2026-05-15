use axum::extract::{Query, State};
use axum::http::StatusCode;
use maud::{Markup, html};
use serde::Deserialize;

use hive_db::enums::Ai;
use hive_db::queries::journal;

use super::layout::page;
use super::{AppState, with_conn};

#[derive(Debug, Deserialize, Default)]
pub struct Q {
    pub ai: Option<Ai>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
    pub q: Option<String>,
}

pub async fn view(
    State(state): State<AppState>,
    Query(q): Query<Q>,
) -> Result<Markup, StatusCode> {
    let limit = q.limit.unwrap_or(50);
    let filters = journal::ListFilters {
        ai: q.ai,
        from_date: None,
        to_date: None,
        tag: q.tag.clone(),
        limit: Some(limit),
    };
    let q_text = q.q.clone();
    let (rows, hits) = with_conn(&state, move |c| {
        let rows = journal::list(c, &filters)?;
        let hits = match &q_text {
            Some(qq) if !qq.is_empty() => Some(hive_db::queries::search::journal(c, qq, 20)?),
            _ => None,
        };
        Ok::<_, hive_db::Error>((rows, hits))
    })
    .await?;

    let body = html! {
        h2 { "journal" }
        form.inline action="/journal" method="get" {
            label { "ai: " }
            select name="ai" {
                option value="" { "(any)" }
                @for a in Ai::ALL {
                    option value=(a.as_str())
                           selected[q.ai.map(|v| v == *a).unwrap_or(false)]
                    { (a.as_str()) }
                }
            }
            label { "tag: " input type="text" name="tag" value=(q.tag.clone().unwrap_or_default()); }
            label { "fts: " input type="text" name="q" value=(q.q.clone().unwrap_or_default()); }
            button type="submit" { "filter" }
        }

        @if let Some(hits) = &hits {
            section {
                h2 { "search hits " span.muted { "(" (hits.len()) ")" } }
                @for h in hits {
                    div style="margin-bottom: 0.6rem;" {
                        span.row-id { "#" (h.id) " " }
                        (h.entry_date) " " (h.ai) " "
                        b { (h.title.clone().unwrap_or_else(|| "(untitled)".into())) }
                        @if let Some(t) = &h.tags { " " span.tags { (t) } }
                        div.snippet {
                            (maud::PreEscaped(h.snippet.replace('[', "<b>").replace(']', "</b>")))
                        }
                    }
                }
            }
        }

        h2 style="margin-top: 1.5rem;" { "entries " span.muted { "(" (rows.len()) ")" } }
        @if rows.is_empty() {
            div.empty { "no journal entries match" }
        } @else {
            table {
                thead { tr { th { "id" } th { "date" } th { "ai" } th { "title" } th { "tags" } } }
                tbody {
                    @for r in &rows {
                        tr {
                            td.row-id { "#" (r.id) }
                            td { (r.entry_date) }
                            td { (r.ai) }
                            td { (r.title.clone().unwrap_or_default()) }
                            td.tags { (r.tags.clone().unwrap_or_default()) }
                        }
                    }
                }
            }
        }
    };
    Ok(page("journal", "journal", body))
}
