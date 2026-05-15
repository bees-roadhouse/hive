//! Server-rendered views. v1 = leptos `view!` -> `render_to_string`. No
//! hydration, no client JS. v1.5 will introduce islands of interactivity.

use hive_db::types::{JournalEntry, Note, Task, WireEvent};
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
td.tags { font-size: 11px; color: #888; max-width: 16rem; }
td.title a { color: #06d; text-decoration: none; }
td.title a:hover { text-decoration: underline; }
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
        </header>
    }
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
            view! {
                <tr>
                    <td class="id">{id}</td>
                    <td class="date">{e.entry_date}</td>
                    <td class="ai">{e.ai}</td>
                    <td class="title"><a href={href}>{title}</a></td>
                    <td class="tags">{tags}</td>
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

pub fn render_journal_page(entries: Vec<JournalEntry>) -> String {
    page_shell(
        "hive-ui — journal",
        view! {
            <h1>"Journal"</h1>
            {journal_table(entries)}
        },
    )
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
            view! {
                <tr>
                    <td class="id">{t.id}</td>
                    <td class="ai">{t.owner}</td>
                    <td class="ai">{t.status}</td>
                    <td class="ai">{t.priority.unwrap_or_default()}</td>
                    <td class="date">{t.due.unwrap_or_default()}</td>
                    <td>{t.title}</td>
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
            view! {
                <tr>
                    <td class="id">{n.id}</td>
                    <td class="ai">{n.author}</td>
                    <td class="title">{n.title.unwrap_or_default()}</td>
                    <td>{n.body}</td>
                    <td class="tags">{n.tags.unwrap_or_default()}</td>
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
