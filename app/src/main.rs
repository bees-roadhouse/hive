// hive — the desktop shell (DIRECTION.md D17).
//
// v0.7 journal + search: the append-only engine (PR 1.6) wired into the
// window. One process — RSX handlers call hive-core in-process; the store's
// writer thread owns the op log + SQLCipher index under the data dir.
//
// First launch (no store under the data dir yet) opens onto onboarding:
// name yourself, then start fresh or import a hosted-era instance in-GUI —
// the hive-import LIBRARY runs in-process (dry-run plan, then the one-shot
// migration) and the window flips to the journal in place, no relaunch.

use std::path::PathBuf;
use std::sync::Arc;

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_wry_event_handler, Config, WindowBuilder};
use dioxus::prelude::*;
use hive_core::keys::{KeySource, KeychainKeySource, MemoryKeySource};
use hive_core::store::Store;
use hive_embed::HashEmbedder;
use hive_import::{Plan, RunOutcome, Summary};
use hive_shared::{ActorKind, JournalEntryView, NewJournalEntry, Person, SearchHit};

/// Config key naming the human behind the hive — the owner display name. The
/// onboarding identity step writes it; Settings edits it; it's the fallback
/// author everywhere. Dotted, matching the config table's conventions
/// (instance.name, search.kind_weights).
const IDENTITY_OWNER_KEY: &str = "identity.owner";

/// Config key naming the ACTIVE identity: the actor (people.slug) the journal
/// composer currently writes as. Defaults to the owner's slug (see
/// `identity.owner`, itself defaulting to $USER). The Identities pane's
/// switcher sets it; the composer passes it as `journal_append`'s
/// `actor_override`.
const IDENTITY_ACTIVE_KEY: &str = "identity.active";

// ── embedder / retrieval config contract ─────────────────────────────────────
//
// SCAFFOLD (Phase 2.1): these keys are WRITTEN by Settings and READ by the
// NEXT PR (the retrieval/embedder swap). This PR only persists them — the live
// engine stays the offline hash embedder (see boot()). The retrieval PR is the
// consumer of this exact schema; the keys and their value domains are the
// contract, so they're named here deliberately and must not drift silently.
//
//   embedder.backend    "hash" (current) | "onnx-local" | "ollama"
//   embedder.model      free text — e.g. "BAAI/bge-small-en-v1.5" (onnx) or an
//                       ollama model tag ("nomic-embed-text"). Empty = backend default.
//   embedder.device     "cpu" | "cuda" | "rocm" | "auto" — ONNX execution
//                       provider (GPU on BOTH NVIDIA (cuda) and AMD (rocm)).
//                       Only meaningful for onnx-local.
//   embedder.ollama_url ollama server base URL (default http://localhost:11434).
//                       Only meaningful for ollama.
//   reranker.enabled    "true" | "false" — cross-encoder rerank stage on/off.
//   reranker.model      free text — cross-encoder model id. Only meaningful when enabled.
const EMBEDDER_BACKEND_KEY: &str = "embedder.backend";
const EMBEDDER_MODEL_KEY: &str = "embedder.model";
const EMBEDDER_DEVICE_KEY: &str = "embedder.device";
const EMBEDDER_OLLAMA_URL_KEY: &str = "embedder.ollama_url";
const RERANKER_ENABLED_KEY: &str = "reranker.enabled";
const RERANKER_MODEL_KEY: &str = "reranker.model";

/// The engine boot() actually wires today — the honest "current" backend value.
const CURRENT_BACKEND: &str = "hash";
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Which top-level section the main pane shows. A root signal drives it; the
/// sidebar sets it. Journal is the app's home (where onboarding lands).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Journal,
    Mail,
    Contacts,
    Calendar,
    Identities,
    Settings,
}

impl Section {
    /// Sidebar order + the (icon, label) each row renders.
    const ALL: [Section; 6] = [
        Section::Journal,
        Section::Mail,
        Section::Contacts,
        Section::Calendar,
        Section::Identities,
        Section::Settings,
    ];

    fn icon(self) -> &'static str {
        match self {
            Section::Journal => "✍",
            Section::Mail => "✉",
            Section::Contacts => "☺",
            Section::Calendar => "▦",
            Section::Identities => "⬡",
            Section::Settings => "⚙",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Section::Journal => "Journal",
            Section::Mail => "Mail",
            Section::Contacts => "Contacts",
            Section::Calendar => "Calendar",
            Section::Identities => "Identities",
            Section::Settings => "Settings",
        }
    }

    /// Stable, distinct DOM id for the sidebar row (xdotool-scriptable).
    fn nav_id(self) -> &'static str {
        match self {
            Section::Journal => "nav-journal",
            Section::Mail => "nav-mail",
            Section::Contacts => "nav-contacts",
            Section::Calendar => "nav-calendar",
            Section::Identities => "nav-identities",
            Section::Settings => "nav-settings",
        }
    }
}

/// What main() hands the UI: a live store, a fresh data dir awaiting
/// onboarding, or the reason there is neither.
#[derive(Clone)]
enum Boot {
    Ready(Store),
    /// The data dir holds no store yet (first launch). Deliberately NOT an
    /// opened store: onboarding may import into this dir, and hive-import
    /// is one-shot — it refuses a dir a store has touched.
    Fresh {
        master: [u8; 32],
        dir: PathBuf,
    },
    Failed(String),
}

fn main() {
    let boot = boot();
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

fn boot() -> Boot {
    // Master key: resolved from the OS keychain exactly once, here, before
    // the UI exists (created on first boot); the store then works from the
    // in-memory copy so no later wrap/unwrap blocks on D-Bus mid-frame.
    // Inside the flatpak this is the org.freedesktop.secrets hole in the
    // manifest. Onboarding never touches the keychain again — it carries
    // these bytes.
    let master = match KeychainKeySource::new().master_key() {
        Ok(key) => key,
        Err(e) => return Boot::Failed(format!("OS keychain unavailable: {e:#}")),
    };
    // First-launch probe: the importer's own fresh-dir rule (device file or
    // op-log segments), shared so the app and hive-import can never disagree
    // about what "fresh" means.
    let dir = data_dir();
    if !hive_import::data_dir_holds_store(&dir) {
        return Boot::Fresh { master, dir };
    }
    // Hash embedder: offline and deterministic. The ONNX model becomes a
    // settings choice in Phase 2 — it wants a model download this build
    // doesn't do, and keyword FTS is the primary retrieval path meanwhile
    // (the network hole exists now, but solely for user-initiated import;
    // see docs/THREAT-MODEL.md).
    match Store::new(
        &dir,
        Arc::new(MemoryKeySource(master)),
        Arc::new(HashEmbedder),
    ) {
        Ok(store) => Boot::Ready(store),
        Err(e) => Boot::Failed(format!("{e:#}")),
    }
}

/// Fallback journal authorship — the OS login name. Prefills the onboarding
/// identity step; covers stores that predate the identity.owner config (and
/// any read failure of it).
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

/// Scoped dark-theme styling for the Markdown-rendered journal bodies. Every
/// rule is prefixed `.md-body` so it only touches rendered entry HTML, not the
/// rest of the shell. Injected once at the Shell root (RSX format strings would
/// eat the braces, so it rides in as an interpolated constant, like SPIN_CSS).
const MD_CSS: &str = "\
.md-body { line-height: 1.6; font-size: 0.95rem; overflow-wrap: anywhere; } \
.md-body > *:first-child { margin-top: 0; } \
.md-body > *:last-child { margin-bottom: 0; } \
.md-body p { margin: 0.5rem 0; } \
.md-body h1, .md-body h2, .md-body h3, .md-body h4 { \
  line-height: 1.3; margin: 0.9rem 0 0.4rem; font-weight: 700; color: #f0ead9; } \
.md-body h1 { font-size: 1.3rem; } .md-body h2 { font-size: 1.15rem; } \
.md-body h3 { font-size: 1.02rem; } .md-body h4 { font-size: 0.95rem; } \
.md-body a { color: #e2a921; text-decoration: underline; } \
.md-body ul, .md-body ol { margin: 0.5rem 0; padding-left: 1.4rem; } \
.md-body li { margin: 0.2rem 0; } \
.md-body li input[type=checkbox] { margin-right: 0.4rem; vertical-align: middle; } \
.md-body code { background: #14120e; border: 1px solid #2c2818; border-radius: 5px; \
  padding: 0.05rem 0.3rem; font-size: 0.88em; } \
.md-body pre { background: #14120e; border: 1px solid #2c2818; border-radius: 8px; \
  padding: 0.7rem 0.85rem; overflow-x: auto; margin: 0.6rem 0; } \
.md-body pre code { background: none; border: none; padding: 0; } \
.md-body blockquote { margin: 0.6rem 0; padding: 0.1rem 0.9rem; color: #9a927e; \
  border-left: 3px solid #2c2818; } \
.md-body table { border-collapse: collapse; margin: 0.6rem 0; display: block; \
  overflow-x: auto; } \
.md-body th, .md-body td { border: 1px solid #2c2818; padding: 0.35rem 0.6rem; text-align: left; } \
.md-body th { background: #1a1712; font-weight: 700; } \
.md-body img { max-width: 100%; border-radius: 6px; } \
.md-body hr { border: none; border-top: 1px solid #2c2818; margin: 0.9rem 0; } \
.md-body h1 { border-bottom: 1px solid #2c2818; padding-bottom: 0.2rem; }";

/// What the window is currently for. Boot picks the initial mode; the
/// onboarding flow moves it to Journal IN PLACE (one process, no relaunch)
/// the moment a store exists.
#[derive(Clone)]
enum Mode {
    Onboarding { master: [u8; 32], dir: PathBuf },
    Journal(Store),
    Failed(String),
}

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

    let boot = use_context::<Boot>();
    let mode = use_signal(move || match boot {
        Boot::Ready(store) => Mode::Journal(store),
        Boot::Fresh { master, dir } => Mode::Onboarding { master, dir },
        Boot::Failed(reason) => Mode::Failed(reason),
    });

    match mode() {
        Mode::Journal(store) => rsx! {
            Shell { store }
        },
        Mode::Onboarding { master, dir } => rsx! {
            Onboarding { mode, master, dir }
        },
        Mode::Failed(reason) => rsx! {
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

// ── onboarding ───────────────────────────────────────────────────────────────

/// Which onboarding panel is showing.
#[derive(Clone, Copy, PartialEq)]
enum OnbStep {
    /// "Who writes here?" — the identity name.
    Identity,
    /// Start fresh vs import.
    Choice,
    /// The import panel (URL, dry run, import).
    Import,
}

/// Import-flow state machine. Every state renders a distinct block under
/// the import panel; the panel's inputs/buttons keep fixed ids and a fixed
/// tab order (URL field first) so the flow is scriptable end to end.
#[derive(Clone)]
enum ImportState {
    /// Inputs live. `err` carries a dry-run/connection failure — those need
    /// no cleanup, because a dry run writes nothing.
    Idle { err: Option<String> },
    /// Dry run in flight.
    Checking,
    /// Dry-run result: the plan card.
    Planned(Plan),
    /// Real import in flight.
    Importing,
    /// Real import failed. The data dir may hold partial state, so the only
    /// way forward is [Clean up and retry].
    ImportFailed(String),
    /// Wiping the partial data dir.
    CleaningUp,
    /// Import complete: summary + [Open my journal].
    Done(Summary),
}

/// Rotation for the working indicator (rsx format strings would eat the
/// braces, so the CSS rides in as an interpolated value).
const SPIN_CSS: &str = "@keyframes onb-spin { to { transform: rotate(360deg); } } \
     .onb-spin { display: inline-block; animation: onb-spin 1.4s linear infinite; }";

/// A store now exists (created fresh, or just imported): open it, record
/// the chosen identity, and flip the window to the journal — in place.
/// Store::new is sync but fast here (empty or freshly folded dir), and the
/// keychain is NOT involved (the master bytes rode in from boot).
async fn enter_journal(
    mut mode: Signal<Mode>,
    master: [u8; 32],
    dir: PathBuf,
    name: String,
) -> Result<(), String> {
    let store = Store::new(
        &dir,
        Arc::new(MemoryKeySource(master)),
        Arc::new(HashEmbedder),
    )
    .map_err(|e| format!("{e:#}"))?;
    let name = name.trim();
    let owner = if name.is_empty() {
        author_name()
    } else {
        name.to_string()
    };
    // On failure the store drops here (writer thread stops, flock releases),
    // so the button that got us here can simply be clicked again.
    store
        .config_set(IDENTITY_OWNER_KEY, &owner)
        .await
        .map_err(|e| format!("recording your name: {e:#}"))?;
    mode.set(Mode::Journal(store));
    Ok(())
}

/// Drive hive_import::run on its own OS thread with a dedicated
/// current-thread tokio runtime — the CLI's exact execution shape. A big
/// mailbox means minutes of blocking Postgres/blockstore work; the UI task
/// only awaits the (runtime-agnostic) oneshot, so the window stays live.
async fn run_import(opts: hive_import::Opts) -> Result<RunOutcome, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("hive-import".to_string())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("building the import runtime: {e:#}"))
                .and_then(|rt| {
                    rt.block_on(hive_import::run(&opts))
                        .map_err(|e| format!("{e:#}"))
                });
            let _ = tx.send(result);
        });
    if spawned.is_err() {
        return Err("could not start the import thread".to_string());
    }
    rx.await
        .unwrap_or_else(|_| Err("the import thread exited without reporting".to_string()))
}

#[component]
fn Onboarding(mode: Signal<Mode>, master: [u8; 32], dir: PathBuf) -> Element {
    let mut name = use_signal(author_name);
    let mut step = use_signal(|| OnbStep::Identity);
    let mut url = use_signal(String::new);
    let mut import_state = use_signal(|| ImportState::Idle { err: None });
    let mut fresh_err = use_signal(|| Option::<String>::None);

    // Handlers each own a clone of the target dir (dioxus event closures
    // are 'static, so they can't borrow the prop).
    let dir_fresh = dir.clone();
    let start_fresh = move |_| {
        let dir = dir_fresh.clone();
        let name = name();
        spawn(async move {
            if let Err(e) = enter_journal(mode, master, dir, name).await {
                fresh_err.set(Some(e));
            }
        });
    };

    let dir_dry = dir.clone();
    let dry_run = move |_| {
        let from = url().trim().to_string();
        if from.is_empty() {
            return;
        }
        let data_dir = dir_dry.clone();
        import_state.set(ImportState::Checking);
        spawn(async move {
            let next = match run_import(hive_import::Opts {
                from,
                data_dir,
                dry_run: true,
                master_key: None, // dry runs read only; no key involved
            })
            .await
            {
                Ok(RunOutcome::Plan(plan)) => ImportState::Planned(plan),
                Ok(RunOutcome::Imported(_)) => ImportState::Idle {
                    err: Some("importer bug: a dry run reported an import".to_string()),
                },
                Err(e) => ImportState::Idle { err: Some(e) },
            };
            import_state.set(next);
        });
    };

    let dir_import = dir.clone();
    let import = move |_| {
        let from = url().trim().to_string();
        if from.is_empty() {
            return;
        }
        let data_dir = dir_import.clone();
        import_state.set(ImportState::Importing);
        spawn(async move {
            let next = match run_import(hive_import::Opts {
                from,
                data_dir,
                dry_run: false,
                master_key: Some(master), // resolved once at boot, before the UI
            })
            .await
            {
                Ok(RunOutcome::Imported(summary)) => ImportState::Done(summary),
                Ok(RunOutcome::Plan(_)) => ImportState::ImportFailed(
                    "importer bug: an import reported a dry-run plan".to_string(),
                ),
                Err(e) => ImportState::ImportFailed(e),
            };
            import_state.set(next);
        });
    };

    let dir_clean = dir.clone();
    let cleanup = move |_| {
        let dir = dir_clean.clone();
        import_state.set(ImportState::CleaningUp);
        spawn(async move {
            // Deleting under the user's data dir is permitted HERE AND ONLY
            // HERE, because all three of these hold:
            //   1. this process verified at boot that the dir held NO store
            //      (Boot::Fresh, via hive_import::data_dir_holds_store);
            //   2. this window has opened no store on it since — every byte
            //      under it was written by the import attempt that just
            //      failed;
            //   3. hive_import::run shut its store down before returning,
            //      so the flock is free and no thread holds the files.
            let result = match std::fs::remove_dir_all(&dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
            .and_then(|()| std::fs::create_dir_all(&dir));
            import_state.set(match result {
                Ok(()) => ImportState::Idle { err: None },
                Err(e) => ImportState::ImportFailed(format!(
                    "clean-up failed: {e} — remove {} by hand and relaunch",
                    dir.display()
                )),
            });
        });
    };

    let dir_open = dir.clone();
    let open_journal = move |_| {
        let dir = dir_open.clone();
        let name = name();
        spawn(async move {
            if let Err(e) = enter_journal(mode, master, dir, name).await {
                import_state.set(ImportState::ImportFailed(format!(
                    "opening the imported store: {e}"
                )));
            }
        });
    };

    let state = import_state();
    let busy = matches!(
        state,
        ImportState::Checking | ImportState::Importing | ImportState::CleaningUp
    );
    let settled = matches!(state, ImportState::ImportFailed(_) | ImportState::Done(_));
    let inputs_off = busy || settled;
    let run_off = inputs_off || url().trim().is_empty();

    rsx! {
        div {
            style: "position: fixed; inset: 0; overflow-y: auto; background: {BG}; \
                    color: {INK}; font-family: system-ui, sans-serif;",
            style { "{SPIN_CSS}" }
            div {
                style: "max-width: 34rem; margin: 0 auto; padding: 3.4rem 1.2rem 3rem;",

                // header
                div {
                    style: "display: flex; align-items: baseline; gap: 0.55rem; margin-bottom: 1.6rem;",
                    span { style: "font-size: 1.5rem; color: {GOLD};", "⬡" }
                    span { style: "font-size: 1.25rem; font-weight: 700; letter-spacing: 0.04em;", "hive" }
                    span { style: "flex: 1;" }
                    span { style: "font-size: 0.78rem; color: {FAINT};", "first launch" }
                }

                match step() {
                    OnbStep::Identity => rsx! {
                        div {
                            style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                                    padding: 1.3rem 1.2rem;",
                            div { style: "font-size: 1.15rem; font-weight: 700;", "Who writes here?" }
                            div {
                                style: "color: {DIM}; font-size: 0.9rem; line-height: 1.6; margin-top: 0.45rem;",
                                "hive signs each journal entry with its author — this is the name \
                                 your writing belongs to."
                            }
                            input {
                                id: "onb-name",
                                style: "width: 100%; box-sizing: border-box; background: {BG}; color: {INK}; \
                                        border: 1px solid {EDGE}; border-radius: 8px; padding: 0.7rem 0.8rem; \
                                        font: inherit; font-size: 0.95rem; outline: none; margin-top: 0.9rem;",
                                value: "{name}",
                                autofocus: true,
                                oninput: move |e| name.set(e.value()),
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter {
                                        step.set(OnbStep::Choice);
                                    }
                                },
                            }
                            div {
                                style: "display: flex; align-items: center; margin-top: 0.9rem;",
                                span { style: "flex: 1;" }
                                button {
                                    id: "onb-continue",
                                    style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                            padding: 0.6rem 1.5rem; font-weight: 700; font-size: 0.95rem; \
                                            cursor: pointer;",
                                    onclick: move |_| step.set(OnbStep::Choice),
                                    "Continue"
                                }
                            }
                        }
                    },

                    OnbStep::Choice => rsx! {
                        div {
                            style: "display: flex; align-items: baseline; gap: 0.5rem; margin-bottom: 0.9rem;",
                            span { style: "font-size: 1.15rem; font-weight: 700;", "How should this hive begin?" }
                            span { style: "flex: 1;" }
                            span { style: "font-size: 0.8rem; color: {FAINT};", "writing as {name}" }
                            button {
                                tabindex: "-1",
                                style: "background: none; border: none; color: {GOLD}; font-size: 0.8rem; \
                                        cursor: pointer; padding: 0;",
                                onclick: move |_| step.set(OnbStep::Identity),
                                "change"
                            }
                        }
                        button {
                            id: "onb-start-fresh",
                            style: "display: block; width: 100%; text-align: left; background: {PANEL}; \
                                    color: {INK}; border: 1px solid {EDGE}; border-radius: 12px; \
                                    padding: 1.1rem 1.2rem; cursor: pointer; font: inherit; \
                                    margin-bottom: 0.8rem;",
                            onclick: start_fresh,
                            div { style: "font-weight: 700; font-size: 1.02rem;", "Start fresh" }
                            div {
                                style: "color: {DIM}; font-size: 0.88rem; line-height: 1.55; margin-top: 0.3rem;",
                                "An empty journal, ready for its first entry."
                            }
                        }
                        button {
                            id: "onb-import-choice",
                            style: "display: block; width: 100%; text-align: left; background: {PANEL}; \
                                    color: {INK}; border: 1px solid {EDGE}; border-radius: 12px; \
                                    padding: 1.1rem 1.2rem; cursor: pointer; font: inherit;",
                            onclick: move |_| step.set(OnbStep::Import),
                            div { style: "font-weight: 700; font-size: 1.02rem;", "Import from my old hive" }
                            div {
                                style: "color: {DIM}; font-size: 0.88rem; line-height: 1.55; margin-top: 0.3rem;",
                                "Copy a hosted-era instance's history — ids and timestamps intact — \
                                 straight from its Postgres database."
                            }
                        }
                        if let Some(err) = fresh_err() {
                            div {
                                style: "color: #e07a5f; font-size: 0.88rem; line-height: 1.55; margin-top: 0.9rem; \
                                        white-space: pre-wrap; overflow-wrap: anywhere;",
                                "{err}"
                            }
                        }
                    },

                    OnbStep::Import => rsx! {
                        div {
                            style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                                    padding: 1.3rem 1.2rem;",
                            div { style: "font-size: 1.15rem; font-weight: 700;", "Import from your old hive" }
                            div {
                                style: "color: {DIM}; font-size: 0.9rem; line-height: 1.6; margin-top: 0.45rem;",
                                "Postgres URL of the legacy instance:"
                            }
                            input {
                                id: "onb-url",
                                style: "width: 100%; box-sizing: border-box; background: {BG}; color: {INK}; \
                                        border: 1px solid {EDGE}; border-radius: 8px; padding: 0.7rem 0.8rem; \
                                        font: inherit; font-size: 0.92rem; outline: none; margin-top: 0.7rem;",
                                placeholder: "postgres://user:pass@host:5432/hive",
                                value: "{url}",
                                autofocus: true,
                                disabled: inputs_off,
                                oninput: move |e| url.set(e.value()),
                            }
                            div {
                                style: "color: {FAINT}; font-size: 0.8rem; line-height: 1.55; margin-top: 0.5rem;",
                                "This is the old server's DATABASE, not the app's web address or an \
                                 API key — reading Postgres directly is what preserves your original \
                                 ids and timestamps."
                            }
                            div {
                                style: "display: flex; gap: 0.7rem; margin-top: 0.9rem;",
                                button {
                                    id: "onb-dry-run",
                                    style: "background: {BG}; color: {INK}; border: 1px solid {EDGE}; \
                                            border-radius: 8px; padding: 0.6rem 1.3rem; font-weight: 600; \
                                            font-size: 0.95rem; cursor: pointer;",
                                    disabled: run_off,
                                    onclick: dry_run,
                                    "Check first (dry run)"
                                }
                                button {
                                    id: "onb-import",
                                    style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                            padding: 0.6rem 1.5rem; font-weight: 700; font-size: 0.95rem; \
                                            cursor: pointer;",
                                    disabled: run_off,
                                    onclick: import,
                                    "Import"
                                }
                                span { style: "flex: 1;" }
                                if matches!(state, ImportState::Idle { .. } | ImportState::Planned(_)) {
                                    button {
                                        tabindex: "-1",
                                        style: "background: none; border: none; color: {FAINT}; \
                                                font-size: 0.85rem; cursor: pointer;",
                                        onclick: move |_| step.set(OnbStep::Choice),
                                        "back"
                                    }
                                }
                            }

                            match state {
                                ImportState::Idle { err: None } => rsx! {},
                                ImportState::Idle { err: Some(err) } => rsx! {
                                    {import_error(&err)}
                                },
                                ImportState::Checking => working("connecting and counting — nothing is written by a dry run…"),
                                ImportState::Planned(plan) => plan_card(&plan),
                                ImportState::Importing => working("importing — this can take a while on big mailboxes…"),
                                ImportState::CleaningUp => working("cleaning up…"),
                                ImportState::ImportFailed(err) => rsx! {
                                    {import_error(&err)}
                                    button {
                                        id: "onb-retry",
                                        style: "background: {BG}; color: {INK}; border: 1px solid {EDGE}; \
                                                border-radius: 8px; padding: 0.6rem 1.3rem; font-weight: 600; \
                                                font-size: 0.95rem; cursor: pointer; margin-top: 0.8rem;",
                                        onclick: cleanup,
                                        "Clean up and retry"
                                    }
                                },
                                ImportState::Done(summary) => rsx! {
                                    {summary_card(&summary)}
                                    button {
                                        id: "onb-open-journal",
                                        style: "background: {GOLD}; color: #14120e; border: none; \
                                                border-radius: 8px; padding: 0.7rem 1.6rem; font-weight: 700; \
                                                font-size: 1rem; cursor: pointer; margin-top: 0.9rem;",
                                        onclick: open_journal,
                                        "Open my journal"
                                    }
                                },
                            }
                        }
                    },
                }
            }
        }
    }
}

// Plain render helpers (same rule as the journal ones below: shared structs
// carry no PartialEq, so these stay out of #[component]).

fn import_error(err: &str) -> Element {
    rsx! {
        div {
            id: "onb-import-error",
            style: "color: #e07a5f; font-size: 0.88rem; line-height: 1.55; margin-top: 0.9rem; \
                    white-space: pre-wrap; overflow-wrap: anywhere;",
            "{err}"
        }
    }
}

fn working(text: &str) -> Element {
    rsx! {
        div {
            id: "onb-working",
            style: "display: flex; align-items: center; gap: 0.6rem; margin-top: 0.9rem; \
                    color: {DIM}; font-size: 0.9rem;",
            span { class: "onb-spin", style: "color: {GOLD}; font-size: 1.1rem;", "⬡" }
            "{text}"
        }
    }
}

/// The dry-run result: hive_import::Plan::grouped(), one row per group.
fn plan_card(plan: &Plan) -> Element {
    let total = plan.total_rows();
    rsx! {
        div {
            id: "onb-plan",
            style: "background: {BG}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.85rem 1rem; margin-top: 0.9rem;",
            div {
                style: "font-size: 0.7rem; font-weight: 700; letter-spacing: 0.08em; \
                        text-transform: uppercase; color: {GOLD}; margin-bottom: 0.45rem;",
                "found in your old hive"
            }
            for (label, n) in plan.grouped() {
                div {
                    key: "{label}",
                    style: "display: flex; font-size: 0.9rem; line-height: 1.75;",
                    span { style: "color: {DIM};", "{label}" }
                    span { style: "flex: 1;" }
                    span { "{n}" }
                }
            }
            div {
                style: "display: flex; font-size: 0.9rem; line-height: 1.75; font-weight: 600; \
                        border-top: 1px solid {EDGE}; margin-top: 0.35rem; padding-top: 0.35rem;",
                span { style: "color: {DIM};", "total source rows" }
                span { style: "flex: 1;" }
                span { "{total}" }
            }
            div {
                style: "color: {FAINT}; font-size: 0.8rem; margin-top: 0.5rem;",
                "Checked only — nothing is written until you import."
            }
        }
    }
}

/// The import result: what landed in the new store.
fn summary_card(summary: &Summary) -> Element {
    rsx! {
        div {
            id: "onb-summary",
            style: "background: {BG}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.85rem 1rem; margin-top: 0.9rem;",
            div {
                style: "font-size: 0.7rem; font-weight: 700; letter-spacing: 0.08em; \
                        text-transform: uppercase; color: {GOLD}; margin-bottom: 0.45rem;",
                "import complete"
            }
            div {
                style: "display: flex; font-size: 0.9rem; line-height: 1.75;",
                span { style: "color: {DIM};", "records written" }
                span { style: "flex: 1;" }
                span { "{summary.records}" }
            }
            div {
                style: "display: flex; font-size: 0.9rem; line-height: 1.75;",
                span { style: "color: {DIM};", "attachment blobs stored" }
                span { style: "flex: 1;" }
                span { "{summary.blobs_stored}" }
            }
            div {
                style: "display: flex; font-size: 0.9rem; line-height: 1.75;",
                span { style: "color: {DIM};", "search (FTS) rows" }
                span { style: "flex: 1;" }
                span { "{summary.mail_fts_rows}" }
            }
            if summary.mail_deleted_skipped > 0 || summary.inbox_read_skipped > 0 {
                div {
                    style: "color: {FAINT}; font-size: 0.8rem; margin-top: 0.5rem; line-height: 1.5;",
                    "Left behind on purpose: {summary.mail_deleted_skipped} deleted mail message(s), \
                     {summary.inbox_read_skipped} already-read inbox row(s)."
                }
            }
            div {
                style: "color: {FAINT}; font-size: 0.8rem; margin-top: 0.5rem; line-height: 1.5;",
                "Search is keyword-first and ready now. Semantic embeddings are \
                 configured in Settings and arrive with the next update."
            }
        }
    }
}

/// The app shell: an Apple-Mail-style fixed sidebar + a main pane that switches
/// on the `Section` root signal, in place (one process, no relaunch). The
/// journal (composer + feed + search) is one pane; identities and settings are
/// the others; mail/contacts/calendar are honest placeholders.
#[component]
fn Shell(store: ReadOnlySignal<Store>) -> Element {
    // Root signals the whole shell shares. `section` drives the main pane.
    let section = use_signal(|| Section::Journal);
    // `active` is the identity the composer writes as. Persisted to
    // identity.active; loaded once at mount, defaulting to the owner slug.
    // `refresh` bumps to re-pull the identity roster + active value after a
    // create/switch, without a relaunch.
    let active = use_signal(String::new);
    let refresh = use_signal(|| 0u32);

    // Resolve the active identity once: identity.active if set, else the owner
    // (identity.owner, itself defaulting to $USER). Writes it back into the
    // `active` signal the composer + panes read.
    let _init = use_resource(move || {
        let store = store();
        let mut active = active;
        async move {
            let resolved = resolve_active_identity(&store).await;
            if active.peek().is_empty() {
                active.set(resolved);
            }
        }
    });

    rsx! {
        div {
            style: "position: fixed; inset: 0; display: flex; background: {BG}; \
                    color: {INK}; font-family: system-ui, sans-serif;",
            // Scoped styling for the Markdown-rendered journal bodies.
            style { "{MD_CSS}" }

            Sidebar { store, section, active, refresh }

            // Main pane — scrolls independently of the sidebar.
            div {
                id: "main-pane",
                style: "flex: 1; min-width: 0; height: 100%; overflow-y: auto;",
                match section() {
                    Section::Journal => rsx! { JournalPane { store, active } },
                    Section::Identities => rsx! {
                        IdentitiesPane { store, section, active, refresh }
                    },
                    Section::Settings => rsx! { SettingsPane { store, refresh } },
                    Section::Mail => placeholder_pane(
                        Section::Mail,
                        "Connect your mail server and give each identity its own mailbox — \
                         send and receive as any of your identities, with every message \
                         woven into the same searchable memory as your journal.",
                    ),
                    Section::Contacts => placeholder_pane(
                        Section::Contacts,
                        "A CardDAV client: your address book synced from the servers you \
                         already use, with people linked to the journal entries, tasks, and \
                         mail they appear in.",
                    ),
                    Section::Calendar => placeholder_pane(
                        Section::Calendar,
                        "A CalDAV client: your calendars synced from the servers you already \
                         use, events sitting alongside the journal so your week and your \
                         writing share one timeline.",
                    ),
                }
            }
        }
    }
}

/// The fixed-width left navigation. Sidebar rows set the `section` signal; the
/// active-identity card at the bottom jumps to Identities. The card's badge
/// comes from the active identity's ACTUAL kind (people_get), not the hardcoded
/// cast, so a custom AI identity still reads "AI"; `refresh` re-pulls it after
/// a create/switch.
#[component]
fn Sidebar(
    store: ReadOnlySignal<Store>,
    section: Signal<Section>,
    active: Signal<String>,
    refresh: Signal<u32>,
) -> Element {
    let current = section();
    let active_name = active();

    // The active identity's kind, resolved from the store (falls back to the
    // shared cast, then Human, for a bare-slug author with no people row).
    let active_kind = use_resource(move || {
        let store = store();
        let who = active();
        async move {
            let _ = refresh();
            match store.people_get(&who).await {
                Ok(Some(p)) => p.kind,
                _ if hive_shared::is_ai(&who) => ActorKind::Ai,
                _ => ActorKind::Human,
            }
        }
    });
    let is_ai = active_kind() == Some(ActorKind::Ai);
    let (badge, badge_bg) = if is_ai { ("AI", GOLD) } else { ("you", FAINT) };
    let shown = if active_name.trim().is_empty() {
        author_name()
    } else {
        active_name.clone()
    };
    let version = env!("CARGO_PKG_VERSION");

    rsx! {
        div {
            id: "sidebar",
            style: "width: 200px; min-width: 200px; height: 100%; box-sizing: border-box; \
                    display: flex; flex-direction: column; background: {PANEL}; \
                    border-right: 1px solid {EDGE}; overflow-y: auto;",

            // brand
            div {
                style: "display: flex; align-items: baseline; gap: 0.5rem; padding: 1.1rem 1rem 0.9rem;",
                span { style: "font-size: 1.35rem; color: {GOLD};", "⬡" }
                span { style: "font-size: 1.1rem; font-weight: 700; letter-spacing: 0.04em;", "hive" }
            }

            // sections
            div {
                style: "flex: 1; padding: 0.2rem 0.5rem;",
                for s in Section::ALL {
                    {sidebar_row(s, s == current, section)}
                }
            }

            // active-identity card → Identities
            button {
                id: "active-identity",
                style: "display: flex; align-items: center; gap: 0.55rem; text-align: left; \
                        margin: 0.5rem; padding: 0.55rem 0.65rem; border-radius: 10px; \
                        background: {BG}; border: 1px solid {EDGE}; cursor: pointer; \
                        color: {INK}; font: inherit;",
                onclick: move |_| section.set(Section::Identities),
                span {
                    style: "display: inline-flex; align-items: center; justify-content: center; \
                            width: 1.9rem; height: 1.9rem; border-radius: 50%; background: {EDGE}; \
                            color: {GOLD}; font-size: 0.95rem; flex-shrink: 0;",
                    "⬡"
                }
                span {
                    style: "flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; \
                            white-space: nowrap; font-size: 0.9rem; font-weight: 600;",
                    "{shown}"
                }
                span {
                    style: "font-size: 0.62rem; font-weight: 700; letter-spacing: 0.05em; \
                            text-transform: uppercase; color: {BG}; background: {badge_bg}; \
                            border-radius: 5px; padding: 0.1rem 0.35rem; flex-shrink: 0;",
                    "{badge}"
                }
            }
            div {
                style: "padding: 0 1rem 0.8rem; font-size: 0.68rem; color: {FAINT};",
                "v{version} · local-first"
            }
        }
    }
}

/// One sidebar navigation row.
fn sidebar_row(s: Section, active: bool, mut section: Signal<Section>) -> Element {
    let (fg, bg, weight) = if active {
        (INK, EDGE, "700")
    } else {
        (DIM, "transparent", "500")
    };
    rsx! {
        button {
            id: s.nav_id(),
            style: "display: flex; align-items: center; gap: 0.6rem; width: 100%; text-align: left; \
                    padding: 0.5rem 0.6rem; margin-bottom: 0.15rem; border: none; border-radius: 8px; \
                    background: {bg}; color: {fg}; font: inherit; font-size: 0.92rem; \
                    font-weight: {weight}; cursor: pointer;",
            onclick: move |_| section.set(s),
            span { style: "width: 1.2rem; text-align: center; color: {GOLD};", "{s.icon()}" }
            span { "{s.label()}" }
        }
    }
}

/// Resolve the active identity slug: identity.active if recorded and non-empty,
/// else the owner (identity.owner, itself defaulting to author_name()/$USER).
async fn resolve_active_identity(store: &Store) -> String {
    if let Ok(Some(active)) = store.config_get(IDENTITY_ACTIVE_KEY).await {
        if !active.trim().is_empty() {
            return active;
        }
    }
    owner_name(store).await
}

/// The owner display name: identity.owner, falling back to $USER.
async fn owner_name(store: &Store) -> String {
    match store.config_get(IDENTITY_OWNER_KEY).await {
        Ok(Some(owner)) if !owner.trim().is_empty() => owner,
        _ => author_name(),
    }
}

// ── journal pane (the existing composer + feed + search, moved verbatim) ─────

/// The Journal section: composer + feed + search, exactly as it worked before
/// the shell existed — plus an identity byline (writes as the active identity)
/// and an All/active author filter above the feed.
#[component]
fn JournalPane(store: ReadOnlySignal<Store>, active: Signal<String>) -> Element {
    let mut draft = use_signal(String::new);
    let mut query = use_signal(String::new);
    let mut status = use_signal(|| Option::<String>::None);
    let mut committed = use_signal(|| 0u32);
    // Feed filter: false = all identities, true = only the active one.
    let mut only_mine = use_signal(|| false);

    let entries = use_resource(move || {
        let store = store();
        let mine = only_mine();
        let who = active();
        async move {
            let _ = committed(); // re-list after every append
            if mine && !who.trim().is_empty() {
                store
                    .journal_list_by_author(&who, 100, 0)
                    .await
                    .map_err(|e| format!("{e:#}"))
            } else {
                store
                    .journal_list(100, 0)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
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
        // Write as the ACTIVE identity: pass it as actor_override so the entry
        // is authored by whichever identity is selected (author_name() only if
        // none resolved, matching the pre-shell fallback).
        let byline = {
            let a = active();
            if a.trim().is_empty() {
                author_name()
            } else {
                a
            }
        };
        spawn(async move {
            let input = NewJournalEntry {
                author: Some(byline.clone()),
                body,
                tags: None,
                anchors: None,
            };
            match store.journal_append(input, Some(&byline), None).await {
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
    let active_name = active();
    let mine = only_mine();

    rsx! {
        div {
            style: "max-width: 760px; margin: 0 auto; padding: 1.4rem 1.2rem 3rem;",

            // composer
            div {
                style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                        padding: 0.9rem; margin-bottom: 1rem;",
                div {
                    style: "display: flex; align-items: baseline; gap: 0.4rem; margin-bottom: 0.6rem; \
                            font-size: 0.78rem; color: {FAINT};",
                    "writing as"
                    span { style: "color: {GOLD}; font-weight: 600;", "{active_name}" }
                }
                textarea {
                    id: "journal-composer",
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
                        id: "journal-append",
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
                id: "journal-search",
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
                // feed filter: All identities vs just the active one
                div {
                    style: "display: inline-flex; margin-bottom: 0.9rem; border: 1px solid {EDGE}; \
                            border-radius: 8px; overflow: hidden;",
                    button {
                        id: "feed-filter-all",
                        style: segmented_style(!mine),
                        onclick: move |_| only_mine.set(false),
                        "All identities"
                    }
                    button {
                        id: "feed-filter-mine",
                        style: segmented_style(mine),
                        onclick: move |_| only_mine.set(true),
                        "{active_name}"
                    }
                }
                {journal_feed(entries())}
            }
        }
    }
}

/// One segment button in the feed filter toggle.
fn segmented_style(on: bool) -> String {
    let (fg, bg) = if on {
        ("#14120e", GOLD)
    } else {
        (DIM, "transparent")
    };
    format!(
        "background: {bg}; color: {fg}; border: none; padding: 0.4rem 0.9rem; \
         font: inherit; font-size: 0.82rem; font-weight: 600; cursor: pointer;"
    )
}

// ── placeholder panes (mail / contacts / calendar — honest "coming soon") ────

/// An honest placeholder: the section icon, "Coming in a later update", and one
/// true sentence of what the section will do. No fake UI.
fn placeholder_pane(s: Section, blurb: &str) -> Element {
    rsx! {
        div {
            id: "placeholder-pane",
            style: "max-width: 560px; margin: 0 auto; padding: 5rem 1.4rem; text-align: center;",
            div { style: "font-size: 3rem; color: {GOLD};", "{s.icon()}" }
            div {
                style: "font-size: 1.5rem; font-weight: 700; margin-top: 0.6rem;",
                "{s.label()}"
            }
            div {
                style: "font-size: 0.82rem; font-weight: 700; letter-spacing: 0.08em; \
                        text-transform: uppercase; color: {DIM}; margin-top: 0.5rem;",
                "Coming in a later update"
            }
            div {
                style: "color: {DIM}; font-size: 0.95rem; line-height: 1.65; margin-top: 1rem;",
                "{blurb}"
            }
        }
    }
}

// ── identities pane ──────────────────────────────────────────────────────────

/// The Identities section: the roster of all authors (Human/AI badge, name,
/// slug, entry count), a create-AI-identity field, and the active-identity
/// switcher. Creating or switching bumps `refresh` so the list re-pulls.
#[component]
fn IdentitiesPane(
    store: ReadOnlySignal<Store>,
    section: Signal<Section>,
    active: Signal<String>,
    refresh: Signal<u32>,
) -> Element {
    let mut new_name = use_signal(String::new);
    let mut err = use_signal(|| Option::<String>::None);

    // The roster: every author. people_list is the identity table (the actors,
    // human + AI). journal_writers is unioned in so an imported author who has
    // entries but somehow no people row still shows. Re-pulled when `refresh`
    // bumps (after a create or switch). No per-actor counts: journal_writers
    // carries none and per-author COUNTs would be N queries (spec: skip).
    let roster = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            let mut people = store.people_list().await.map_err(|e| format!("{e:#}"))?;
            let have: std::collections::HashSet<String> =
                people.iter().map(|p| p.slug.clone()).collect();
            if let Ok(writers) = store.journal_writers().await {
                for w in writers {
                    if !have.contains(&w.slug) {
                        people.push(Person {
                            id: format!("writer:{}", w.slug),
                            slug: w.slug,
                            name: w.name,
                            kind: w.kind,
                            owner: w.owner,
                            bio: None,
                            role: None,
                            created_at: String::new(),
                        });
                    }
                }
            }
            people.sort_by(|a, b| (a.kind.as_str(), &a.slug).cmp(&(b.kind.as_str(), &b.slug)));
            Ok::<_, String>(people)
        }
    });

    let create = move || {
        let name = new_name().trim().to_string();
        if name.is_empty() {
            return;
        }
        let store = store();
        let mut refresh = refresh;
        spawn(async move {
            match store.identity_create(&name, ActorKind::Ai).await {
                Ok(_) => {
                    new_name.set(String::new());
                    err.set(None);
                    refresh += 1;
                }
                Err(e) => err.set(Some(format!("{e:#}"))),
            }
        });
    };

    rsx! {
        div {
            style: "max-width: 760px; margin: 0 auto; padding: 1.6rem 1.2rem 3rem;",
            {pane_header("Identities", "The humans and AIs who write in this hive. Each identity \
                                       authors its own journal entries; the active one is who the \
                                       composer writes as.")}

            // create a new AI identity
            div {
                style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                        padding: 1rem 1.1rem; margin: 1rem 0 1.3rem;",
                div { style: "font-weight: 700; font-size: 1rem;", "New AI identity" }
                div {
                    style: "color: {DIM}; font-size: 0.86rem; line-height: 1.55; margin-top: 0.3rem;",
                    "A name your AI writes under — it becomes an author you can switch to and \
                     mention. "
                    // Phase 3: per-identity mail credentials hang off the identity here.
                }
                div {
                    style: "display: flex; gap: 0.6rem; margin-top: 0.8rem;",
                    input {
                        id: "identity-new-name",
                        style: "flex: 1; box-sizing: border-box; background: {BG}; color: {INK}; \
                                border: 1px solid {EDGE}; border-radius: 8px; padding: 0.6rem 0.75rem; \
                                font: inherit; font-size: 0.92rem; outline: none;",
                        placeholder: "e.g. Apis",
                        value: "{new_name}",
                        oninput: move |e| new_name.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                create();
                            }
                        },
                    }
                    button {
                        id: "identity-create",
                        style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                padding: 0.6rem 1.3rem; font-weight: 700; font-size: 0.9rem; cursor: pointer;",
                        onclick: move |_| create(),
                        "Create"
                    }
                }
                if let Some(e) = err() {
                    div {
                        style: "color: #e07a5f; font-size: 0.85rem; margin-top: 0.6rem;",
                        "{e}"
                    }
                }
            }

            // the roster
            match roster() {
                None => muted("loading identities…"),
                Some(Err(e)) => muted(&format!("identities unavailable: {e}")),
                Some(Ok(people)) => rsx! {
                    div {
                        id: "identity-list",
                        for person in people.iter() {
                            {identity_row(store, person, active, section)}
                        }
                    }
                },
            }
        }
    }
}

/// One identity row: badge + name + slug, with a Switch/Active control that
/// persists identity.active (and hops to the Journal so the switch is felt).
/// Plain fn: Person and the signals lack the PartialEq component props need.
fn identity_row(
    store: ReadOnlySignal<Store>,
    person: &Person,
    mut active: Signal<String>,
    mut section: Signal<Section>,
) -> Element {
    let is_ai = person.kind == ActorKind::Ai;
    let (badge, badge_bg) = if is_ai { ("AI", GOLD) } else { ("you", FAINT) };
    let is_active = active() == person.slug;
    let slug = person.slug.clone();
    // Phase 3: per-identity mail credentials hang off the identity here.

    rsx! {
        div {
            // A stable, per-slug id so a script can target a specific identity row.
            id: "identity-row-{person.slug}",
            style: "display: flex; align-items: center; gap: 0.7rem; background: {PANEL}; \
                    border: 1px solid {EDGE}; border-radius: 10px; padding: 0.7rem 0.9rem; \
                    margin-bottom: 0.6rem;",
            span {
                style: "display: inline-flex; align-items: center; justify-content: center; \
                        width: 2.1rem; height: 2.1rem; border-radius: 50%; background: {EDGE}; \
                        color: {GOLD}; font-size: 1rem; flex-shrink: 0;",
                "⬡"
            }
            div {
                style: "flex: 1; min-width: 0;",
                div {
                    style: "display: flex; align-items: baseline; gap: 0.5rem;",
                    span { style: "font-weight: 700; font-size: 0.98rem;", "{person.name}" }
                    span {
                        style: "font-size: 0.62rem; font-weight: 700; letter-spacing: 0.05em; \
                                text-transform: uppercase; color: {BG}; background: {badge_bg}; \
                                border-radius: 5px; padding: 0.1rem 0.35rem;",
                        "{badge}"
                    }
                }
                div {
                    style: "font-size: 0.78rem; color: {FAINT}; margin-top: 0.15rem;",
                    "{person.slug}"
                }
            }
            if is_active {
                span {
                    style: "font-size: 0.75rem; font-weight: 700; color: {GOLD}; \
                            border: 1px solid {GOLD}; border-radius: 999px; padding: 0.2rem 0.7rem;",
                    "active"
                }
            } else {
                button {
                    id: "identity-switch-{person.slug}",
                    style: "background: {BG}; color: {INK}; border: 1px solid {EDGE}; \
                            border-radius: 999px; padding: 0.25rem 0.8rem; font: inherit; \
                            font-size: 0.78rem; font-weight: 600; cursor: pointer;",
                    onclick: move |_| {
                        let slug = slug.clone();
                        let store = store();
                        // Persist identity.active, reflect it locally, and hop to
                        // the Journal so the composer immediately writes as it.
                        active.set(slug.clone());
                        section.set(Section::Journal);
                        spawn(async move {
                            let _ = store.config_set(IDENTITY_ACTIVE_KEY, &slug).await;
                        });
                    },
                    "Switch"
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
            // The body renders as sanitized GFM Markdown. The source is authored
            // by the user AND by AI identities and displays inside the app's own
            // WebKit context, so it is rendered → SANITIZED (D17) before it ever
            // reaches dangerous_inner_html. Bracket tokens ([task: …]) and
            // @mentions/#tags survive as literal text (markdown leaves `[x]`
            // without a following `(url)` alone), so the emergence chips below
            // still line up with what's written.
            div {
                class: "md-body",
                style: "margin-top: 0.35rem;",
                dangerous_inner_html: "{render_markdown(&e.body)}",
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

/// A pane's title + one-line description block (Identities/Settings headers).
fn pane_header(title: &str, blurb: &str) -> Element {
    rsx! {
        div {
            div { style: "font-size: 1.4rem; font-weight: 700;", "{title}" }
            div {
                style: "color: {DIM}; font-size: 0.9rem; line-height: 1.55; margin-top: 0.3rem;",
                "{blurb}"
            }
        }
    }
}

// ── markdown rendering (sanitized; the body is untrusted, D17) ────────────────

/// Render a journal body (GFM Markdown, authored by the user AND by AI
/// identities) to HTML that is SAFE to inject via dangerous_inner_html.
///
/// Pipeline: comrak (GFM: task lists, tables, strikethrough, autolinks, fenced
/// code) → ammonia sanitize. Sanitizing is mandatory (D17): this HTML renders
/// inside the app's own WebKit context, so unsanitized markup is a live XSS
/// surface. The ammonia policy is a strict allowlist — no `<script>`, no event
/// handlers, no remote resource loads:
///   - `img`/`iframe` are dropped entirely (no network fetch, no tracking px).
///   - links are kept but neutralized: only http/https/mailto survive, and each
///     gets `target=_blank` + `rel="noopener noreferrer nofollow"` so a click
///     opens the OS browser and can NEVER navigate the app webview.
fn render_markdown(md: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.tagfilter = true; // neutralize raw <script>/<style>/etc.
                                        // Do NOT enable unsafe_ raw-HTML passthrough: raw HTML stays escaped by
                                        // comrak, and ammonia is the second, authoritative gate regardless.
    let html = comrak::markdown_to_html(md, &options);

    ammonia::Builder::default()
        .add_generic_attributes(["class"]) // comrak stamps e.g. task-list classes
        .rm_tags(["img"]) // no remote images / tracking pixels
        .add_tags(["input"]) // GFM task-list checkboxes
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .link_rel(Some("noopener noreferrer nofollow"))
        .add_tag_attributes("a", ["target"])
        .url_schemes(
            ["http", "https", "mailto"]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
        )
        .clean(&html)
        .to_string()
}

// ── settings pane ────────────────────────────────────────────────────────────

/// The Settings section: the owner display name (identity.owner), and the
/// retrieval/embeddings config the NEXT PR consumes (saved-as-config scaffold;
/// the live engine is unchanged this PR). See the EMBEDDER_* key block at the
/// top of this file for the exact schema the retrieval PR reads.
#[component]
fn SettingsPane(store: ReadOnlySignal<Store>, refresh: Signal<u32>) -> Element {
    // Load every persisted value once, into editable signals.
    let mut owner = use_signal(String::new);
    let mut backend = use_signal(|| CURRENT_BACKEND.to_string());
    let mut model = use_signal(String::new);
    let mut device = use_signal(|| "auto".to_string());
    let mut ollama_url = use_signal(|| DEFAULT_OLLAMA_URL.to_string());
    let mut rerank_on = use_signal(|| false);
    let mut rerank_model = use_signal(String::new);
    let mut saved = use_signal(|| false);

    let _load = use_resource(move || {
        let store = store();
        async move {
            owner.set(owner_name(&store).await);
            if let Ok(Some(v)) = store.config_get(EMBEDDER_BACKEND_KEY).await {
                if !v.is_empty() {
                    backend.set(v);
                }
            }
            if let Ok(Some(v)) = store.config_get(EMBEDDER_MODEL_KEY).await {
                model.set(v);
            }
            if let Ok(Some(v)) = store.config_get(EMBEDDER_DEVICE_KEY).await {
                if !v.is_empty() {
                    device.set(v);
                }
            }
            if let Ok(Some(v)) = store.config_get(EMBEDDER_OLLAMA_URL_KEY).await {
                if !v.is_empty() {
                    ollama_url.set(v);
                }
            }
            if let Ok(v) = store.config_bool(RERANKER_ENABLED_KEY).await {
                rerank_on.set(v);
            }
            if let Ok(Some(v)) = store.config_get(RERANKER_MODEL_KEY).await {
                rerank_model.set(v);
            }
        }
    });

    // Embedding corpus truth: total embedded vs embeddable (cheap; one stat).
    let stats = use_resource(move || {
        let store = store();
        async move { store.embedding_stats().await.ok() }
    });

    let save = move || {
        let store = store();
        let mut refresh = refresh;
        let vals = [
            (IDENTITY_OWNER_KEY, owner().trim().to_string()),
            (EMBEDDER_BACKEND_KEY, backend()),
            (EMBEDDER_MODEL_KEY, model().trim().to_string()),
            (EMBEDDER_DEVICE_KEY, device()),
            (EMBEDDER_OLLAMA_URL_KEY, ollama_url().trim().to_string()),
            (
                RERANKER_ENABLED_KEY,
                if rerank_on() { "true" } else { "false" }.to_string(),
            ),
            (RERANKER_MODEL_KEY, rerank_model().trim().to_string()),
        ];
        spawn(async move {
            for (k, v) in vals {
                let _ = store.config_set(k, &v).await;
            }
            saved.set(true);
            // Owner may have changed — nudge the shell to re-read the roster/name.
            refresh += 1;
        });
    };

    let backend_val = backend();
    let is_onnx = backend_val == "onnx-local";
    let is_ollama = backend_val == "ollama";

    rsx! {
        div {
            style: "max-width: 640px; margin: 0 auto; padding: 1.6rem 1.2rem 3rem;",
            {pane_header("Settings", "How this hive knows you, and how it will search your memory.")}

            // ── Identity ──
            div {
                style: settings_card_style(),
                div { style: "font-weight: 700; font-size: 1.02rem;", "Identity" }
                div {
                    style: "color: {DIM}; font-size: 0.86rem; line-height: 1.55; margin-top: 0.25rem;",
                    "Your display name — the human behind the hive."
                }
                {field_label("Owner name")}
                input {
                    id: "settings-owner",
                    style: text_input_style(),
                    value: "{owner}",
                    oninput: move |e| { owner.set(e.value()); saved.set(false); },
                }
            }

            // ── Retrieval / Embeddings ──
            div {
                style: settings_card_style(),
                div { style: "font-weight: 700; font-size: 1.02rem;", "Retrieval & Embeddings" }

                // honest current state
                div {
                    id: "embedder-current-state",
                    style: "background: {BG}; border: 1px solid {EDGE}; border-radius: 8px; \
                            padding: 0.7rem 0.85rem; margin-top: 0.6rem; font-size: 0.85rem; \
                            line-height: 1.55; color: {DIM};",
                    div {
                        "Current engine: "
                        span { style: "color: {INK}; font-weight: 600;", "keyword search + hash vectors, running on CPU." }
                    }
                    div {
                        style: "margin-top: 0.3rem;",
                        "Your imported entries are "
                        span { style: "color: {INK}; font-weight: 600;", "not semantically embedded yet." }
                    }
                    if let Some(Some(s)) = stats() {
                        div {
                            style: "margin-top: 0.4rem; color: {FAINT};",
                            "Embedded: {s.total} of {s.embeddable} embeddable item(s)."
                        }
                    }
                }

                // forward config — the contract the retrieval PR reads
                {field_label("Embedding backend")}
                select {
                    id: "settings-embedder-backend",
                    style: text_input_style(),
                    value: "{backend_val}",
                    onchange: move |e| { backend.set(e.value()); saved.set(false); },
                    option { value: "hash", "Hash vectors (current — offline, CPU)" }
                    option { value: "onnx-local", "Local ONNX model (on-device, GPU-capable)" }
                    option { value: "ollama", "Ollama (local server)" }
                }

                if is_onnx || is_ollama {
                    {field_label("Model")}
                    input {
                        id: "settings-embedder-model",
                        style: text_input_style(),
                        placeholder: if is_ollama { "e.g. nomic-embed-text" } else { "e.g. BAAI/bge-small-en-v1.5" },
                        value: "{model}",
                        oninput: move |e| { model.set(e.value()); saved.set(false); },
                    }
                }

                if is_onnx {
                    {field_label("Device")}
                    select {
                        id: "settings-embedder-device",
                        style: text_input_style(),
                        value: "{device}",
                        onchange: move |e| { device.set(e.value()); saved.set(false); },
                        option { value: "auto", "Auto (detect GPU, fall back to CPU)" }
                        option { value: "cpu", "CPU" }
                        option { value: "cuda", "NVIDIA GPU (CUDA)" }
                        option { value: "rocm", "AMD GPU (ROCm)" }
                    }
                }

                if is_ollama {
                    {field_label("Ollama server URL")}
                    input {
                        id: "settings-embedder-ollama-url",
                        style: text_input_style(),
                        placeholder: "{DEFAULT_OLLAMA_URL}",
                        value: "{ollama_url}",
                        oninput: move |e| { ollama_url.set(e.value()); saved.set(false); },
                    }
                }

                // reranker
                div {
                    style: "display: flex; align-items: center; gap: 0.5rem; margin-top: 1rem;",
                    input {
                        id: "settings-reranker-enabled",
                        r#type: "checkbox",
                        checked: rerank_on(),
                        onchange: move |e| { rerank_on.set(e.checked()); saved.set(false); },
                    }
                    label {
                        r#for: "settings-reranker-enabled",
                        style: "font-size: 0.9rem; color: {INK};",
                        "Rerank results with a cross-encoder"
                    }
                }
                if rerank_on() {
                    {field_label("Reranker model")}
                    input {
                        id: "settings-reranker-model",
                        style: text_input_style(),
                        placeholder: "e.g. BAAI/bge-reranker-base",
                        value: "{rerank_model}",
                        oninput: move |e| { rerank_model.set(e.value()); saved.set(false); },
                    }
                }

                div {
                    style: "font-size: 0.8rem; color: {FAINT}; line-height: 1.55; margin-top: 0.9rem; \
                            border-top: 1px solid {EDGE}; padding-top: 0.7rem;",
                    "Saved now; the engine switches to your choice in the next update."
                }
            }

            // save
            div {
                style: "display: flex; align-items: center; gap: 0.8rem; margin-top: 0.4rem;",
                button {
                    id: "settings-save",
                    style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                            padding: 0.6rem 1.5rem; font-weight: 700; font-size: 0.92rem; cursor: pointer;",
                    onclick: move |_| save(),
                    "Save settings"
                }
                if saved() {
                    span {
                        id: "settings-saved",
                        style: "color: {GOLD}; font-size: 0.85rem; font-weight: 600;",
                        "Saved."
                    }
                }
            }
        }
    }
}

fn settings_card_style() -> String {
    format!(
        "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
         padding: 1.1rem 1.15rem; margin: 1rem 0;"
    )
}

fn text_input_style() -> String {
    format!(
        "width: 100%; box-sizing: border-box; background: {BG}; color: {INK}; \
         border: 1px solid {EDGE}; border-radius: 8px; padding: 0.55rem 0.7rem; \
         font: inherit; font-size: 0.9rem; outline: none; margin-top: 0.35rem;"
    )
}

fn field_label(text: &str) -> Element {
    rsx! {
        div {
            style: "font-size: 0.74rem; font-weight: 700; letter-spacing: 0.06em; \
                    text-transform: uppercase; color: {FAINT}; margin-top: 0.9rem;",
            "{text}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render_markdown;

    /// The body is untrusted (user + AI authored): a hostile HTML payload must
    /// be stripped, while GFM features (task lists) still render.
    #[test]
    fn markdown_renders_gfm_and_sanitizes_hostile_html() {
        let src = "# Notes\n\n\
                   - [x] shipped the shell\n\
                   - [ ] wire the embedder\n\n\
                   <script>alert('xss')</script>\n\n\
                   <img src=\"https://evil.example/track.gif\">\n\n\
                   <a href=\"javascript:alert(1)\">click</a>\n\n\
                   [visit](https://example.com)";
        let html = render_markdown(src);

        // The script tag and its payload are gone.
        assert!(!html.contains("<script"), "script tag survived: {html}");
        assert!(
            !html.contains("alert('xss')"),
            "script body survived: {html}"
        );
        // No remote image fetch.
        assert!(!html.contains("<img"), "img survived: {html}");
        assert!(
            !html.contains("evil.example"),
            "remote src survived: {html}"
        );
        // The javascript: link scheme is neutralized (attribute dropped).
        assert!(!html.contains("javascript:"), "js: scheme survived: {html}");
        // GFM task list rendered to checkbox inputs.
        assert!(
            html.contains("type=\"checkbox\"") || html.contains("type=checkbox"),
            "task list not rendered: {html}"
        );
        // A safe link survives and is neutralized to open externally.
        assert!(
            html.contains("https://example.com"),
            "safe link dropped: {html}"
        );
        assert!(
            html.contains("noopener"),
            "safe link not made inert: {html}"
        );
        // A heading rendered.
        assert!(html.contains("<h1"), "heading not rendered: {html}");
    }

    /// hive's emergence conventions must SURVIVE markdown rendering as literal
    /// text: bracket tokens ([task: …]) carry no `(url)`, so markdown leaves
    /// them alone (never a link), and @mentions/#tags pass straight through.
    /// If markdown mangled or hid them, the chips below the entry would stop
    /// lining up with the prose.
    #[test]
    fn markdown_leaves_hive_tokens_literal() {
        let src = "Kickoff with @pia. [task: import my old data] [topic: bees] #rewrite";
        let html = render_markdown(src);
        assert!(
            html.contains("[task: import my old data]"),
            "bracket task token was mangled: {html}"
        );
        assert!(
            html.contains("[topic: bees]"),
            "bracket topic token was mangled: {html}"
        );
        assert!(html.contains("@pia"), "mention was dropped: {html}");
        assert!(html.contains("#rewrite"), "tag was dropped: {html}");
        // No token became a link (they have no trailing (url)).
        assert!(
            !html.contains("<a "),
            "a token was turned into a link: {html}"
        );
    }
}
