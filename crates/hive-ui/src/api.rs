//! hive-api HTTP client + network-aware URL resolver.
//!
//! Resolution order (matches the deleted slint client and python hive.py):
//!   1. `HIVE_API_URL` ... explicit override wins outright.
//!   2. `HIVE_PRIVATE_URL` ... used iff system DNS search-domain list
//!      intersects `HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS`.
//!   3. `HIVE_PUBLIC_URL` ... fallback when off-LAN.
//!   4. `http://localhost:7878` ... last-resort default.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The current request's session id, provided into Leptos context by `App`
/// (read from the `hive_ui_session` cookie). `None` => not logged in; fetches
/// then go out tokenless (accepted by the warn-only server, 401 under enforce).
#[derive(Debug, Clone, Default)]
pub struct SessionId(pub Option<String>);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JournalEntry {
    pub id: Uuid,
    pub ai: String,
    pub entry_date: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub body: String,
    pub tags: Option<String>,
    pub created_at: Option<String>,
}

/// Mirrors `hive_db::types::Task` (TIMESTAMPTZ fields arrive as ISO-8601
/// strings over the wire; `due` is TEXT YYYY-MM-DD).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Task {
    pub id: Uuid,
    pub project: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub owner: String,
    pub status: String,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub block_reason: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub closed_at: Option<String>,
}

/// Mirrors `hive_db::types::Note`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Note {
    pub id: Uuid,
    pub author: String,
    pub title: Option<String>,
    #[serde(default)]
    pub body: String,
    pub tags: Option<String>,
    pub project: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Mirrors `hive_db::types::Link`. Used by the entry detail page to
/// surface outgoing mentions + incoming backlinks as sidecars beneath
/// the prose.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Link {
    pub id: Uuid,
    pub source_table: String,
    pub source_id: Uuid,
    pub target_table: String,
    pub target_id: Uuid,
    #[serde(default)]
    pub link_type: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    // Enrichment fields the API MAY add when known (target/source titles
    // joined in). Optional so we degrade gracefully if the link-pipeline
    // agent hasn't shipped enrichment yet.
    #[serde(default)]
    pub target_title: Option<String>,
    #[serde(default)]
    pub target_slug: Option<String>,
    #[serde(default)]
    pub source_title: Option<String>,
    #[serde(default)]
    pub source_slug: Option<String>,
}

/// Mirrors `hive_db::types::Person`. Humans only after migration 0013 ...
/// AIs moved out to their own table, surfaced via `Ai` below.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Person {
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Mirrors `hive_db::types::Ai`. Directory side of AIs (Pia, Apis, Cera).
/// Distinct from the auth-side `ai_identities` table (MCP grants). The
/// `kind` here is `assistant`, `agent`, or `persona` ... independent of
/// the human/AI top-level dichotomy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ai {
    pub id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub kind: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Mirrors `hive_db::types::Event`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    pub id: Uuid,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub starts_at: String,
    #[serde(default)]
    pub ends_at: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Mirrors `hive_db::types::WireEvent`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireEvent {
    pub id: Uuid,
    pub source: String,
    pub category: Option<String>,
    pub external_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub url: Option<String>,
    pub severity: Option<String>,
    pub affects: Option<String>,
    #[serde(default)]
    pub acknowledged: bool,
    #[serde(default)]
    pub pinged_discord: bool,
    pub first_seen_at: Option<String>,
    pub last_seen_at: Option<String>,
}

fn system_search_domains() -> Vec<String> {
    use std::process::Command;
    let raw: String = if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "$g = (Get-DnsClientGlobalSetting).SuffixSearchList; $c = (Get-DnsClient | Where-Object { $_.ConnectionSpecificSuffix }).ConnectionSpecificSuffix; ($g + $c) -join ','",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        Command::new("scutil")
            .arg("--dns")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.to_lowercase().contains("search domain"))
                    .filter_map(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    } else {
        std::fs::read_to_string("/etc/resolv.conf")
            .ok()
            .map(|s| {
                s.lines()
                    .filter(|l| l.starts_with("search "))
                    .flat_map(|l| l[7..].split_whitespace().map(|d| d.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn api_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        if let Ok(url) = std::env::var("HIVE_API_URL") {
            let u = url.trim();
            if !u.is_empty() {
                return u.trim_end_matches('/').to_string();
            }
        }
        let public = std::env::var("HIVE_PUBLIC_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let private = std::env::var("HIVE_PRIVATE_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let awareness: HashSet<String> =
            std::env::var("HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
        if let Some(p) = private.as_ref()
            && !awareness.is_empty()
        {
            let domains: HashSet<_> = system_search_domains().into_iter().collect();
            if !domains.is_disjoint(&awareness) {
                return p.trim_end_matches('/').to_string();
            }
        }
        if let Some(p) = public {
            return p.trim_end_matches('/').to_string();
        }
        "http://localhost:7878".to_string()
    })
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client")
    })
}

/// The session id for the current SSR render, from Leptos context. `None` when
/// no `App`-provided context exists (e.g. in unit tests) or the user isn't
/// logged in.
fn current_session() -> Option<String> {
    leptos::prelude::use_context::<SessionId>().and_then(|s| s.0)
}

/// GET a hive-api list endpoint, deserializing the JSON array. The query
/// string is built from `(key, value)` pairs, skipping any with empty value.
///
/// Auth (Phase 3, §3.1): attaches the session's bearer token when logged in.
/// On a 401 (access token expired/revoked under enforce mode) it rotates the
/// refresh token once and retries — transparent token refresh.
async fn fetch_list<T: serde::de::DeserializeOwned>(
    path: &str,
    params: &[(&str, &str)],
) -> anyhow::Result<Vec<T>> {
    let mut url = format!("{}{}", api_base(), path);
    let query: Vec<String> = params
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect();
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query.join("&"));
    }

    let session = current_session();
    let token = session.as_deref().and_then(crate::auth::access_token_for);

    let resp = send_get(&url, token.as_deref()).await?;
    let status = resp.status();

    // 401 + a live session => try one refresh, then retry once.
    if status == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(sid) = session.as_deref()
            && let Some(fresh) = crate::auth::refresh(sid).await
        {
            let resp = send_get(&url, Some(&fresh)).await?;
            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!("GET {url} returned {status} (after refresh)");
            }
            return Ok(resp.json().await?);
        }
        anyhow::bail!("GET {url} returned 401 (not authenticated — please log in)");
    }

    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}");
    }
    let rows: Vec<T> = resp.json().await?;
    Ok(rows)
}

/// Issue a GET, attaching the bearer token when present.
async fn send_get(url: &str, token: Option<&str>) -> anyhow::Result<reqwest::Response> {
    let mut req = http_client().get(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    Ok(req.send().await?)
}

/// Minimal percent-encoding for query values (filters are short alnum/dash
/// tokens, but tags can carry spaces or commas).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// GET a hive-api endpoint that returns a single JSON object. Same auth +
/// transparent-refresh shape as `fetch_list`.
async fn fetch_one<T: serde::de::DeserializeOwned>(path: &str) -> anyhow::Result<T> {
    let url = format!("{}{}", api_base(), path);

    let session = current_session();
    let token = session.as_deref().and_then(crate::auth::access_token_for);

    let resp = send_get(&url, token.as_deref()).await?;
    let status = resp.status();

    if status == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(sid) = session.as_deref()
            && let Some(fresh) = crate::auth::refresh(sid).await
        {
            let resp = send_get(&url, Some(&fresh)).await?;
            let status = resp.status();
            if !status.is_success() {
                anyhow::bail!("GET {url} returned {status} (after refresh)");
            }
            return Ok(resp.json().await?);
        }
        anyhow::bail!("GET {url} returned 401 (not authenticated — please log in)");
    }

    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}");
    }
    Ok(resp.json().await?)
}

/// Unfiltered journal fetch. Kept for parity with `fetch_journal_filtered`
/// and future call sites. Hidden behind allow(dead_code) because no current
/// page uses it; remove the attr when one does.
#[allow(dead_code)]
pub async fn fetch_journal(limit: i64) -> anyhow::Result<Vec<JournalEntry>> {
    fetch_list("/journal", &[("limit", &limit.to_string())]).await
}

/// Fetch a single journal entry by its id.
pub async fn fetch_journal_entry(id: &str) -> anyhow::Result<JournalEntry> {
    fetch_one(&format!("/journal/{id}")).await
}

/// Filtered journal list. `ai` and `tag` are optional (empty = unfiltered).
pub async fn fetch_journal_filtered(
    ai: &str,
    tag: &str,
    limit: i64,
) -> anyhow::Result<Vec<JournalEntry>> {
    fetch_list(
        "/journal",
        &[("ai", ai), ("tag", tag), ("limit", &limit.to_string())],
    )
    .await
}

/// Task list. `owner` and `status` are optional; `all=true` includes closed.
pub async fn fetch_tasks(owner: &str, status: &str, all: bool) -> anyhow::Result<Vec<Task>> {
    fetch_list(
        "/tasks",
        &[
            ("owner", owner),
            ("status", status),
            ("all", if all { "true" } else { "" }),
        ],
    )
    .await
}

/// Note list. `author` and `tag` are optional.
pub async fn fetch_notes(author: &str, tag: &str, limit: i64) -> anyhow::Result<Vec<Note>> {
    fetch_list(
        "/notes",
        &[
            ("author", author),
            ("tag", tag),
            ("limit", &limit.to_string()),
        ],
    )
    .await
}

/// FTS5 journal search. Hits `GET /journal/search?q=...&limit=...` and remaps
/// the snippet-bearing `JournalHit` rows into `JournalEntry` so the same
/// `EntryArticle` visual can render them. The snippet's `[..]` highlight
/// markers are converted to `<mark>..</mark>` and inlined into `body`;
/// pulldown_cmark passes the raw HTML through on the trusted-content path.
pub async fn fetch_journal_search(q: &str, limit: i64) -> anyhow::Result<Vec<JournalEntry>> {
    #[derive(Debug, Deserialize)]
    struct JournalHit {
        id: Uuid,
        ai: String,
        entry_date: String,
        title: Option<String>,
        tags: Option<String>,
        snippet: String,
    }

    let hits: Vec<JournalHit> = fetch_list(
        "/journal/search",
        &[("q", q), ("limit", &limit.to_string())],
    )
    .await?;
    Ok(hits
        .into_iter()
        .map(|h| JournalEntry {
            id: h.id,
            ai: h.ai,
            entry_date: Some(h.entry_date),
            title: h.title,
            body: snippet_to_html(&h.snippet),
            tags: h.tags,
            created_at: None,
        })
        .collect())
}

/// Convert the FTS5 snippet's `[term]` highlight markers into `<mark>term</mark>`.
/// Everything else passes through verbatim; the markdown renderer treats the
/// result as inline HTML (trusted-content path).
fn snippet_to_html(snippet: &str) -> String {
    let mut out = String::with_capacity(snippet.len() + 16);
    let mut in_mark = false;
    for ch in snippet.chars() {
        match ch {
            '[' if !in_mark => {
                out.push_str("<mark>");
                in_mark = true;
            }
            ']' if in_mark => {
                out.push_str("</mark>");
                in_mark = false;
            }
            _ => out.push(ch),
        }
    }
    if in_mark {
        out.push_str("</mark>");
    }
    out
}

/// Wire-event list. `source` and `severity` are optional; `unacknowledged=true`
/// hides acked events.
pub async fn fetch_wire(
    source: &str,
    severity: &str,
    unacknowledged: bool,
    limit: i64,
) -> anyhow::Result<Vec<WireEvent>> {
    fetch_list(
        "/wire",
        &[
            ("source", source),
            ("severity", severity),
            ("unacknowledged", if unacknowledged { "true" } else { "" }),
            ("limit", &limit.to_string()),
        ],
    )
    .await
}

/// Outgoing links from the given source entity. The hive-api `/links`
/// endpoint expects a single `source=<table>:<id>` query param. Returns
/// the rows AS-IS; the caller groups by `target_table` for the sidecar.
///
/// Degrades to `Ok(vec![])` on any error ... the sidecar is secondary
/// chrome and shouldn't block the page render.
pub async fn fetch_links_outgoing(source_table: &str, source_id: &str) -> Vec<Link> {
    let source = format!("{source_table}:{source_id}");
    match fetch_list::<Link>("/links", &[("source", &source)]).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(%err, "fetch_links_outgoing failed; rendering empty");
            Vec::new()
        }
    }
}

/// Incoming links to the given target entity (i.e. backlinks).
///
/// Degrades to `Ok(vec![])` on any error.
pub async fn fetch_links_incoming(target_table: &str, target_id: &str) -> Vec<Link> {
    let target = format!("{target_table}:{target_id}");
    match fetch_list::<Link>("/links/incoming", &[("target", &target)]).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::debug!(%err, "fetch_links_incoming failed; rendering empty");
            Vec::new()
        }
    }
}

/// Fetch a single task by id or slug. The hive-api `/tasks/{id_or_slug}`
/// endpoint accepts either ... the slug-fallback is what makes the
/// `[[task:slug]]` mention click land on a real row.
pub async fn fetch_task_by_slug(id_or_slug: &str) -> anyhow::Result<Task> {
    fetch_one(&format!("/tasks/{id_or_slug}")).await
}

/// Fetch a single note by id or slug.
pub async fn fetch_note_by_slug(id_or_slug: &str) -> anyhow::Result<Note> {
    fetch_one(&format!("/notes/{id_or_slug}")).await
}

/// Fetch a single event by id or slug.
pub async fn fetch_event_by_slug(id_or_slug: &str) -> anyhow::Result<Event> {
    fetch_one(&format!("/events/{id_or_slug}")).await
}

/// Fetch the humans directory. (AIs live at `/ai` ... see `fetch_ai_list`.)
pub async fn fetch_people() -> anyhow::Result<Vec<Person>> {
    fetch_list("/people", &[]).await
}

/// Fetch a single human by slug (or uuid).
pub async fn fetch_person(slug: &str) -> anyhow::Result<Person> {
    fetch_one(&format!("/people/{slug}")).await
}

/// Fetch the AI directory (assistants, agents, personas).
pub async fn fetch_ai_list() -> anyhow::Result<Vec<Ai>> {
    fetch_list("/ai", &[]).await
}

/// Fetch a single AI by slug (or uuid).
pub async fn fetch_ai_by_slug(slug: &str) -> anyhow::Result<Ai> {
    fetch_one(&format!("/ai/{slug}")).await
}

/// Event list (optionally tag-filtered).
pub async fn fetch_events(tag: &str, limit: i64) -> anyhow::Result<Vec<Event>> {
    fetch_list("/events", &[("tag", tag), ("limit", &limit.to_string())]).await
}

/// Tasks extracted from the given journal entry via task_anchors.
///
/// TODO(parallel-agent): the hive-api doesn't expose task_anchors yet.
/// Expected endpoint: `GET /journal/:id/tasks` returns `Vec<Task>` for
/// any rows in `task_anchors` that point at this entry. Until that
/// lands, return empty and the sidecar shows empty-state.
pub async fn fetch_task_anchors(_journal_entry_id: &str) -> Vec<Task> {
    Vec::new()
}
