//! Server-rendered views. v1 = leptos `view!` -> `render_to_string`. No
//! hydration, no client JS. v1.5 will introduce islands of interactivity.

use hive_db::queries::search::{JournalHit, NoteHit};
use hive_db::types::{JournalEntry, Note, Project, Task, WireEvent};
use leptos::prelude::*;

const STYLE: &str = r#"
:root { color-scheme: light dark; }
body { font: 14px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; margin: 0; padding: 0; }
header { background: #111; color: #eee; padding: 0.75rem 1.25rem; }
header a { color: #9cf; text-decoration: none; margin-right: 1rem; }
header a:hover { text-decoration: underline; }
header strong { color: #fff; margin-right: 1.5rem; font-weight: 600; }
main { padding: 1rem 1.25rem; max-width: 100%; }
h1 { font-size: 1.25rem; margin: 0 0 1rem 0; }
h2 { font-size: 1rem; margin: 1.25rem 0 0.5rem 0; color: #555; font-weight: 600; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { text-align: left; padding: 0.4rem 0.6rem; border-bottom: 1px solid #ddd; vertical-align: top; }
th { background: #f5f5f5; font-weight: 600; }
tr:hover td { background: #fafafa; }
td.id { font-variant-numeric: tabular-nums; color: #888; width: 4rem; }
td.date { font-variant-numeric: tabular-nums; white-space: nowrap; width: 6.5rem; color: #555; }
td.ai { width: 4rem; color: #07a; font-weight: 500; }
td.tags { font-size: 11px; max-width: 18rem; }
td.tags a, .chip { display: inline-block; padding: 1px 6px; margin: 1px 2px 1px 0; background: #eef; color: #449; border-radius: 8px; font-size: 11px; text-decoration: none; }
td.tags a:hover { background: #dde; }
td.title a { color: #06d; text-decoration: none; }
td.title a:hover { text-decoration: underline; }
form.search { float: right; }
form.search input { padding: 0.25rem 0.5rem; border: 1px solid #444; background: #222; color: #eee; border-radius: 3px; font: inherit; width: 18rem; }
.snip { font-size: 12px; color: #555; margin: 0.15rem 0 0 0; }
.snip mark { background: #ff6; padding: 0 2px; }
.applied { color: #888; margin-bottom: 0.5rem; }
.applied a { margin-left: 0.5rem; color: #c33; text-decoration: none; }
.empty { color: #999; padding: 2rem 0; text-align: center; }
.meta { color: #888; font-size: 12px; margin-bottom: 0.75rem; }
pre { background: #f5f5f5; padding: 0.75rem; overflow-x: auto; white-space: pre-wrap; word-wrap: break-word; }
@media (prefers-color-scheme: dark) {
  body { background: #181818; color: #ddd; }
  th { background: #222; }
  th, td { border-bottom-color: #333; }
  tr:hover td { background: #1f1f1f; }
  td.id, .meta, .empty { color: #777; }
  td.date, h2 { color: #aaa; }
  pre { background: #222; }
  td.tags a, .chip { background: #224; color: #adf; }
  td.tags a:hover { background: #335; }
  .snip { color: #aaa; }
  .snip mark { background: #663; color: #ff8; }
  .applied { color: #888; }
}
"#;

fn nav() -> impl IntoView {
    view! {
        <header>
            <strong>"hive-ui"</strong>
            <a href="/">"home"</a>
            <a href="/journal">"journal"</a>
            <a href="/tasks">"tasks"</a>
            <a href="/notes">"notes"</a>
            <a href="/wire">"wire"</a>
            <a href="/projects">"projects"</a>
            <form class="search" action="/search" method="get">
                <input type="search" name="q" placeholder="search journal + notes (FTS5)..."/>
            </form>
        </header>
    }
}

fn tag_chips(tag_str: &str) -> Vec<impl IntoView + use<>> {
    tag_str
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let href = format!("/journal?tag={}", urlencode(&t));
            view! { <a href={href}>{t}</a> }
        })
        .collect()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .flat_map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                vec![b as char]
            } else {
                format!("%{:02X}", b).chars().collect()
            }
        })
        .collect()
}

fn page_shell(title: &str, body: impl IntoView + 'static) -> String {
    let title_owned = title.to_string();
    let inner = view! {
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width,initial-scale=1"/>
                <title>{title_owned}</title>
                <style>{STYLE}</style>
            </head>
            <body>
                {nav()}
                <main>
                    {body}
                </main>
            </body>
        </html>
    }
    .to_html();
    format!("<!DOCTYPE html>{}", inner)
}

fn journal_table(entries: Vec<JournalEntry>) -> impl IntoView {
    let count = entries.len();
    let rows: Vec<_> = entries
        .into_iter()
        .map(|e| {
            let id = e.id;
            let title = e.title.unwrap_or_default();
            let tags = e.tags.unwrap_or_default();
            let href = format!("/journal/{id}");
            let ai_href = format!("/journal?ai={}", urlencode(&e.ai));
            view! {
                <tr>
                    <td class="id">{id}</td>
                    <td class="date">{e.entry_date}</td>
                    <td class="ai"><a href={ai_href} style="color:inherit;text-decoration:none">{e.ai}</a></td>
                    <td class="title"><a href={href}>{title}</a></td>
                    <td class="tags">{tag_chips(&tags)}</td>
                </tr>
            }
        })
        .collect();
    view! {
        <p class="meta">{count} " entries"</p>
        <table>
            <thead>
                <tr>
                    <th>"id"</th>
                    <th>"date"</th>
                    <th>"ai"</th>
                    <th>"title"</th>
                    <th>"tags"</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
    }
}

pub fn render_home(entries: Vec<JournalEntry>) -> String {
    page_shell(
        "hive-ui — recent journal",
        view! {
            <h1>"Recent journal entries"</h1>
            {journal_table(entries)}
        },
    )
}

pub fn render_journal_page(
    entries: Vec<JournalEntry>,
    tag_filter: Option<String>,
    ai_filter: Option<String>,
) -> String {
    let applied = match (tag_filter.as_deref(), ai_filter.as_deref()) {
        (None, None) => None,
        (tag, ai) => {
            let mut bits = Vec::new();
            if let Some(t) = tag {
                bits.push(format!("tag = {t}"));
            }
            if let Some(a) = ai {
                bits.push(format!("ai = {a}"));
            }
            Some(bits.join(" · "))
        }
    };
    let applied_view = applied.map(|a| {
        view! {
            <p class="applied">
                "Filtering: " {a}
                <a href="/journal">"clear"</a>
            </p>
        }
    });
    page_shell(
        "hive-ui — journal",
        view! {
            <h1>"Journal"</h1>
            {applied_view}
            {journal_table(entries)}
        },
    )
}

pub fn render_search_page(
    query: String,
    journal_hits: Vec<JournalHit>,
    note_hits: Vec<NoteHit>,
) -> String {
    if query.trim().is_empty() {
        return page_shell(
            "hive-ui — search",
            view! {
                <h1>"Search"</h1>
                <p class="meta">"Use the search box in the header. FTS5 syntax: " <code>"hive OR cera"</code> ", " <code>"\"exact phrase\""</code> ", " <code>"prefix*"</code> "."</p>
            },
        );
    }
    let jcount = journal_hits.len();
    let ncount = note_hits.len();
    let jrows: Vec<_> = journal_hits
        .into_iter()
        .map(|h| {
            let title = h.title.unwrap_or_default();
            let tags = h.tags.unwrap_or_default();
            let href = format!("/journal/{}", h.id);
            view! {
                <tr>
                    <td class="id">{h.id}</td>
                    <td class="date">{h.entry_date}</td>
                    <td class="ai">{h.ai}</td>
                    <td class="title">
                        <a href={href}>{title}</a>
                        <p class="snip" inner_html={fts_snippet_to_html(&h.snippet)}></p>
                    </td>
                    <td class="tags">{tag_chips(&tags)}</td>
                </tr>
            }
        })
        .collect();
    let nrows: Vec<_> = note_hits
        .into_iter()
        .map(|h| {
            let title = h.title.unwrap_or_default();
            let tags = h.tags.unwrap_or_default();
            view! {
                <tr>
                    <td class="id">{h.id}</td>
                    <td class="ai">{h.author}</td>
                    <td class="title">
                        {title}
                        <p class="snip" inner_html={fts_snippet_to_html(&h.snippet)}></p>
                    </td>
                    <td class="tags">{tag_chips(&tags)}</td>
                    <td class="ai">{h.project.unwrap_or_default()}</td>
                </tr>
            }
        })
        .collect();
    page_shell(
        "hive-ui — search",
        view! {
            <h1>"Search: " <code>{query}</code></h1>
            <h2>"Journal hits (" {jcount} ")"</h2>
            <table>
                <thead>
                    <tr>
                        <th>"id"</th><th>"date"</th><th>"ai"</th><th>"title / snippet"</th><th>"tags"</th>
                    </tr>
                </thead>
                <tbody>{jrows}</tbody>
            </table>
            <h2>"Note hits (" {ncount} ")"</h2>
            <table>
                <thead>
                    <tr>
                        <th>"id"</th><th>"author"</th><th>"title / snippet"</th><th>"tags"</th><th>"project"</th>
                    </tr>
                </thead>
                <tbody>{nrows}</tbody>
            </table>
        },
    )
}

/// Convert FTS5 snippet markers `[term]` into `<mark>term</mark>`. The query
/// in `search::journal` uses `'['` / `']'` as delimiters. Input must be
/// HTML-escaped first since this returns markup.
fn fts_snippet_to_html(snip: &str) -> String {
    let escaped: String = snip
        .chars()
        .map(|c| match c {
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '&' => "&amp;".into(),
            '"' => "&quot;".into(),
            other => other.to_string(),
        })
        .collect();
    escaped.replace('[', "<mark>").replace(']', "</mark>")
}

pub fn render_journal_detail(id: i64, entry: Option<JournalEntry>) -> String {
    match entry {
        None => page_shell(
            &format!("hive-ui — journal #{id} not found"),
            view! {
                <h1>"Entry " {id} " not found"</h1>
                <p><a href="/journal">"← back to journal"</a></p>
            },
        ),
        Some(e) => {
            let title = e.title.clone().unwrap_or_else(|| format!("entry {}", e.id));
            let title_for_page = format!("hive-ui — {title}");
            let body = view! {
                <h1>{title.clone()}</h1>
                <p class="meta">
                    "id " {e.id} " · " {e.entry_date} " · " {e.ai}
                    " · tags: " {e.tags.unwrap_or_default()}
                </p>
                <pre>{e.body}</pre>
                <p><a href="/journal">"← back to journal"</a></p>
            };
            page_shell(&title_for_page, body)
        }
    }
}

pub fn render_tasks_page(rows: Vec<Task>) -> String {
    let count = rows.len();
    let rendered: Vec<_> = rows
        .into_iter()
        .map(|t| {
            let id = t.id;
            let href = format!("/tasks/{id}");
            view! {
                <tr>
                    <td class="id"><a href={href.clone()} style="color:inherit">{id}</a></td>
                    <td class="ai">{t.owner}</td>
                    <td class="ai">{t.status}</td>
                    <td class="ai">{t.priority.unwrap_or_default()}</td>
                    <td class="date">{t.due.unwrap_or_default()}</td>
                    <td class="title"><a href={href}>{t.title}</a></td>
                    <td class="tags">{t.project}</td>
                </tr>
            }
        })
        .collect();
    page_shell(
        "hive-ui — tasks",
        view! {
            <h1>"Tasks"</h1>
            <p class="meta">{count} " open / in-progress (default filter)"</p>
            <table>
                <thead>
                    <tr>
                        <th>"id"</th>
                        <th>"owner"</th>
                        <th>"status"</th>
                        <th>"priority"</th>
                        <th>"due"</th>
                        <th>"title"</th>
                        <th>"project"</th>
                    </tr>
                </thead>
                <tbody>{rendered}</tbody>
            </table>
        },
    )
}

pub fn render_notes_page(rows: Vec<Note>) -> String {
    let count = rows.len();
    let rendered: Vec<_> = rows
        .into_iter()
        .map(|n| {
            let id = n.id;
            let href = format!("/notes/{id}");
            let title = n.title.unwrap_or_default();
            let body_preview = if n.body.len() > 200 {
                format!("{}…", &n.body[..200])
            } else {
                n.body
            };
            view! {
                <tr>
                    <td class="id"><a href={href.clone()} style="color:inherit">{id}</a></td>
                    <td class="ai">{n.author}</td>
                    <td class="title"><a href={href}>{title}</a></td>
                    <td>{body_preview}</td>
                    <td class="tags">{tag_chips(&n.tags.unwrap_or_default())}</td>
                    <td class="ai">{n.project.unwrap_or_default()}</td>
                </tr>
            }
        })
        .collect();
    page_shell(
        "hive-ui — notes",
        view! {
            <h1>"Notes"</h1>
            <p class="meta">{count} " notes"</p>
            <table>
                <thead>
                    <tr>
                        <th>"id"</th>
                        <th>"author"</th>
                        <th>"title"</th>
                        <th>"body"</th>
                        <th>"tags"</th>
                        <th>"project"</th>
                    </tr>
                </thead>
                <tbody>{rendered}</tbody>
            </table>
        },
    )
}

pub fn render_wire_page(rows: Vec<WireEvent>) -> String {
    let count = rows.len();
    let rendered: Vec<_> = rows
        .into_iter()
        .map(|w| {
            let ack = if w.acknowledged { "✓" } else { "" };
            view! {
                <tr>
                    <td class="id">{w.id}</td>
                    <td class="date">{w.last_seen_at}</td>
                    <td class="ai">{w.source}</td>
                    <td class="ai">{w.severity.unwrap_or_default()}</td>
                    <td>{w.title}</td>
                    <td class="tags">{w.affects.unwrap_or_default()}</td>
                    <td class="ai">{ack}</td>
                </tr>
            }
        })
        .collect();
    page_shell(
        "hive-ui — wire events",
        view! {
            <h1>"Wire events (latest 50)"</h1>
            <p class="meta">{count} " events"</p>
            <table>
                <thead>
                    <tr>
                        <th>"id"</th>
                        <th>"last seen"</th>
                        <th>"source"</th>
                        <th>"severity"</th>
                        <th>"title"</th>
                        <th>"affects"</th>
                        <th>"ack"</th>
                    </tr>
                </thead>
                <tbody>{rendered}</tbody>
            </table>
        },
    )
}

pub fn render_task_detail(id: i64, task: Option<Task>) -> String {
    match task {
        None => page_shell(
            &format!("hive-ui — task #{id} not found"),
            view! {
                <h1>"Task " {id} " not found"</h1>
                <p><a href="/tasks">"← back to tasks"</a></p>
            },
        ),
        Some(t) => {
            let title_for_page = format!("hive-ui — task: {}", t.title);
            let body = view! {
                <h1>{t.title.clone()}</h1>
                <p class="meta">
                    "id " {t.id} " · status " {t.status} " · owner " {t.owner}
                    " · priority " {t.priority.unwrap_or_else(|| "—".into())}
                    " · due " {t.due.unwrap_or_else(|| "—".into())}
                    " · project " {t.project}
                </p>
                <pre>{t.body.unwrap_or_default()}</pre>
                <p class="meta">
                    "created " {t.created_at}
                    " · updated " {t.updated_at}
                    " · closed " {t.closed_at.unwrap_or_else(|| "—".into())}
                </p>
                <p class="meta">
                    "block reason: " {t.block_reason.unwrap_or_else(|| "—".into())}
                </p>
                <p><a href="/tasks">"← back to tasks"</a></p>
            };
            page_shell(&title_for_page, body)
        }
    }
}

pub fn render_note_detail(id: i64, note: Option<Note>) -> String {
    match note {
        None => page_shell(
            &format!("hive-ui — note #{id} not found"),
            view! {
                <h1>"Note " {id} " not found"</h1>
                <p><a href="/notes">"← back to notes"</a></p>
            },
        ),
        Some(n) => {
            let title = n.title.clone().unwrap_or_else(|| format!("note {}", n.id));
            let title_for_page = format!("hive-ui — {title}");
            let body = view! {
                <h1>{title.clone()}</h1>
                <p class="meta">
                    "id " {n.id} " · author " {n.author}
                    " · project " {n.project.unwrap_or_else(|| "—".into())}
                    " · tags: " {tag_chips(&n.tags.unwrap_or_default())}
                </p>
                <pre>{n.body}</pre>
                <p class="meta">
                    "created " {n.created_at}
                    " · updated " {n.updated_at}
                </p>
                <p><a href="/notes">"← back to notes"</a></p>
            };
            page_shell(&title_for_page, body)
        }
    }
}

pub fn render_projects_page(rows: Vec<Project>) -> String {
    let count = rows.len();
    let rendered: Vec<_> = rows
        .into_iter()
        .map(|p| {
            view! {
                <tr>
                    <td class="id">{p.id}</td>
                    <td class="title">{p.name}</td>
                    <td class="ai">{p.status}</td>
                    <td class="ai">{p.owner}</td>
                    <td>{p.description.unwrap_or_default()}</td>
                    <td class="date">{p.updated_at}</td>
                </tr>
            }
        })
        .collect();
    page_shell(
        "hive-ui — projects",
        view! {
            <h1>"Projects"</h1>
            <p class="meta">{count} " projects"</p>
            <table>
                <thead>
                    <tr>
                        <th>"id"</th>
                        <th>"name"</th>
                        <th>"status"</th>
                        <th>"owner"</th>
                        <th>"description"</th>
                        <th>"updated"</th>
                    </tr>
                </thead>
                <tbody>{rendered}</tbody>
            </table>
        },
    )
}
