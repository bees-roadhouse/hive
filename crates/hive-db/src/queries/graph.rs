//! Tag-hub knowledge graph. Mirrors python `cmd_graph` ... emits the same
//! JSON shape consumed by hive-ui /graph.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::Result;
use crate::types::split_tags;

const META_TAGS: &[&str] = &[
    "legacy-migration",
    "nate-authored",
    "pia-authored",
    "apis-authored",
    "cera-authored",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphOptions {
    pub min_tag_count: i64,
    pub limit_tags: i64,
    pub limit_nodes: i64,
    pub include_meta: bool,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            min_tag_count: 2,
            limit_tags: 80,
            limit_nodes: 600,
            include_meta: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphLink {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStats {
    pub tag_count: usize,
    pub journal_count: usize,
    pub task_count: usize,
    pub note_count: usize,
    pub wire_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPayload {
    pub nodes: Vec<GraphNode>,
    pub links: Vec<GraphLink>,
    pub stats: GraphStats,
    pub options: GraphOptions,
}

pub async fn build(pool: &PgPool, opts: GraphOptions) -> Result<GraphPayload> {
    let min_tag_count = opts.min_tag_count.max(1);
    let limit_tags = opts.limit_tags.max(1) as usize;
    let limit_nodes = opts.limit_nodes.max(limit_tags as i64) as usize;

    let journal_rows: Vec<(i64, Option<String>, String)> = sqlx::query_as(
        "SELECT id, title, tags FROM journal_entries WHERE tags IS NOT NULL AND tags != ''",
    )
    .fetch_all(pool)
    .await?;

    let note_rows: Vec<(i64, Option<String>, String)> = sqlx::query_as(
        "SELECT id, title, tags FROM notes WHERE tags IS NOT NULL AND tags != ''",
    )
    .fetch_all(pool)
    .await?;

    let meta: HashSet<&'static str> = META_TAGS.iter().copied().collect();
    let mut tag_freq: HashMap<String, i64> = HashMap::new();
    let mut tag_members: HashMap<String, Vec<(String, i64, String)>> = HashMap::new();

    let mut record = |tag: &str, kind: &str, id: i64, title: Option<&str>| {
        if !opts.include_meta && meta.contains(tag) {
            return;
        }
        *tag_freq.entry(tag.to_string()).or_insert(0) += 1;
        let label = title
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{kind} #{id}"));
        tag_members
            .entry(tag.to_string())
            .or_default()
            .push((kind.to_string(), id, label));
    };

    for (id, title, tags) in &journal_rows {
        for t in split_tags(Some(tags.as_str())) {
            record(&t, "journal", *id, title.as_deref());
        }
    }
    for (id, title, tags) in &note_rows {
        for t in split_tags(Some(tags.as_str())) {
            record(&t, "note", *id, title.as_deref());
        }
    }

    let mut top: Vec<(String, i64)> = tag_freq
        .into_iter()
        .filter(|(_, n)| *n >= min_tag_count)
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(limit_tags);

    let keep: HashSet<String> = top.iter().map(|(t, _)| t.clone()).collect();

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut links: Vec<GraphLink> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (tag, n) in &top {
        let nid = format!("tag:{tag}");
        nodes.push(GraphNode {
            id: nid.clone(),
            kind: "tag".into(),
            label: tag.clone(),
            size: *n,
            tag: Some(tag.clone()),
            ref_id: None,
        });
        seen.insert(nid);
    }

    let mut budget: i64 = (limit_nodes as i64) - (nodes.len() as i64);
    for (tag, members) in &tag_members {
        if !keep.contains(tag) {
            continue;
        }
        for (kind, id, label) in members {
            if budget <= 0 {
                break;
            }
            let nid = format!("{kind}:{id}");
            if !seen.contains(&nid) {
                nodes.push(GraphNode {
                    id: nid.clone(),
                    kind: kind.clone(),
                    label: label.clone(),
                    size: 1,
                    tag: None,
                    ref_id: Some(*id),
                });
                seen.insert(nid.clone());
                budget -= 1;
            }
            links.push(GraphLink {
                source: format!("tag:{tag}"),
                target: nid,
            });
        }
    }

    Ok(GraphPayload {
        nodes,
        links,
        stats: GraphStats {
            tag_count: top.len(),
            journal_count: journal_rows.len(),
            task_count: 0,
            note_count: note_rows.len(),
            wire_count: 0,
        },
        options: GraphOptions {
            min_tag_count,
            limit_tags: limit_tags as i64,
            limit_nodes: limit_nodes as i64,
            include_meta: opts.include_meta,
        },
    })
}

/// Cap to keep BFS bounded ... the semantic-search blanket boost shouldn't
/// fan out the whole graph.
const MARKOV_BLANKET_CAP: usize = 200;

/// BFS the `links` table from `(root_table, root_id)` up to `depth` hops in
/// both directions. Returns `(table, id)` pairs including the root. Capped at
/// `MARKOV_BLANKET_CAP` nodes to prevent runaway on hot tags.
pub async fn markov_blanket(
    pool: &PgPool,
    root_table: &str,
    root_id: i64,
    depth: u32,
) -> Result<Vec<(String, i64)>> {
    let mut visited: HashSet<(String, i64)> = HashSet::new();
    let mut order: Vec<(String, i64)> = Vec::new();
    let root = (root_table.to_string(), root_id);
    visited.insert(root.clone());
    order.push(root.clone());
    let mut frontier: Vec<(String, i64)> = vec![root];

    'outer: for _ in 0..depth {
        let mut next: Vec<(String, i64)> = Vec::new();
        for (t, i) in &frontier {
            let outs: Vec<(String, i64)> = sqlx::query_as(
                "SELECT target_table, target_id FROM links \
                 WHERE source_table = $1 AND source_id = $2",
            )
            .bind(t)
            .bind(*i)
            .fetch_all(pool)
            .await?;
            for pair in outs {
                if visited.insert(pair.clone()) {
                    order.push(pair.clone());
                    next.push(pair);
                    if visited.len() >= MARKOV_BLANKET_CAP {
                        break 'outer;
                    }
                }
            }
            let ins: Vec<(String, i64)> = sqlx::query_as(
                "SELECT source_table, source_id FROM links \
                 WHERE target_table = $1 AND target_id = $2",
            )
            .bind(t)
            .bind(*i)
            .fetch_all(pool)
            .await?;
            for pair in ins {
                if visited.insert(pair.clone()) {
                    order.push(pair.clone());
                    next.push(pair);
                    if visited.len() >= MARKOV_BLANKET_CAP {
                        break 'outer;
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    Ok(order)
}
