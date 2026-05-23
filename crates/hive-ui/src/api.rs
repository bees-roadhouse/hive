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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JournalEntry {
    pub id: i64,
    pub ai: String,
    pub entry_date: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub body: String,
    pub tags: Option<String>,
    pub created_at: Option<String>,
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

pub async fn fetch_journal(limit: i64) -> anyhow::Result<Vec<JournalEntry>> {
    let url = format!("{}/journal?limit={}", api_base(), limit);
    let resp = http_client().get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}");
    }
    let entries: Vec<JournalEntry> = resp.json().await?;
    Ok(entries)
}
