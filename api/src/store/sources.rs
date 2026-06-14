// Worker feed sources CRUD + poll/ingest (store.ts `sources`, `pollSources`,
// `ingest`, `ingestScrape`; feed.ts; scrape.ts). Owned by the admin workstream.

use anyhow::Result;
use hive_shared::{EntityKind, InboxReason, NewSource, Severity, Source, SourceKind, SourcePatch};
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, Store};

#[derive(Debug, Clone, serde::Serialize)]
pub struct PollOutcome {
    pub polled: i64,
    pub ingested: i64,
}

/// One parsed feed entry (feed.ts FeedItem).
#[derive(Debug, Clone)]
pub struct FeedItem {
    pub guid: String,
    pub title: String,
    pub url: Option<String>,
    pub body: Option<String>,
}

/// One scraped page candidate (scrape.ts ScrapeItem).
#[derive(Debug, Clone)]
pub struct ScrapeItem {
    pub guid: String,
    pub title: String,
    pub url: String,
}

impl Store {
    /// List sources. With an owner, returns global (owner=null) + that actor's;
    /// `None` returns all (the worker path).
    pub async fn sources_list(&self, owner: Option<&str>) -> Result<Vec<Source>> {
        let rows = crate::pgq::query("SELECT * FROM sources ORDER BY created_at")
            .fetch_all(self.db())
            .await?;
        let all = rows.iter().map(row_to_source).collect::<Result<Vec<_>>>()?;
        Ok(match owner {
            None => all,
            Some(o) => all
                .into_iter()
                .filter(|s| s.owner.is_none() || s.owner.as_deref() == Some(o))
                .collect(),
        })
    }

    pub async fn sources_get(&self, source_id: &str) -> Result<Option<Source>> {
        let row = crate::pgq::query("SELECT * FROM sources WHERE id = ?")
            .bind(source_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_source).transpose()
    }

    pub async fn sources_create(&self, input: NewSource, actor: &str) -> Result<Source> {
        let s = Source {
            id: new_id("src"),
            name: input.name,
            url: input.url,
            kind: input.kind.unwrap_or(SourceKind::Rss),
            category: input.category,
            severity: input.severity.unwrap_or(Severity::Info),
            interval_secs: input.interval_secs.unwrap_or(900),
            notify: input.notify,
            enabled: input.enabled.unwrap_or(true),
            owner: input.owner,
            last_polled_at: None,
            last_status: None,
            created_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO sources (id, name, url, kind, category, severity, interval_secs, notify, enabled, owner, last_polled_at, last_status, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?)",
        )
        .bind(&s.id)
        .bind(&s.name)
        .bind(&s.url)
        .bind(s.kind.as_str())
        .bind(&s.category)
        .bind(s.severity.as_str())
        .bind(s.interval_secs)
        .bind(&s.notify)
        .bind(s.enabled)
        .bind(&s.owner)
        .bind(&s.created_at)
        .execute(self.db())
        .await?;
        self.emit(
            "source.added",
            actor,
            json!({"id": s.id, "name": s.name, "url": s.url}),
        )
        .await?;
        Ok(s)
    }

    pub async fn sources_update(
        &self,
        source_id: &str,
        patch: SourcePatch,
        actor: &str,
    ) -> Result<Option<Source>> {
        let Some(cur) = self.sources_get(source_id).await? else {
            return Ok(None);
        };
        // {...cur, ...patch} — present keys override; double-Option fields can null.
        let next = Source {
            id: cur.id,
            name: patch.name.unwrap_or(cur.name),
            url: patch.url.unwrap_or(cur.url),
            kind: patch.kind.unwrap_or(cur.kind),
            category: patch.category.unwrap_or(cur.category),
            severity: patch.severity.unwrap_or(cur.severity),
            interval_secs: patch.interval_secs.unwrap_or(cur.interval_secs),
            notify: patch.notify.unwrap_or(cur.notify),
            enabled: patch.enabled.unwrap_or(cur.enabled),
            owner: patch.owner.unwrap_or(cur.owner),
            last_polled_at: cur.last_polled_at,
            last_status: cur.last_status,
            created_at: cur.created_at,
        };
        crate::pgq::query(
            "UPDATE sources SET name=?, url=?, kind=?, category=?, severity=?, interval_secs=?, notify=?, enabled=?, owner=? WHERE id=?",
        )
        .bind(&next.name)
        .bind(&next.url)
        .bind(next.kind.as_str())
        .bind(&next.category)
        .bind(next.severity.as_str())
        .bind(next.interval_secs)
        .bind(&next.notify)
        .bind(next.enabled)
        .bind(&next.owner)
        .bind(&next.id)
        .execute(self.db())
        .await?;
        self.emit("source.updated", actor, json!({"id": next.id}))
            .await?;
        Ok(Some(next))
    }

    pub async fn sources_remove(&self, source_id: &str, actor: &str) -> Result<bool> {
        let ok = crate::pgq::query("DELETE FROM sources WHERE id = ?")
            .bind(source_id)
            .execute(self.db())
            .await?
            .rows_affected()
            > 0;
        if ok {
            self.emit("source.removed", actor, json!({"id": source_id}))
                .await?;
        }
        Ok(ok)
    }

    /// Enabled sources whose poll interval has elapsed.
    pub async fn sources_due(&self) -> Result<Vec<Source>> {
        let now = chrono::Utc::now();
        Ok(self
            .sources_list(None)
            .await?
            .into_iter()
            .filter(|s| s.enabled)
            .filter(|s| match &s.last_polled_at {
                None => true,
                // Unparseable timestamps are "not due" (NaN comparison in Node).
                Some(at) => chrono::DateTime::parse_from_rfc3339(at)
                    .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() >= s.interval_secs)
                    .unwrap_or(false),
            })
            .collect())
    }

    pub async fn sources_mark_polled(&self, source_id: &str, status: &str) -> Result<()> {
        crate::pgq::query("UPDATE sources SET last_polled_at = ?, last_status = ? WHERE id = ?")
            .bind(now_iso())
            .bind(status)
            .bind(source_id)
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// Poll feed/scrape sources into wire events (store.ts pollSources). With no
    /// `id`, polls every due+enabled source; with an `id`, polls that one source
    /// if it's enabled, ignoring its interval. Per-source failures land in
    /// last_status, never abort the batch.
    pub async fn poll_sources(&self, id: Option<&str>) -> Result<PollOutcome> {
        let targets = match id {
            Some(id) => match self.sources_get(id).await? {
                Some(s) if s.enabled => vec![s],
                _ => vec![],
            },
            None => self.sources_due().await?,
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let mut polled = 0;
        let mut ingested = 0;
        for source in targets {
            polled += 1;
            let outcome: Result<(i64, String)> = async {
                let res = client.get(&source.url).send().await?;
                if !res.status().is_success() {
                    anyhow::bail!("HTTP {}", res.status().as_u16());
                }
                let text = res.text().await?;
                if source.kind == SourceKind::Scrape {
                    let items = parse_page(&text, &source.url);
                    let count = self.ingest_scrape(&source, &items).await?;
                    Ok((
                        count,
                        format!("ok · {} new of {} items", count, items.len()),
                    ))
                } else {
                    let items = parse_feed(&text);
                    let count = self.ingest_feed(&source, &items).await?;
                    Ok((count, format!("ok · {} items", items.len())))
                }
            }
            .await;
            match outcome {
                Ok((count, status)) => {
                    ingested += count;
                    self.sources_mark_polled(&source.id, &status).await?;
                }
                Err(e) => {
                    self.sources_mark_polled(&source.id, &format!("error · {e}"))
                        .await?;
                }
            }
        }
        Ok(PollOutcome { polled, ingested })
    }

    /// Ingest fetched feed items into wire events (deduped by guid).
    pub async fn ingest_feed(&self, source: &Source, items: &[FeedItem]) -> Result<i64> {
        let mut added = 0;
        for it in items {
            if self.wire_has_guid("feed.item", &it.guid).await? {
                continue;
            }
            self.emit(
                "feed.item",
                &source.name,
                json!({
                    "guid": it.guid,
                    "title": it.title,
                    "url": it.url,
                    "body": it.body.clone().unwrap_or_default(),
                    "source": source.name,
                    "category": source.category,
                    "severity": source.severity,
                }),
            )
            .await?;
            if let Some(notify) = &source.notify {
                self.inbox_add(
                    notify,
                    &source.name,
                    InboxReason::Mention,
                    EntityKind::Journal,
                    &source.id,
                    None,
                    &format!("{}: {}", source.name, it.title),
                )
                .await?;
            }
            added += 1;
        }
        Ok(added)
    }

    /// Ingest scraped page items into wire events (deduped by guid = resolved URL).
    pub async fn ingest_scrape(&self, source: &Source, items: &[ScrapeItem]) -> Result<i64> {
        let mut added = 0;
        for it in items {
            if self.wire_has_guid("scrape.item", &it.guid).await? {
                continue;
            }
            self.emit(
                "scrape.item",
                &source.name,
                json!({
                    "guid": it.guid,
                    "title": it.title,
                    "url": it.url,
                    "source": source.name,
                    "category": source.category,
                    "severity": source.severity,
                }),
            )
            .await?;
            if let Some(notify) = &source.notify {
                self.inbox_add(
                    notify,
                    &source.name,
                    InboxReason::Mention,
                    EntityKind::Journal,
                    &source.id,
                    None,
                    &format!("{}: {}", source.name, it.title),
                )
                .await?;
            }
            added += 1;
        }
        Ok(added)
    }

    /// Node's dedup: the guid (JSON-escaped, unquoted) appears in a stored payload.
    async fn wire_has_guid(&self, kind: &str, guid: &str) -> Result<bool> {
        let escaped = serde_json::to_string(guid)?;
        let pattern = format!("%{}%", &escaped[1..escaped.len() - 1]);
        Ok(
            crate::pgq::query("SELECT 1 FROM wire WHERE kind = ? AND payload LIKE ? LIMIT 1")
                .bind(kind)
                .bind(pattern)
                .fetch_optional(self.db())
                .await?
                .is_some(),
        )
    }
}

fn row_to_source(r: &sqlx::postgres::PgRow) -> Result<Source> {
    Ok(Source {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        url: r.try_get("url")?,
        kind: SourceKind::from_str_lossy(r.try_get::<String, _>("kind")?.as_str()),
        category: r.try_get("category")?,
        severity: Severity::from_str_lossy(r.try_get::<String, _>("severity")?.as_str()),
        interval_secs: r.try_get("interval_secs")?,
        notify: r.try_get("notify")?,
        enabled: r.try_get::<bool, _>("enabled")?,
        owner: r.try_get("owner")?,
        last_polled_at: r.try_get("last_polled_at")?,
        last_status: r.try_get("last_status")?,
        created_at: r.try_get("created_at")?,
    })
}

// ============================================================================
// feed.ts — RSS/Atom parsing. RSS 2.0 goes through the `rss` crate; anything it
// can't read (Atom <entry> feeds, sloppy XML) falls back to the same loose
// block scan Node's hand-rolled parser used.
// ============================================================================

pub fn parse_feed(xml: &str) -> Vec<FeedItem> {
    if let Ok(channel) = rss::Channel::read_from(xml.as_bytes()) {
        let items: Vec<FeedItem> = channel
            .items()
            .iter()
            .map(|item| {
                let title = item.title().unwrap_or("(untitled)").trim().to_string();
                let url = item.link().map(|s| s.trim().to_string());
                let guid = item
                    .guid()
                    .map(|g| g.value().trim().to_string())
                    .or_else(|| url.clone())
                    .unwrap_or_else(|| title.clone());
                let body = item.description().map(|s| s.trim().to_string());
                FeedItem {
                    guid,
                    title,
                    url,
                    body,
                }
            })
            .collect();
        if !items.is_empty() {
            return items;
        }
    }
    parse_feed_blocks(xml)
}

/// Node feed.ts parseFeed: pull guid/title/link/description out of each
/// <item> (RSS) or <entry> (Atom) block.
fn parse_feed_blocks(xml: &str) -> Vec<FeedItem> {
    let mut items = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = next_block(xml, pos) {
        let block = &xml[start..end];
        let title = tag_text(block, "title").unwrap_or_else(|| "(untitled)".to_string());
        let link = tag_text(block, "link").or_else(|| attr_value(block, "link", "href"));
        let guid = tag_text(block, "guid")
            .or_else(|| tag_text(block, "id"))
            .or_else(|| link.clone())
            .unwrap_or_else(|| title.clone());
        let body = tag_text(block, "description")
            .or_else(|| tag_text(block, "summary"))
            .or_else(|| tag_text(block, "content"));
        items.push(FeedItem {
            guid,
            title,
            url: link,
            body,
        });
        pos = end;
    }
    items
}

/// Next `<item>…</item>` or `<entry>…</entry>` block at/after `from`.
fn next_block(xml: &str, from: usize) -> Option<(usize, usize)> {
    let start = min_opt(find_ci(xml, "<item", from), find_ci(xml, "<entry", from))?;
    let end = min_opt(
        find_ci(xml, "</item>", start).map(|i| i + "</item>".len()),
        find_ci(xml, "</entry>", start).map(|i| i + "</entry>".len()),
    )?;
    Some((start, end))
}

/// Node's `tag()`: `<name[^>]*>(content)</name>`, CDATA-unwrapped, trimmed, decoded.
fn tag_text(block: &str, name: &str) -> Option<String> {
    let at = find_ci(block, &format!("<{name}"), 0)?;
    let gt = block[at..].find('>')? + at;
    let end = find_ci(block, &format!("</{name}>"), gt + 1)?;
    let raw = block[gt + 1..end]
        .replace("<![CDATA[", "")
        .replace("]]>", "");
    Some(decode_entities(raw.trim()))
}

/// Node's `attr()`: `<name[^>]*\battr="([^"]+)"` within the opening tag.
fn attr_value(block: &str, name: &str, attr: &str) -> Option<String> {
    let at = find_ci(block, &format!("<{name}"), 0)?;
    let gt = block[at..].find('>')? + at;
    let open_tag = &block[at..gt];
    let needle = format!("{attr}=\"");
    let a = find_ci(open_tag, &needle, 0)? + needle.len();
    let end = open_tag[a..].find('"')? + a;
    if a == end {
        return None; // [^"]+ requires a non-empty value
    }
    Some(open_tag[a..end].to_string())
}

// ============================================================================
// scrape.ts — anchor links + h1..h3 headings out of an HTML page.
// ============================================================================

pub fn parse_page(html: &str, base_url: &str) -> Vec<ScrapeItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Anchors: <a href="...">text</a> (Node: `<a\s[^>]*href="([^"]*)"[^>]*>([\s\S]*?)<\/a>`).
    let mut pos = 0;
    while let Some(at) = find_ci(html, "<a", pos) {
        pos = at + 2;
        if !html[at + 2..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace())
        {
            continue;
        }
        let Some(gt) = html[at..].find('>').map(|i| i + at) else {
            break;
        };
        let Some(href) = anchor_href(&html[at..gt]) else {
            continue;
        };
        let Some(close) = find_ci(html, "</a>", gt + 1) else {
            continue;
        };
        pos = close + "</a>".len();
        let title = decode_entities(&strip_tags(&html[gt + 1..close]));
        if title.chars().count() < 3 {
            continue;
        }
        let Some(url) = resolve_url(href.trim(), base_url) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        items.push(ScrapeItem {
            guid: url.clone(),
            title,
            url,
        });
    }

    // Headings h1..h3 (Node: `<(h[1-3])\b[^>]*>([\s\S]*?)<\/h[1-3]>`).
    let mut pos = 0;
    while let Some(at) = find_ci(html, "<h", pos) {
        pos = at + 2;
        let mut rest = html[at + 2..].chars();
        if !matches!(rest.next(), Some('1'..='3')) {
            continue;
        }
        if rest
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue; // \b boundary after the digit
        }
        let Some(gt) = html[at..].find('>').map(|i| i + at) else {
            break;
        };
        let Some(close) = min_opt(
            min_opt(
                find_ci(html, "</h1>", gt + 1),
                find_ci(html, "</h2>", gt + 1),
            ),
            find_ci(html, "</h3>", gt + 1),
        ) else {
            continue;
        };
        pos = close + "</h1>".len();
        let title = decode_entities(&strip_tags(&html[gt + 1..close]));
        if title.chars().count() < 3 {
            continue;
        }
        // Title as a stable key, prefixed with baseUrl to scope it to this source.
        let guid = format!("heading:{base_url}:{title}");
        if !seen.insert(guid.clone()) {
            continue;
        }
        items.push(ScrapeItem {
            guid,
            title,
            url: base_url.to_string(),
        });
    }

    items
}

/// `href="([^"]*)"` inside an anchor's opening tag (empty value allowed).
fn anchor_href(open_tag: &str) -> Option<String> {
    let needle = "href=\"";
    let a = find_ci(open_tag, needle, 0)? + needle.len();
    let end = open_tag[a..].find('"')? + a;
    Some(open_tag[a..end].to_string())
}

fn resolve_url(href: &str, base: &str) -> Option<String> {
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    let base = reqwest::Url::parse(base).ok()?;
    base.join(href).ok().map(String::from)
}

/// `<[^>]+>` → " ", collapse whitespace, trim.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The five entities Node decodes, in the same order.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// ASCII case-insensitive substring search starting at byte `from`.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || from > h.len() {
        return None;
    }
    h[from..]
        .windows(n.len())
        .position(|w| w.eq_ignore_ascii_case(n))
        .map(|i| i + from)
}

fn min_opt(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_XML: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Bee feed</title><item><guid>bee-rss-1</guid><title>pgvector 0.8 released</title><link>https://example.com/bee-rss-1</link><description>Postgres vector search gets faster ANN indexes.</description></item><item><guid>bee-rss-2</guid><title>Solid 2.0 roadmap</title><link>https://example.com/bee-rss-2</link><description>Fine-grained reactivity, same tiny runtime.</description></item><item><guid>bee-rss-3</guid><title>SQLite ships native JSON5</title><link>https://example.com/bee-rss-3</link><description>Looser JSON parsing lands in the amalgamation.</description></item></channel></rss>"#;

    #[test]
    fn parses_rss_fixture() {
        let items = parse_feed(FIXTURE_XML);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].guid, "bee-rss-1");
        assert_eq!(items[0].title, "pgvector 0.8 released");
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://example.com/bee-rss-1")
        );
        assert_eq!(
            items[0].body.as_deref(),
            Some("Postgres vector search gets faster ANN indexes.")
        );
    }

    #[test]
    fn parses_atom_fallback() {
        let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
<entry><title>Atom post</title><id>atom-1</id><link href="https://example.com/atom-1"/><summary>Hello &amp; welcome</summary></entry>
</feed>"#;
        let items = parse_feed(atom);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].guid, "atom-1");
        assert_eq!(items[0].url.as_deref(), Some("https://example.com/atom-1"));
        assert_eq!(items[0].body.as_deref(), Some("Hello & welcome"));
    }

    #[test]
    fn parses_scrape_fixture() {
        let html = r#"<!DOCTYPE html><html><head><title>Bee scrape fixture</title></head><body>
<h1>Bee's Roadhouse dev feed</h1>
<h2>Latest picks</h2>
<ul>
  <li><a href="https://example.com/bee-scrape-1">Hono v4 ships — faster routing, smaller core</a></li>
  <li><a href="https://example.com/bee-scrape-2">SolidJS fine-grained signals land in v2</a></li>
  <li><a href="https://example.com/bee-scrape-3">better-sqlite3 adds WAL2 support</a></li>
</ul>
<nav><a href="/">home</a> <a href="/about">about</a></nav>
</body></html>"#;
        let items = parse_page(html, "https://example.com/feed");
        // 5 anchors (3 absolute, "/" and "/about" resolved) + 2 headings.
        assert_eq!(items.len(), 7);
        assert_eq!(items[0].guid, "https://example.com/bee-scrape-1");
        assert_eq!(items[3].url, "https://example.com/");
        assert_eq!(items[4].url, "https://example.com/about");
        assert_eq!(
            items[5].guid,
            "heading:https://example.com/feed:Bee's Roadhouse dev feed"
        );
        assert_eq!(items[6].title, "Latest picks");
    }

    #[test]
    fn skips_fragments_and_javascript_links() {
        let html = r##"<a href="#top">to the top</a><a href="javascript:void(0)">click me</a><a href="/ok">a real link</a>"##;
        let items = parse_page(html, "https://example.com/");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://example.com/ok");
    }
}
