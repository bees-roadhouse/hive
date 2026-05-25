use maud::{DOCTYPE, Markup, html};

const STYLE: &str = r#"
:root { color-scheme: light dark; --fg: #1a1a1a; --bg: #f8f7f3; --accent: #c87f0a; --muted: #6a6864; --border: #d8d4cc; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #ece9e2; --bg: #16140f; --muted: #9a958a; --border: #2c2820; }
}
* { box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
       color: var(--fg); background: var(--bg); margin: 0; }
header { padding: 0.5rem 1rem; border-bottom: 1px solid var(--border);
         display: flex; gap: 1rem; align-items: baseline; }
header h1 { margin: 0; font-size: 1.1rem; font-weight: 600; }
header nav { display: flex; gap: 0.75rem; flex-wrap: wrap; }
header nav a { color: var(--muted); text-decoration: none; padding: 0.15rem 0.4rem; border-radius: 4px; }
header nav a:hover, header nav a.active { color: var(--accent); background: rgba(200, 127, 10, 0.08); }
main { padding: 1rem; max-width: 1400px; margin: 0 auto; }
h2 { font-size: 1.05rem; margin-top: 0; }
table { width: 100%; border-collapse: collapse; font-size: 0.875rem; }
th { text-align: left; font-weight: 600; padding: 0.4rem 0.6rem; border-bottom: 2px solid var(--border); color: var(--muted); }
td { padding: 0.4rem 0.6rem; border-bottom: 1px solid var(--border); vertical-align: top; }
tr:hover td { background: rgba(200, 127, 10, 0.04); }
.tags { color: var(--muted); font-size: 0.78rem; }
.muted { color: var(--muted); }
.overdue { color: #c12a2a; font-weight: 600; }
.snippet { color: var(--muted); font-size: 0.85rem; padding-left: 1.5rem; }
.snippet b { color: var(--fg); background: rgba(200, 127, 10, 0.18); padding: 0 0.1rem; border-radius: 2px; }
form.inline { display: inline-flex; gap: 0.4rem; align-items: center; }
input, select { font-size: 0.85rem; padding: 0.2rem 0.4rem; background: var(--bg);
                color: var(--fg); border: 1px solid var(--border); border-radius: 3px; }
.empty { color: var(--muted); padding: 2rem; text-align: center; }
.row-id { font-family: ui-monospace, monospace; color: var(--muted); }
.pre { font-family: ui-monospace, monospace; white-space: pre-wrap; font-size: 0.85rem; }
"#;

pub fn page(active: &str, title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "hive ... " (title) }
                style { (maud::PreEscaped(STYLE)) }
            }
            body {
                header {
                    h1 { "hive" }
                    nav {
                        @for (name, label) in &NAV {
                            a href={ "/" (if *name == "home" { "" } else { name }) }
                              class=(if *name == active { "active" } else { "" })
                            { (label) }
                        }
                    }
                }
                main { (body) }
            }
        }
    }
}

const NAV: [(&str, &str); 6] = [
    ("home", "home"),
    ("tasks", "tasks"),
    ("journal", "journal"),
    ("notes", "notes"),
    ("wire", "wire"),
    ("graph", "graph"),
];
