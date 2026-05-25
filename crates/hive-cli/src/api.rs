//! hive-api HTTP client + network-aware URL resolver.
//!
//! The CLI is a thin client over hive-api (the API is the source of truth;
//! every consumer hits it). No database access here ... all state lives behind
//! the HTTP boundary, matching python `~/.hive/hive.py`'s HTTP-only design.
//!
//! Resolution order (mirrors `crates/hive-ui/src/api.rs` exactly):
//!   1. `HIVE_API_URL` ... explicit override wins outright.
//!   2. `HIVE_PRIVATE_URL` ... used iff system DNS search-domain list
//!      intersects `HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS`.
//!   3. `HIVE_PUBLIC_URL` ... fallback when off-LAN.
//!   4. `http://localhost:7878` ... last-resort default.

use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An entity id as it arrives from hive-api. Schema-agnostic on purpose: the
/// canonical schema uses UUIDv7 (`hive_db::types`), but a deployed server may
/// still be on the legacy BIGSERIAL integer PKs. This deserializes from either
/// a JSON integer or a string and renders the same either way, so the CLI works
/// against both without betting on which migration the server has applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Id(pub String);

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept int, string, or null (null -> empty, shouldn't happen for a PK).
        let v = Value::deserialize(d)?;
        let s = match v {
            Value::String(s) => s,
            Value::Number(n) => n.to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        Ok(Id(s))
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------- wire types (deserialize hive-api JSON) ----------
//
// Timestamps arrive as ISO-8601 strings over the wire (TIMESTAMPTZ); `due`
// and `entry_date` are TEXT (YYYY-MM-DD). Modeled as Option<String> so a null
// or absent field doesn't fail deserialization.

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub owner: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Task {
    pub id: Id,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JournalEntry {
    pub id: Id,
    pub ai: String,
    pub entry_date: String,
    pub title: Option<String>,
    #[serde(default)]
    pub body: String,
    pub tags: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Note {
    pub id: Id,
    pub author: String,
    pub title: Option<String>,
    #[serde(default)]
    pub body: String,
    pub tags: Option<String>,
    pub project: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireEvent {
    pub id: Id,
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

/// A `links` row as returned by `/links` and `/links/incoming`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Link {
    pub id: Id,
    pub source_table: String,
    pub source_id: Id,
    pub target_table: String,
    pub target_id: Id,
    pub link_type: Option<String>,
    pub note: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LinkTypeCount {
    pub link_type: String,
    pub count: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JournalHit {
    pub id: Id,
    pub ai: String,
    pub entry_date: String,
    pub title: Option<String>,
    pub tags: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NoteHit {
    pub id: Id,
    pub author: String,
    pub project: Option<String>,
    pub title: Option<String>,
    pub tags: Option<String>,
    pub snippet: String,
}

/// `/search` combined response shape.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CombinedHits {
    #[serde(default)]
    pub journal: Vec<JournalHit>,
    #[serde(default)]
    pub notes: Vec<NoteHit>,
}

// ---------- URL resolver (copied from hive-ui/src/api.rs) ----------
//
// hive-core is charter-bound to pure DTOs with "no platform code"; this
// resolver shells out to read DNS search domains, so it lives here rather
// than being hoisted into hive-core. Keep it byte-identical to
// hive-ui/src/api.rs so both clients resolve the same base URL.

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
        let public = std::env::var("HIVE_PUBLIC_URL").ok().filter(|s| !s.is_empty());
        let private = std::env::var("HIVE_PRIVATE_URL").ok().filter(|s| !s.is_empty());
        let awareness: HashSet<String> =
            std::env::var("HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
        if let Some(p) = private.as_ref() {
            if !awareness.is_empty() {
                let domains: HashSet<_> = system_search_domains().into_iter().collect();
                if !domains.is_disjoint(&awareness) {
                    return p.trim_end_matches('/').to_string();
                }
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
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build reqwest client")
    })
}

/// Minimal percent-encoding for query values (filters are short alnum/dash
/// tokens, but tags/queries can carry spaces or commas).
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

fn build_url(path: &str, params: &[(&str, String)]) -> String {
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
    url
}

/// Pull a human-readable error message out of hive-api's `{error, code}` body,
/// falling back to the raw text / status when it isn't the expected shape.
fn error_message(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(msg) = v.get("error").and_then(|e| e.as_str()) {
            return msg.to_string();
        }
    }
    if body.trim().is_empty() {
        format!("hive-api returned {status}")
    } else {
        format!("hive-api returned {status}: {body}")
    }
}

async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> anyhow::Result<T> {
    let resp = http_client().get(url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{}", error_message(status, &text));
    }
    Ok(serde_json::from_str(&text)?)
}

async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
    url: &str,
    body: &B,
) -> anyhow::Result<T> {
    let resp = http_client().post(url).json(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{}", error_message(status, &text));
    }
    Ok(serde_json::from_str(&text)?)
}

async fn patch_json<B: Serialize, T: serde::de::DeserializeOwned>(
    url: &str,
    body: &B,
) -> anyhow::Result<T> {
    let resp = http_client().patch(url).json(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{}", error_message(status, &text));
    }
    Ok(serde_json::from_str(&text)?)
}

async fn delete_json<T: serde::de::DeserializeOwned>(url: &str) -> anyhow::Result<T> {
    let resp = http_client().delete(url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{}", error_message(status, &text));
    }
    Ok(serde_json::from_str(&text)?)
}

// ---------- health ----------

pub async fn healthz() -> anyhow::Result<Value> {
    get_json(&format!("{}/healthz", api_base())).await
}

// ---------- projects ----------

pub async fn list_projects(status: Option<&str>) -> anyhow::Result<Vec<Project>> {
    let url = build_url("/projects", &[("status", opt(status))]);
    get_json(&url).await
}

#[derive(Serialize)]
struct ProjectAddBody<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    owner: &'a str,
}

pub async fn add_project(
    name: &str,
    description: Option<&str>,
    owner: &str,
) -> anyhow::Result<Project> {
    let url = format!("{}/projects", api_base());
    post_json(&url, &ProjectAddBody { name, description, owner }).await
}

pub async fn archive_project(name: &str) -> anyhow::Result<Value> {
    let url = format!("{}/projects/{}/archive", api_base(), urlencode(name));
    post_json(&url, &Value::Null).await
}

// ---------- tasks ----------

pub async fn list_tasks(
    project: Option<&str>,
    owner: Option<&str>,
    status: Option<&str>,
    all: bool,
) -> anyhow::Result<Vec<Task>> {
    let url = build_url(
        "/tasks",
        &[
            ("project", opt(project)),
            ("owner", opt(owner)),
            ("status", opt(status)),
            ("all", flag(all)),
        ],
    );
    get_json(&url).await
}

#[derive(Serialize)]
struct TaskAddBody<'a> {
    project: &'a str,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
    owner: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub async fn add_task(
    project: &str,
    title: &str,
    body: Option<&str>,
    owner: &str,
    priority: Option<&str>,
    due: Option<&str>,
) -> anyhow::Result<Task> {
    let url = format!("{}/tasks", api_base());
    post_json(
        &url,
        &TaskAddBody { project, title, body, owner, priority, due },
    )
    .await
}

pub async fn show_task(id: &str) -> anyhow::Result<Task> {
    let url = format!("{}/tasks/{}", api_base(), id);
    get_json(&url).await
}

pub async fn update_task(id: &str, body: &Value) -> anyhow::Result<Task> {
    let url = format!("{}/tasks/{}", api_base(), id);
    patch_json(&url, body).await
}

pub async fn task_done(id: &str) -> anyhow::Result<Task> {
    let url = format!("{}/tasks/{}/done", api_base(), id);
    post_json(&url, &Value::Null).await
}

#[derive(Serialize)]
struct BlockBody<'a> {
    reason: &'a str,
}

pub async fn task_block(id: &str, reason: &str) -> anyhow::Result<Task> {
    let url = format!("{}/tasks/{}/block", api_base(), id);
    post_json(&url, &BlockBody { reason }).await
}

pub async fn task_drop(id: &str) -> anyhow::Result<Task> {
    let url = format!("{}/tasks/{}/drop", api_base(), id);
    post_json(&url, &Value::Null).await
}

// ---------- journal ----------

pub async fn list_journal(
    ai: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    tag: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<JournalEntry>> {
    let url = build_url(
        "/journal",
        &[
            ("ai", opt(ai)),
            ("from", opt(from)),
            ("to", opt(to)),
            ("tag", opt(tag)),
            ("limit", limit.to_string()),
        ],
    );
    get_json(&url).await
}

#[derive(Serialize)]
struct JournalAddBody<'a> {
    ai: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    body: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<&'a str>,
}

pub async fn add_journal(
    ai: &str,
    date: Option<&str>,
    title: Option<&str>,
    body: &str,
    tags: Option<&str>,
) -> anyhow::Result<JournalEntry> {
    let url = format!("{}/journal", api_base());
    post_json(&url, &JournalAddBody { ai, date, title, body, tags }).await
}

pub async fn show_journal(id: &str) -> anyhow::Result<JournalEntry> {
    let url = format!("{}/journal/{}", api_base(), id);
    get_json(&url).await
}

pub async fn search_journal(query: &str, limit: i64) -> anyhow::Result<Vec<JournalHit>> {
    let url = build_url(
        "/journal/search",
        &[("q", query.to_string()), ("limit", limit.to_string())],
    );
    get_json(&url).await
}

// ---------- notes ----------

pub async fn list_notes(
    author: Option<&str>,
    project: Option<&str>,
    tag: Option<&str>,
    limit: Option<i64>,
) -> anyhow::Result<Vec<Note>> {
    let url = build_url(
        "/notes",
        &[
            ("author", opt(author)),
            ("project", opt(project)),
            ("tag", opt(tag)),
            ("limit", limit.map(|l| l.to_string()).unwrap_or_default()),
        ],
    );
    get_json(&url).await
}

#[derive(Serialize)]
struct NoteAddBody<'a> {
    author: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    body: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<&'a str>,
}

pub async fn add_note(
    author: &str,
    title: Option<&str>,
    body: &str,
    project: Option<&str>,
    tags: Option<&str>,
) -> anyhow::Result<Note> {
    let url = format!("{}/notes", api_base());
    post_json(&url, &NoteAddBody { author, title, body, project, tags }).await
}

pub async fn show_note(id: &str) -> anyhow::Result<Note> {
    let url = format!("{}/notes/{}", api_base(), id);
    get_json(&url).await
}

pub async fn search_notes(query: &str, limit: i64) -> anyhow::Result<Vec<NoteHit>> {
    let url = build_url(
        "/notes/search",
        &[("q", query.to_string()), ("limit", limit.to_string())],
    );
    get_json(&url).await
}

// ---------- wire ----------

pub async fn list_wire(
    source: Option<&str>,
    severity: Option<&str>,
    unacknowledged: bool,
    limit: i64,
) -> anyhow::Result<Vec<WireEvent>> {
    let url = build_url(
        "/wire",
        &[
            ("source", opt(source)),
            ("severity", opt(severity)),
            ("unacknowledged", flag(unacknowledged)),
            ("limit", limit.to_string()),
        ],
    );
    get_json(&url).await
}

#[derive(Serialize)]
struct WireAddBody<'a> {
    source: &'a str,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    affects: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'a str>,
}

/// `/wire` POST returns `{"added": <event>}` or `{"already_seen": {"id": ...}}`.
#[allow(clippy::too_many_arguments)]
pub async fn add_wire(
    source: &str,
    title: &str,
    body: Option<&str>,
    external_id: Option<&str>,
    severity: Option<&str>,
    affects: Option<&str>,
    url_field: Option<&str>,
    category: Option<&str>,
) -> anyhow::Result<Value> {
    let url = format!("{}/wire", api_base());
    post_json(
        &url,
        &WireAddBody {
            source,
            title,
            body,
            external_id,
            severity,
            affects,
            url: url_field,
            category,
        },
    )
    .await
}

pub async fn ack_wire(id: &str) -> anyhow::Result<Value> {
    let url = format!("{}/wire/{}/ack", api_base(), id);
    post_json(&url, &Value::Null).await
}

// ---------- links ----------

pub async fn links_outgoing(source: &str) -> anyhow::Result<Vec<Link>> {
    let url = build_url("/links", &[("source", source.to_string())]);
    get_json(&url).await
}

pub async fn links_incoming(target: &str) -> anyhow::Result<Vec<Link>> {
    let url = build_url("/links/incoming", &[("target", target.to_string())]);
    get_json(&url).await
}

#[derive(Serialize)]
struct LinkAddBody<'a> {
    source: &'a str,
    target: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
}

/// `/links` POST returns `{"id": <uuid>}` on success, 409 on duplicate.
pub async fn add_link(
    source: &str,
    target: &str,
    link_type: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<Value> {
    let url = format!("{}/links", api_base());
    post_json(&url, &LinkAddBody { source, target, link_type, note }).await
}

pub async fn remove_link(id: &str) -> anyhow::Result<Value> {
    let url = format!("{}/links/{}", api_base(), id);
    delete_json(&url).await
}

pub async fn link_types() -> anyhow::Result<Vec<LinkTypeCount>> {
    let url = format!("{}/links/types", api_base());
    get_json(&url).await
}

// ---------- graph + search ----------

pub async fn graph(
    min: i64,
    tags: i64,
    nodes: i64,
    include_meta: bool,
) -> anyhow::Result<Value> {
    let url = build_url(
        "/graph",
        &[
            ("min", min.to_string()),
            ("tags", tags.to_string()),
            ("nodes", nodes.to_string()),
            ("include_meta", flag(include_meta)),
        ],
    );
    get_json(&url).await
}

pub async fn search(query: &str, limit: i64) -> anyhow::Result<CombinedHits> {
    let url = build_url(
        "/search",
        &[("q", query.to_string()), ("limit", limit.to_string())],
    );
    get_json(&url).await
}

// ---------- helpers ----------

fn opt(v: Option<&str>) -> String {
    v.unwrap_or("").to_string()
}

fn flag(b: bool) -> String {
    if b { "true".to_string() } else { String::new() }
}
