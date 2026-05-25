//! Graph view. v1 ships a tabular tag-hub render of the same payload the
//! python+svelte UI uses for its d3 force layout. Interactive d3 render
//! is the natural place WASM hydration earns its keep, and stays as a
//! follow-up.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use maud::{Markup, html};
use serde::Deserialize;

use hive_db::queries::graph::{self, GraphOptions};

use super::layout::page;
use super::{AppState, with_conn};

#[derive(Debug, Deserialize, Default)]
pub struct Q {
    pub min: Option<i64>,
    pub tags: Option<i64>,
    pub nodes: Option<i64>,
    #[serde(default)]
    pub include_meta: bool,
}

pub async fn view(
    State(state): State<AppState>,
    Query(q): Query<Q>,
) -> Result<Markup, StatusCode> {
    let opts = GraphOptions {
        min_tag_count: q.min.unwrap_or(2),
        limit_tags: q.tags.unwrap_or(80),
        limit_nodes: q.nodes.unwrap_or(600),
        include_meta: q.include_meta,
    };
    let opts_for_query = opts.clone();
    let payload = with_conn(&state, move |c| graph::build(c, opts_for_query)).await?;

    // Group leaves under their tag for the table render.
    use std::collections::BTreeMap;
    let mut by_tag: BTreeMap<String, Vec<&graph::GraphNode>> = BTreeMap::new();
    let mut tag_sizes: BTreeMap<String, i64> = BTreeMap::new();
    for n in &payload.nodes {
        if n.kind == "tag" {
            tag_sizes.insert(n.label.clone(), n.size);
        }
    }
    for l in &payload.links {
        let tag = l.source.strip_prefix("tag:").unwrap_or(&l.source).to_string();
        if let Some(target_node) = payload.nodes.iter().find(|n| n.id == l.target) {
            by_tag.entry(tag).or_default().push(target_node);
        }
    }

    let body = html! {
        h2 { "knowledge graph" }
        form.inline action="/graph" method="get" {
            label { "min: " input type="number" name="min" value=(opts.min_tag_count) style="width:5ch;"; }
            label { "tags: " input type="number" name="tags" value=(opts.limit_tags) style="width:6ch;"; }
            label { "nodes: " input type="number" name="nodes" value=(opts.limit_nodes) style="width:6ch;"; }
            label { input type="checkbox" name="include_meta" value="true" checked[opts.include_meta]; " include meta tags" }
            button type="submit" { "rebuild" }
        }
        p.muted style="margin-top: 0.6rem;" {
            (payload.stats.tag_count) " tag hubs from "
            (payload.stats.journal_count) " journal entries and "
            (payload.stats.note_count) " notes."
        }

        @if by_tag.is_empty() {
            div.empty { "no tags meet the threshold" }
        } @else {
            @for (tag, members) in &by_tag {
                section style="margin-bottom: 1.2rem;" {
                    h2 style="font-size:0.95rem; border-bottom:1px solid var(--border); padding-bottom:0.2rem;" {
                        "#" (tag) " " span.muted { "(" (tag_sizes.get(tag).copied().unwrap_or(0)) ")" }
                    }
                    ul style="margin: 0.3rem 0 0 1.2rem; padding: 0;" {
                        @for m in members {
                            li style="font-size: 0.85rem;" {
                                span.row-id { (m.kind) " #" (m.ref_id.unwrap_or(0)) ": " }
                                (m.label)
                            }
                        }
                    }
                }
            }
        }

        details style="margin-top: 1.5rem;" {
            summary.muted { "raw graph payload (json)" }
            pre.pre { (serde_json::to_string_pretty(&payload).unwrap_or_default()) }
        }
    };

    Ok(page("graph", "graph", body))
}
