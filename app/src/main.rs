// hive — the desktop shell (DIRECTION.md D17).
//
// v0.7 journal + search: the append-only engine (PR 1.6) wired into the
// window. One process — RSX handlers call hive-core in-process; the store's
// writer thread owns the op log + SQLCipher index under the data dir.

use std::path::PathBuf;
use std::sync::Arc;

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_wry_event_handler, Config, WindowBuilder};
use dioxus::prelude::*;
use hive_core::keys::{KeySource, KeychainKeySource, MemoryKeySource};
use hive_core::store::Store;
use hive_embed::HashEmbedder;
use hive_shared::{JournalEntryView, NewJournalEntry, SearchHit};

/// What main() hands the UI: a live store, or the reason there isn't one.
#[derive(Clone)]
enum Boot {
    Ready(Store),
    Failed(String),
}

fn main() {
    let boot = open_store();
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_menu(None).with_window(
                WindowBuilder::new()
                    .with_title("hive")
                    .with_inner_size(LogicalSize::new(1080.0, 740.0)),
            ),
        )
        .with_context(boot)
        .launch(app);
}

/// The store lives under XDG data: the flatpak maps this to
/// `~/.var/app/com.beesroadhouse.Hive/data/hive`, plain runs to
/// `~/.local/share/hive`.
fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        });
    base.join("hive")
}

fn open_store() -> Boot {
    // Master key: resolved from the OS keychain exactly once, here, before
    // the UI exists (created on first boot); the store then works from the
    // in-memory copy so no later wrap/unwrap blocks on D-Bus mid-frame.
    // Inside the flatpak this is the org.freedesktop.secrets hole in the
    // manifest.
    let master = match KeychainKeySource::new().master_key() {
        Ok(key) => key,
        Err(e) => return Boot::Failed(format!("OS keychain unavailable: {e:#}")),
    };
    // Hash embedder: offline and deterministic. The ONNX model becomes a
    // settings choice in Phase 2 — it wants a network hole plus a model
    // download this build deliberately doesn't have (D27), and keyword FTS
    // is the primary retrieval path meanwhile.
    match Store::new(
        &data_dir(),
        Arc::new(MemoryKeySource(master)),
        Arc::new(HashEmbedder),
    ) {
        Ok(store) => Boot::Ready(store),
        Err(e) => Boot::Failed(format!("{e:#}")),
    }
}

/// Journal authorship until identity setup lands (Phase 2 settings; the
/// importer maps the old instance's actor): the OS login name.
fn author_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "owner".to_string())
}

const BG: &str = "#14120e";
const PANEL: &str = "#1a1712";
const EDGE: &str = "#2c2818";
const INK: &str = "#e8e2d4";
const DIM: &str = "#9a927e";
const FAINT: &str = "#6f684f";
const GOLD: &str = "#e2a921";

fn app() -> Element {
    // The titlebar close button must actually quit (dioxus 0.6's default
    // close handling doesn't). process::exit is safe by design here: every
    // committed write is already fsynced in the op log, and a mid-fold kill
    // is exactly the crash-heal path the cutover tests replay.
    use_wry_event_handler(|event, _| {
        if matches!(
            event,
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
        ) {
            std::process::exit(0);
        }
    });

    match use_context::<Boot>() {
        Boot::Ready(store) => rsx! {
            Shell { store }
        },
        Boot::Failed(reason) => rsx! {
            div {
                style: "position: fixed; inset: 0; display: flex; flex-direction: column; \
                        align-items: center; justify-content: center; gap: 0.8rem; \
                        background: {BG}; color: {INK}; font-family: system-ui, sans-serif; \
                        padding: 2rem; text-align: center;",
                div { style: "font-size: 2.6rem; color: {GOLD};", "⬡" }
                div { style: "font-size: 1.3rem; font-weight: 700;", "hive can't open its store" }
                div {
                    style: "max-width: 34rem; color: {DIM}; line-height: 1.6; font-size: 0.92rem;",
                    "{reason}"
                }
            }
        },
    }
}

#[component]
fn Shell(store: ReadOnlySignal<Store>) -> Element {
    let mut draft = use_signal(String::new);
    let mut query = use_signal(String::new);
    let mut status = use_signal(|| Option::<String>::None);
    let mut committed = use_signal(|| 0u32);

    let entries = use_resource(move || {
        let store = store();
        async move {
            let _ = committed(); // re-list after every append
            store
                .journal_list(100, 0)
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    let hits = use_resource(move || {
        let store = store();
        let q = query();
        async move {
            let q = q.trim().to_string();
            if q.is_empty() {
                return Ok(Vec::<SearchHit>::new());
            }
            store.search(&q, 25).await.map_err(|e| format!("{e:#}"))
        }
    });

    let append = move || {
        let body = draft().trim().to_string();
        if body.is_empty() {
            return;
        }
        let store = store();
        spawn(async move {
            let input = NewJournalEntry {
                author: Some(author_name()),
                body,
                tags: None,
                anchors: None,
            };
            match store.journal_append(input, None, None).await {
                Ok(_) => {
                    draft.set(String::new());
                    status.set(None);
                    committed += 1;
                }
                Err(e) => status.set(Some(format!("append failed: {e:#}"))),
            }
        });
    };

    let searching = !query().trim().is_empty();
    let version = env!("CARGO_PKG_VERSION");

    rsx! {
        div {
            style: "position: fixed; inset: 0; overflow-y: auto; background: {BG}; \
                    color: {INK}; font-family: system-ui, sans-serif;",
            div {
                style: "max-width: 760px; margin: 0 auto; padding: 1.4rem 1.2rem 3rem;",

                // header
                div {
                    style: "display: flex; align-items: baseline; gap: 0.55rem; margin-bottom: 1.1rem;",
                    span { style: "font-size: 1.5rem; color: {GOLD};", "⬡" }
                    span { style: "font-size: 1.25rem; font-weight: 700; letter-spacing: 0.04em;", "hive" }
                    span { style: "flex: 1;" }
                    span { style: "font-size: 0.78rem; color: {FAINT};", "v{version} · local-first" }
                }

                // composer
                div {
                    style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                            padding: 0.9rem; margin-bottom: 1rem;",
                    textarea {
                        style: "width: 100%; box-sizing: border-box; min-height: 92px; resize: vertical; \
                                background: {BG}; color: {INK}; border: 1px solid {EDGE}; \
                                border-radius: 8px; padding: 0.7rem 0.8rem; font: inherit; \
                                font-size: 0.95rem; line-height: 1.55; outline: none;",
                        placeholder: "Write to your journal… entities emerge from your words: \
                                      [task: import my old data] [topic: bees] @nate #tag",
                        value: "{draft}",
                        autofocus: true,
                        oninput: move |e| draft.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter && e.modifiers().ctrl() {
                                append();
                            }
                        },
                    }
                    div {
                        style: "display: flex; align-items: center; margin-top: 0.6rem;",
                        span { style: "font-size: 0.78rem; color: {FAINT};", "Ctrl+Enter appends" }
                        span { style: "flex: 1;" }
                        button {
                            style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                    padding: 0.5rem 1.2rem; font-weight: 700; font-size: 0.9rem; \
                                    cursor: pointer;",
                            onclick: move |_| append(),
                            "Append"
                        }
                    }
                }

                // search
                input {
                    style: "width: 100%; box-sizing: border-box; background: {PANEL}; color: {INK}; \
                            border: 1px solid {EDGE}; border-radius: 10px; padding: 0.65rem 0.9rem; \
                            font: inherit; font-size: 0.95rem; outline: none; margin-bottom: 1.1rem;",
                    r#type: "search",
                    placeholder: "Search everything…",
                    value: "{query}",
                    oninput: move |e| query.set(e.value()),
                }

                if let Some(err) = status() {
                    div {
                        style: "color: #e07a5f; font-size: 0.88rem; margin-bottom: 0.9rem;",
                        "{err}"
                    }
                }

                if searching {
                    {search_results(hits())}
                } else {
                    {journal_feed(entries())}
                }
            }
        }
    }
}

// Plain functions, not #[component]s: the shared view structs don't carry
// PartialEq, which component props require for memoization.
fn search_results(hits: Option<Result<Vec<SearchHit>, String>>) -> Element {
    match hits {
        None => muted("searching…"),
        Some(Err(e)) => muted(&format!("search failed: {e}")),
        Some(Ok(hits)) if hits.is_empty() => muted("No matches."),
        Some(Ok(hits)) => rsx! {
            for hit in hits {
                div {
                    key: "{hit.kind}:{hit.id}",
                    style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                            padding: 0.85rem 1rem; margin-bottom: 0.7rem;",
                    div {
                        style: "display: flex; align-items: baseline; gap: 0.6rem;",
                        span {
                            style: "font-size: 0.7rem; font-weight: 700; letter-spacing: 0.08em; \
                                    text-transform: uppercase; color: {GOLD};",
                            "{hit.kind}"
                        }
                        span { style: "font-weight: 600; font-size: 0.98rem;", "{hit.title}" }
                    }
                    // snippet() marks matches with [ ] — plain text, safe to render as-is
                    div {
                        style: "color: {DIM}; font-size: 0.88rem; line-height: 1.55; margin-top: 0.3rem;",
                        "{hit.snippet}"
                    }
                }
            }
        },
    }
}

fn journal_feed(entries: Option<Result<Vec<JournalEntryView>, String>>) -> Element {
    match entries {
        None => muted("opening the journal…"),
        Some(Err(e)) => muted(&format!("journal unavailable: {e}")),
        Some(Ok(entries)) if entries.is_empty() => muted(
            "Nothing here yet. Write the first entry above — wrap intentions in tokens \
             like [task: …] or [topic: …] and hive turns them into entities you can \
             search and track.",
        ),
        Some(Ok(entries)) => rsx! {
            for view in entries {
                {entry_card(&view)}
            }
        },
    }
}

fn entry_card(view: &JournalEntryView) -> Element {
    let e = &view.entry;
    let when = e
        .created_at
        .get(0..16)
        .unwrap_or(&e.created_at)
        .replace('T', " ");
    rsx! {
        div {
            style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                    padding: 0.85rem 1rem; margin-bottom: 0.7rem;",
            div {
                style: "display: flex; gap: 0.6rem; font-size: 0.76rem; color: {FAINT};",
                span { "{when} UTC" }
                span { "· {e.author}" }
            }
            div {
                style: "white-space: pre-wrap; overflow-wrap: anywhere; line-height: 1.6; \
                        font-size: 0.95rem; margin-top: 0.35rem;",
                "{e.body}"
            }
            if !e.tags.is_empty() || !view.anchors.is_empty() || !view.refs.is_empty() {
                div {
                    style: "display: flex; flex-wrap: wrap; gap: 0.4rem; margin-top: 0.55rem;",
                    for tag in e.tags.iter() {
                        span {
                            style: "font-size: 0.74rem; color: {GOLD};",
                            "#{tag}"
                        }
                    }
                    // bracket tokens resolve to refs: the entities this entry emerged
                    for r in view.refs.iter() {
                        span {
                            style: "font-size: 0.74rem; color: {INK}; border: 1px solid {EDGE}; \
                                    border-radius: 999px; padding: 0.1rem 0.55rem; background: {BG};",
                            span { style: "color: {GOLD}; margin-right: 0.3rem;", "{r.kind.as_str()}" }
                            "{r.name}"
                        }
                    }
                    // text-selection anchors (the Phase 2 editor creates these)
                    for anchor in view.anchors.iter() {
                        span {
                            style: "font-size: 0.74rem; color: {DIM}; border: 1px solid {EDGE}; \
                                    border-radius: 999px; padding: 0.1rem 0.55rem;",
                            "{anchor.anchor.text}"
                        }
                    }
                }
            }
        }
    }
}

fn muted(text: &str) -> Element {
    rsx! {
        div {
            style: "color: {FAINT}; font-size: 0.9rem; line-height: 1.6; padding: 1.6rem 0.4rem; \
                    text-align: center;",
            "{text}"
        }
    }
}
