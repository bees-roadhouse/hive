//! HTTP client layer for hive-api. Plain Rust structs only;
//! Slint conversion happens in main.rs.
//!
//! Resolves the API URL once at first use. Resolution order:
//!   1. `HIVE_API_URL` (explicit override) wins if set
//!   2. else `HIVE_PUBLIC_URL` + `HIVE_PRIVATE_URL` +
//!      `HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS` ... if the system's
//!      DNS search-domain list intersects the awareness list, use private;
//!      else use public
//!   3. else fall back to `http://localhost:7878`
//! On any transport / decode failure: emit `eprintln!` and return an empty Vec
//! so the UI degrades quietly rather than failing to launch.

use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct TaskFetched {
    pub id: i64,
    pub title: String,
    pub due_label: String,
    pub overdue: bool,
}

#[derive(Debug, Clone)]
pub struct TaskFullFetched {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub project: String,
    pub owner: String,
    pub priority: String,
    pub status: String,
    pub due_label: String,
    pub block_reason: String,
}

#[derive(Debug, Clone)]
pub struct WireFetched {
    pub title: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct JournalFetched {
    pub id: i32,
    pub ai: String,
    pub when_label: String,
    pub title: String,
    pub body: String,
    pub tags_label: String,
    pub related_label: String,
}

/// Raw JSON shape returned by hive-api. Mirrors `hive_db::types::Task`.
#[derive(Debug, Deserialize)]
struct ApiTask {
    id: i64,
    project: String,
    title: String,
    body: Option<String>,
    owner: String,
    status: String,
    priority: Option<String>,
    due: Option<String>,
    block_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiJournal {
    id: i64,
    ai: String,
    title: Option<String>,
    body: String,
    tags: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct ApiWire {
    title: String,
    source: String,
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

fn api_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        // explicit override wins
        if let Ok(url) = std::env::var("HIVE_API_URL") {
            let u = url.trim();
            if !u.is_empty() {
                return u.trim_end_matches('/').to_string();
            }
        }
        let public = std::env::var("HIVE_PUBLIC_URL").ok().filter(|s| !s.is_empty());
        let private = std::env::var("HIVE_PRIVATE_URL").ok().filter(|s| !s.is_empty());
        let awareness: std::collections::HashSet<String> =
            std::env::var("HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
        // network-aware: private wins iff system DNS search domains intersect awareness list
        if let Some(p) = private.as_ref() {
            if !awareness.is_empty() {
                let domains: std::collections::HashSet<_> =
                    system_search_domains().into_iter().collect();
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

fn http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build blocking reqwest client")
    })
}

fn get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Option<T> {
    let url = format!("{}{}", api_base(), path);
    match http_client().get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                eprintln!("hive-desktop: GET {url} returned {status}");
                return None;
            }
            match resp.json::<T>() {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("hive-desktop: decode {url} failed: {e}");
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("hive-desktop: GET {url} failed: {e}");
            None
        }
    }
}

/// POST /tasks/{id}/done. Returns true on 2xx, false otherwise. Body ignored.
pub fn mark_task_done(id: i64) -> bool {
    let url = format!("{}/tasks/{}/done", api_base(), id);
    match http_client().post(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                eprintln!("hive-desktop: POST {url} returned {status}");
                return false;
            }
            true
        }
        Err(e) => {
            eprintln!("hive-desktop: POST {url} failed: {e}");
            false
        }
    }
}

/// ET (America/New_York) as a fixed offset. Roadhouse runs ET; the desktop
/// client follows. Skip full IANA tz crate for now .. EDT vs EST drift is
/// cosmetic on a label and we can swap to chrono-tz later if needed.
fn et_offset() -> FixedOffset {
    // EDT (-04:00) for now; this is May. Switch to chrono-tz if we ever ship
    // outside DST or care about winter labels.
    FixedOffset::west_opt(4 * 3600).expect("valid offset")
}

fn parse_utc_naive(s: &str) -> Option<DateTime<Utc>> {
    // SQLite datetime('now') format: "YYYY-MM-DD HH:MM:SS" naive UTC.
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|nd| Utc.from_utc_datetime(&nd))
}

fn format_when_label(created_at_utc: &str) -> String {
    let Some(utc) = parse_utc_naive(created_at_utc) else {
        return created_at_utc.to_string();
    };
    let et = utc.with_timezone(&et_offset());
    let today_et = Utc::now().with_timezone(&et_offset()).date_naive();
    let entry_date_et = et.date_naive();
    let time_part = et.format("%I:%M %p ET").to_string();
    // Strip leading zero from hour for "12:09 PM ET" vs "02:09 PM ET" style.
    let time_part = time_part.trim_start_matches('0').to_string();
    if entry_date_et == today_et {
        format!("today .. {}", time_part)
    } else {
        format!("{} .. {}", entry_date_et, time_part)
    }
}

fn today_et_naive() -> NaiveDate {
    Utc::now().with_timezone(&et_offset()).date_naive()
}

fn parse_due_date(s: &str) -> Option<NaiveDate> {
    // Tasks.due is stored as YYYY-MM-DD by hive.py.
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn task_due_label(due: Option<&str>) -> (String, bool) {
    let Some(due_str) = due else {
        return (String::new(), false);
    };
    let Some(due_date) = parse_due_date(due_str) else {
        return (due_str.to_string(), false);
    };
    let today = today_et_naive();
    let days = (due_date - today).num_days();
    if days == 0 {
        ("today".to_string(), false)
    } else if days < 0 {
        let n = -days;
        (format!("overdue {} day{}", n, if n == 1 { "" } else { "s" }), true)
    } else {
        (format!("{} day{}", days, if days == 1 { "" } else { "s" }), false)
    }
}

fn priority_weight(p: Option<&str>) -> u8 {
    let s = p.unwrap_or("");
    if s.contains("hot") {
        0
    } else if s.contains("next") {
        1
    } else {
        2
    }
}

pub fn fetch_today_tasks() -> Vec<TaskFetched> {
    // API default (no status, all=false) returns open + in_progress only,
    // matching the python rules. Filter + sort + cap client-side.
    let Some(tasks) = get_json::<Vec<ApiTask>>("/tasks") else {
        return Vec::new();
    };
    let today = today_et_naive();
    let mut filtered: Vec<ApiTask> = tasks
        .into_iter()
        .filter(|t| {
            // Match the original WHERE: (due set AND due <= today) OR priority hot.
            let due_hit = t
                .due
                .as_deref()
                .and_then(parse_due_date)
                .map(|d| d <= today)
                .unwrap_or(false);
            let hot_hit = t
                .priority
                .as_deref()
                .map(|s| s.contains("hot"))
                .unwrap_or(false);
            due_hit || hot_hit
        })
        .collect();

    filtered.sort_by(|a, b| {
        let a_due_now = a
            .due
            .as_deref()
            .and_then(parse_due_date)
            .map(|d| d <= today)
            .unwrap_or(false);
        let b_due_now = b
            .due
            .as_deref()
            .and_then(parse_due_date)
            .map(|d| d <= today)
            .unwrap_or(false);
        // overdue/today bucket first
        match (a_due_now, b_due_now) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        // then due ascending (None sorts after Some)
        let a_due = a.due.as_deref().and_then(parse_due_date);
        let b_due = b.due.as_deref().and_then(parse_due_date);
        let due_cmp = match (a_due, b_due) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if due_cmp != std::cmp::Ordering::Equal {
            return due_cmp;
        }
        // then priority weight
        priority_weight(a.priority.as_deref()).cmp(&priority_weight(b.priority.as_deref()))
    });

    filtered
        .into_iter()
        .take(6)
        .map(|t| {
            let (due_label, overdue) = task_due_label(t.due.as_deref());
            TaskFetched {
                id: t.id,
                title: t.title,
                due_label,
                overdue,
            }
        })
        .collect()
}

pub fn fetch_all_tasks() -> Vec<TaskFullFetched> {
    // `all=true` returns every status; drop `dropped` client-side to match
    // the original SQL (`status != 'dropped'`).
    let Some(tasks) = get_json::<Vec<ApiTask>>("/tasks?all=true") else {
        return Vec::new();
    };
    let mut tasks: Vec<ApiTask> = tasks
        .into_iter()
        .filter(|t| t.status != "dropped")
        .collect();

    fn status_weight(s: &str) -> u8 {
        match s {
            "in_progress" => 0,
            "open" => 1,
            "blocked" => 2,
            "done" => 3,
            _ => 4,
        }
    }

    tasks.sort_by(|a, b| {
        let sw = status_weight(&a.status).cmp(&status_weight(&b.status));
        if sw != std::cmp::Ordering::Equal {
            return sw;
        }
        let pw = priority_weight(a.priority.as_deref())
            .cmp(&priority_weight(b.priority.as_deref()));
        if pw != std::cmp::Ordering::Equal {
            return pw;
        }
        // nulls-last on due, then asc, then id asc.
        let a_due = a.due.as_deref().and_then(parse_due_date);
        let b_due = b.due.as_deref().and_then(parse_due_date);
        let null_cmp = a_due.is_none().cmp(&b_due.is_none());
        if null_cmp != std::cmp::Ordering::Equal {
            return null_cmp;
        }
        let due_cmp = match (a_due, b_due) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => std::cmp::Ordering::Equal,
        };
        if due_cmp != std::cmp::Ordering::Equal {
            return due_cmp;
        }
        a.id.cmp(&b.id)
    });

    tasks
        .into_iter()
        .take(50)
        .map(|t| {
            let (due_label, _) = task_due_label(t.due.as_deref());
            TaskFullFetched {
                id: t.id,
                title: t.title,
                body: t.body.unwrap_or_default(),
                project: t.project,
                owner: t.owner,
                priority: t.priority.unwrap_or_default(),
                status: t.status,
                due_label,
                block_reason: t.block_reason.unwrap_or_default(),
            }
        })
        .collect()
}

pub fn fetch_wire(limit: i64) -> Vec<WireFetched> {
    let path = format!("/wire?limit={}", limit);
    let Some(rows) = get_json::<Vec<ApiWire>>(&path) else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|w| WireFetched {
            title: w.title,
            source: w.source,
        })
        .collect()
}

pub fn fetch_journal(limit: i64) -> Vec<JournalFetched> {
    let path = format!("/journal?limit={}", limit);
    let Some(rows) = get_json::<Vec<ApiJournal>>(&path) else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|j| JournalFetched {
            id: j.id as i32,
            ai: j.ai,
            when_label: format_when_label(&j.created_at),
            title: j.title.unwrap_or_default(),
            body: j.body,
            tags_label: j.tags.unwrap_or_default(),
            related_label: String::new(),
        })
        .collect()
}
