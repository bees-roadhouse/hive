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
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client")
    })
}

/// GET a hive-api list endpoint, deserializing the JSON array. The query
/// string is built from `(key, value)` pairs, skipping any with empty value.
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
    let resp = http_client().get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}");
    }
    let rows: Vec<T> = resp.json().await?;
    Ok(rows)
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

pub async fn fetch_journal(limit: i64) -> anyhow::Result<Vec<JournalEntry>> {
    fetch_list("/journal", &[("limit", &limit.to_string())]).await
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
pub async fn fetch_tasks(
    owner: &str,
    status: &str,
    all: bool,
) -> anyhow::Result<Vec<Task>> {
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
        &[("author", author), ("tag", tag), ("limit", &limit.to_string())],
    )
    .await
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
            (
                "unacknowledged",
                if unacknowledged { "true" } else { "" },
            ),
            ("limit", &limit.to_string()),
        ],
    )
    .await
}
