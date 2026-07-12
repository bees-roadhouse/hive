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
use hive_core::store::custom_entities::EntityFilter;
use hive_core::store::events::EventCreate;
use hive_core::store::tasks::TaskFilter;
use hive_core::store::Store;
use hive_embed::{Backend, EmbedConfig, Embedder, RuntimeEmbedder};
use hive_import::{Plan, RunOutcome, Summary};
use hive_shared::{
    ActorKind, ActorMergeResult, CustomEntity, CustomEntityPatch, EntityField, EntityTypePatch,
    EventItem, EventPatch, FieldType, JournalEntryView, Link, NewCustomEntity, NewEntityField,
    NewJournalEntry, Person, PersonPatch, Priority, SearchHit, Task, TaskPatch, TaskStatus,
    PRIORITIES, TASK_STATUSES,
};
use serde_json::Value;

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

// ── embedder / retrieval config ──────────────────────────────────────────────
//
// These keys are now LIVE: boot() builds the actual engine from them (via the
// plaintext `embedder.json` sidecar the store can't hold pre-open — see
// hive_embed::EmbedConfig and the module note in embed/src/runtime.rs). They're
// written to BOTH the encrypted config table (durable, greppable) and the
// sidecar (readable at boot). The device is NO LONGER a key — it's auto-detected
// at model load and reported truthfully; there is no user device dropdown.
//
//   embedder.backend    "native" (default, on-device ONNX BGE) | "ollama".
//                       Legacy scaffolds may hold "onnx-local" (→ native) or
//                       "hash"; Backend::parse folds those. The CI/test hash
//                       path is forced by HIVE_EMBED=hash, not by this key.
//   embedder.model      ollama model tag (e.g. "nomic-embed-text"); only
//                       meaningful for ollama. The native model is the BGE
//                       default (or $HIVE_EMBED_MODEL).
//   embedder.ollama_url ollama server base URL (default http://localhost:11434);
//                       only meaningful for ollama.
//   reranker.enabled    "true" | "false" — cross-encoder rerank stage on/off.
//                       Honored by search when a reranker is actually loaded
//                       (native + reranker model); a no-op otherwise, labeled so.
//   reranker.model      free text — cross-encoder model id. Only meaningful when enabled.
const EMBEDDER_BACKEND_KEY: &str = "embedder.backend";
const EMBEDDER_MODEL_KEY: &str = "embedder.model";
const EMBEDDER_OLLAMA_URL_KEY: &str = "embedder.ollama_url";
const RERANKER_ENABLED_KEY: &str = "reranker.enabled";
const RERANKER_MODEL_KEY: &str = "reranker.model";

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Which top-level section the main pane shows. A root signal drives it; the
/// sidebar sets it. Journal is the app's home (where onboarding lands).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Journal,
    Tasks,
    Mail,
    Contacts,
    Calendar,
    Identities,
    Settings,
}

impl Section {
    /// Sidebar order + the (icon, label) each row renders.
    const ALL: [Section; 7] = [
        Section::Journal,
        Section::Tasks,
        Section::Mail,
        Section::Contacts,
        Section::Calendar,
        Section::Identities,
        Section::Settings,
    ];

    fn icon(self) -> &'static str {
        match self {
            Section::Journal => "✍",
            // Tasks: a ballot-box-with-check glyph — a single symbol in the
            // same block as its siblings (NOT a word; the Contacts-icon-was-
            // the-literal-word bug is exactly what this avoids).
            Section::Tasks => "☑",
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
            Section::Tasks => "Tasks",
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
            Section::Tasks => "nav-tasks",
            Section::Mail => "nav-mail",
            Section::Contacts => "nav-contacts",
            Section::Calendar => "nav-calendar",
            Section::Identities => "nav-identities",
            Section::Settings => "nav-settings",
        }
    }
}

/// What the detail pane is showing, when anything. A root signal in the Shell
/// holds `Option<Selected>`; while it is `Some`, the main pane renders the
/// reusable `EntityDetail` over the section pane (no relaunch, no route
/// change). Both variants carry just an id — the detail view loads the rest.
#[derive(Clone, PartialEq)]
enum Selected {
    /// A contact card: a custom entity of the `contact` type.
    Contact(String),
    /// A task row.
    Task(String),
    /// A calendar event (a built-in `event` row).
    Event(String),
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
    // The real engine, resolved from the sidecar config: native ONNX BGE by
    // default (auto CPU/GPU), or the manually-configured Ollama backend. CI/
    // tests still force the hash path via HIVE_EMBED=hash. The model download
    // (BGE via hf-hub) defers to first embed; the flatpak now has the network
    // hole for it (docs/THREAT-MODEL.md) and it caches under the data dir.
    match Store::new(
        &dir,
        Arc::new(MemoryKeySource(master)),
        build_embedder(&dir),
    ) {
        Ok(store) => Boot::Ready(store),
        Err(e) => Boot::Failed(format!("{e:#}")),
    }
}

/// Build the injected embedder from the sidecar config beside `data_dir`, after
/// pinning the ONNX model cache under the data dir (so BGE downloads land in the
/// app's own writable space, not the container-era `/data/models` default).
fn build_embedder(data_dir: &std::path::Path) -> Arc<dyn Embedder> {
    ensure_model_cache_env(data_dir);
    let cfg = EmbedConfig::load(data_dir);
    Arc::new(RuntimeEmbedder::from_config_or_env(&cfg))
}

/// Point `$HIVE_MODEL_CACHE` at `<data_dir>/models` unless the operator already
/// set it. hive-embed's default is the container path `/data/models`, wrong for
/// the desktop app; this keeps model downloads inside the app's writable dir.
fn ensure_model_cache_env(data_dir: &std::path::Path) {
    if std::env::var_os("HIVE_MODEL_CACHE").is_none() {
        std::env::set_var("HIVE_MODEL_CACHE", data_dir.join("models"));
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
        build_embedder(&dir),
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
    // `selected` opens the reusable detail view IN the main pane over the
    // current section (no relaunch, no route). Contacts/Tasks set it to a
    // card/task; the detail's Back button clears it. Kept at the Shell root so
    // it survives section switches and the section pane stays mounted behind.
    let selected = use_signal(|| Option::<Selected>::None);

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

            Sidebar { store, section, active, refresh, selected }

            // Main pane — scrolls independently of the sidebar. When a card or
            // task is selected, the reusable detail view takes over here; the
            // section pane renders otherwise.
            div {
                id: "main-pane",
                style: "flex: 1; min-width: 0; height: 100%; overflow-y: auto;",
                if let Some(sel) = selected() {
                    EntityDetail { store, selected, refresh, target: sel }
                } else {
                    match section() {
                        Section::Journal => rsx! { JournalPane { store, active } },
                        Section::Tasks => rsx! { TasksPane { store, selected } },
                        Section::Contacts => rsx! { ContactsPane { store, selected, refresh } },
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
                        Section::Calendar => rsx! { CalendarPane { store, selected, refresh } },
                    }
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
    selected: Signal<Option<Selected>>,
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
                    {sidebar_row(s, s == current, section, selected)}
                }
            }

            // active-identity card → Identities
            button {
                id: "active-identity",
                style: "display: flex; align-items: center; gap: 0.55rem; text-align: left; \
                        margin: 0.5rem; padding: 0.55rem 0.65rem; border-radius: 10px; \
                        background: {BG}; border: 1px solid {EDGE}; cursor: pointer; \
                        color: {INK}; font: inherit;",
                onclick: move |_| {
                    selected.set(None);
                    section.set(Section::Identities);
                },
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

/// One sidebar navigation row. Selecting a section also clears any open
/// detail view, so the section's own pane shows.
fn sidebar_row(
    s: Section,
    active: bool,
    mut section: Signal<Section>,
    mut selected: Signal<Option<Selected>>,
) -> Element {
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
            onclick: move |_| {
                selected.set(None);
                section.set(s);
            },
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

/// The owner *slug* — who "you" are as an identity, for ownership/merge ops.
///
/// `identity.owner` historically stored a display name (defaulting to $USER).
/// This resolves that string to a real person slug so claim/take-over target a
/// concrete identity: an exact slug match wins; else a person whose slug equals
/// the slugified string (name → slug); else the slugified string itself (so a
/// brand-new store with only "$USER" still yields a usable, stable slug key).
async fn owner_slug(store: &Store) -> String {
    let raw = owner_name(store).await;
    if let Ok(Some(p)) = store.people_by_slug(&raw).await {
        return p.slug;
    }
    let slug = hive_shared::slugify(&raw);
    if let Ok(Some(p)) = store.people_by_slug(&slug).await {
        return p.slug;
    }
    slug
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
                    // Embed-on-write: make the new entry searchable without a
                    // manual re-embed. Fire-and-forget and OFF the composer's
                    // path — the backfill only touches missing/stale items (so
                    // it just embeds this one) and embeds off the writer thread.
                    // A latched model no-ops cleanly; failure is non-fatal to
                    // writing, so we don't surface it here.
                    spawn(async move {
                        let _ = store.backfill_embeddings().await;
                    });
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

// ── calendar pane (fold-safe: a month grid over the existing events) ──────────
//
// The calendar renders the SAME built-in events the journal emits (title/body/
// at/tags/assignees) — it invents no new type. Each event is placed by parsing
// its free-form `at` with `event_day`; anything undated or unparseable falls
// into the "Undated / unscheduled" list so nothing is hidden. Create/edit/
// delete go through the reusable EntityDetail (Selected::Event). Recurrence,
// reminders, and CalDAV are a deferred, batched fold-migration slice.

/// The Calendar section: a navigable month grid of events, an undated list, and
/// a minimal create form. Selecting an event (or creating one) opens the shared
/// EntityDetail. `refresh` re-pulls the events after a create/edit/delete.
#[component]
fn CalendarPane(
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    refresh: Signal<u32>,
) -> Element {
    // Visible month, initialized to today (falls back to a fixed epoch only if
    // the clock string is somehow unparseable — it never is).
    let (ty0, tm0, _td0) = parse_ymd(&today_ymd()).unwrap_or((2026, 7, 11));
    let mut year = use_signal(|| ty0);
    let mut month = use_signal(|| tm0);
    // Create-form state; a day-cell click prefills the date and the form sits
    // at the top of the pane.
    let mut new_title = use_signal(String::new);
    let mut new_date = use_signal(today_ymd);
    let mut err = use_signal(|| Option::<String>::None);

    let events = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            store.events_list().await.map_err(|e| format!("{e:#}"))
        }
    });

    // Create a minimal event on the chosen date, then open its detail.
    let mut create = move || {
        let title = new_title().trim().to_string();
        if title.is_empty() {
            err.set(Some("Give the event a title first.".to_string()));
            return;
        }
        let date = new_date().trim().to_string();
        let at = if date.is_empty() { None } else { Some(date) };
        let store = store();
        let mut refresh = refresh;
        spawn(async move {
            match store
                .events_create(
                    EventCreate {
                        title,
                        at,
                        ..Default::default()
                    },
                    "system",
                )
                .await
            {
                Ok(e) => {
                    new_title.set(String::new());
                    err.set(None);
                    refresh += 1;
                    selected.set(Some(Selected::Event(e.id)));
                }
                Err(e) => err.set(Some(format!("{e:#}"))),
            }
        });
    };

    let y = year();
    let m = month();
    let today = today_ymd();

    rsx! {
        div {
            id: "calendar-pane",
            style: "max-width: 960px; margin: 0 auto; padding: 1.6rem 1.2rem 3rem;",
            {pane_header("Calendar", "Your events on a month grid — the same happenings that \
                                     emerge from your journal, plus any you add here. Click a day \
                                     to plan something; click an event to open it.")}

            // create form (a day-cell click prefills the date)
            div {
                style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                        padding: 1rem 1.1rem; margin: 1rem 0 1.3rem;",
                div { style: "font-weight: 700; font-size: 1rem;", "New event" }
                div {
                    style: "display: flex; gap: 0.6rem; margin-top: 0.8rem; flex-wrap: wrap;",
                    input {
                        id: "cal-new-title",
                        style: "flex: 1; min-width: 12rem; box-sizing: border-box; background: {BG}; color: {INK}; \
                                border: 1px solid {EDGE}; border-radius: 8px; padding: 0.6rem 0.75rem; \
                                font: inherit; font-size: 0.92rem; outline: none;",
                        placeholder: "What's happening? e.g. Dentist",
                        value: "{new_title}",
                        oninput: move |e| new_title.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                create();
                            }
                        },
                    }
                    input {
                        id: "cal-new-date",
                        r#type: "date",
                        style: "box-sizing: border-box; background: {BG}; color: {INK}; \
                                border: 1px solid {EDGE}; border-radius: 8px; padding: 0.6rem 0.75rem; \
                                font: inherit; font-size: 0.92rem; outline: none;",
                        value: "{new_date}",
                        oninput: move |e| new_date.set(e.value()),
                    }
                    button {
                        id: "cal-new",
                        style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                padding: 0.6rem 1.3rem; font-weight: 700; font-size: 0.9rem; cursor: pointer;",
                        onclick: move |_| create(),
                        "+ New event"
                    }
                }
                if let Some(e) = err() {
                    div {
                        style: "color: #e07a5f; font-size: 0.85rem; margin-top: 0.6rem;",
                        "{e}"
                    }
                }
            }

            // month nav
            div {
                style: "display: flex; align-items: center; gap: 0.6rem; margin-bottom: 0.8rem;",
                button {
                    id: "cal-prev",
                    style: "{cal_nav_btn_style()}",
                    onclick: move |_| {
                        let (ny, nm) = step_month(year(), month(), false);
                        year.set(ny);
                        month.set(nm);
                    },
                    "‹"
                }
                button {
                    id: "cal-today",
                    style: "{cal_nav_btn_style()} padding-left: 0.9rem; padding-right: 0.9rem;",
                    onclick: move |_| {
                        if let Some((ty, tm, _)) = parse_ymd(&today_ymd()) {
                            year.set(ty);
                            month.set(tm);
                        }
                    },
                    "Today"
                }
                button {
                    id: "cal-next",
                    style: "{cal_nav_btn_style()}",
                    onclick: move |_| {
                        let (ny, nm) = step_month(year(), month(), true);
                        year.set(ny);
                        month.set(nm);
                    },
                    "›"
                }
                div {
                    id: "cal-label",
                    style: "font-size: 1.15rem; font-weight: 700; margin-left: 0.4rem;",
                    "{month_label(y, m)}"
                }
            }

            match events() {
                None => muted("loading events…"),
                Some(Err(e)) => muted(&format!("events unavailable: {e}")),
                Some(Ok(list)) => {
                    let placed = placed_events(&list);
                    let undated = undated_events(&list);
                    rsx! {
                        {month_grid_view(y, m, &today, &placed, selected, new_date)}
                        {undated_view(&undated, selected)}
                    }
                }
            }

            // honest deferral — one quiet line, no non-functional controls.
            div {
                id: "cal-deferred",
                style: "color: {FAINT}; font-size: 0.82rem; line-height: 1.6; margin-top: 1.6rem; \
                        text-align: center;",
                "Recurring events, reminders, and calendar-server (CalDAV) sync arrive in a later update."
            }
        }
    }
}

/// Group events by the (year, month, day) their `at` parses to. Pure, so the
/// placement is testable without a store or a component.
fn placed_events(list: &[EventItem]) -> std::collections::HashMap<(i32, u32, u32), Vec<EventItem>> {
    let mut map: std::collections::HashMap<(i32, u32, u32), Vec<EventItem>> =
        std::collections::HashMap::new();
    for e in list {
        if let Some(day) = e.at.as_deref().and_then(event_day) {
            map.entry(day).or_default().push(e.clone());
        }
    }
    // Stable in-cell order: timed first (by time), then untimed, then title.
    // `None` (untimed) must sort AFTER any time, so key it to a sentinel that
    // orders last rather than relying on Option's None-sorts-first ordering.
    for evs in map.values_mut() {
        let key = |e: &EventItem| {
            e.at.as_deref()
                .and_then(event_time)
                .unwrap_or_else(|| "99:99".to_string())
        };
        evs.sort_by(|a, b| key(a).cmp(&key(b)).then(a.title.cmp(&b.title)));
    }
    map
}

/// The events with no parseable date — null or vague `at` — surfaced so nothing
/// is hidden. Pure and testable.
fn undated_events(list: &[EventItem]) -> Vec<EventItem> {
    list.iter()
        .filter(|e| e.at.as_deref().and_then(event_day).is_none())
        .cloned()
        .collect()
}

/// The weekday header + the 6×7 day grid for the visible month. Plain fn:
/// EventItem/HashMap lack the PartialEq component props want.
fn month_grid_view(
    year: i32,
    month: u32,
    today: &str,
    placed: &std::collections::HashMap<(i32, u32, u32), Vec<EventItem>>,
    selected: Signal<Option<Selected>>,
    new_date: Signal<String>,
) -> Element {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let cells = month_grid(year, month);
    let today_day = parse_ymd(today).filter(|(ty, tm, _)| *ty == year && *tm == month);
    rsx! {
        // weekday header
        div {
            style: "display: grid; grid-template-columns: repeat(7, 1fr); gap: 4px; \
                    margin-bottom: 4px;",
            for wd in WEEKDAYS.iter() {
                div {
                    style: "font-size: 0.72rem; font-weight: 700; letter-spacing: 0.05em; \
                            text-transform: uppercase; color: {DIM}; text-align: center; \
                            padding: 0.3rem 0;",
                    "{wd}"
                }
            }
        }
        // day cells
        div {
            id: "cal-grid",
            style: "display: grid; grid-template-columns: repeat(7, 1fr); gap: 4px;",
            for (i, cell) in cells.iter().enumerate() {
                {day_cell(*cell, i, year, month, today_day, placed, selected, new_date)}
            }
        }
    }
}

/// One grid cell: an out-of-month blank, or a day with its (up to 3) event
/// chips + "+N more". Clicking empty cell space prefills the create date.
#[allow(clippy::too_many_arguments)]
fn day_cell(
    cell: Option<u32>,
    idx: usize,
    year: i32,
    month: u32,
    today_day: Option<(i32, u32, u32)>,
    placed: &std::collections::HashMap<(i32, u32, u32), Vec<EventItem>>,
    selected: Signal<Option<Selected>>,
    mut new_date: Signal<String>,
) -> Element {
    let Some(day) = cell else {
        // Padding cell before/after the month — inert, keyed by position.
        return rsx! {
            div {
                key: "pad-{idx}",
                style: "min-height: 6.2rem; background: transparent; border-radius: 8px;",
            }
        };
    };
    let key = ymd_key(year, month, day);
    let is_today = today_day == Some((year, month, day));
    let evs = placed.get(&(year, month, day)).cloned().unwrap_or_default();
    let shown = evs.iter().take(3).cloned().collect::<Vec<_>>();
    let extra = evs.len().saturating_sub(shown.len());
    let border = if is_today {
        format!("2px solid {GOLD}")
    } else {
        format!("1px solid {EDGE}")
    };
    let num_color = if is_today { GOLD } else { DIM };
    let date_for_click = key.clone();
    rsx! {
        div {
            id: "cal-cell-{key}",
            style: "min-height: 6.2rem; background: {PANEL}; border: {border}; border-radius: 8px; \
                    padding: 0.3rem 0.35rem; display: flex; flex-direction: column; gap: 2px; \
                    cursor: pointer; overflow: hidden;",
            // A click on empty cell space prefills the create form's date. Event
            // chips stopPropagation so opening one doesn't also re-arm the date.
            onclick: move |_| new_date.set(date_for_click.clone()),
            div {
                style: "font-size: 0.72rem; font-weight: 700; color: {num_color}; text-align: right; \
                        padding: 0 0.15rem;",
                "{day}"
            }
            for e in shown.iter() {
                {event_chip(e, selected)}
            }
            if extra > 0 {
                div {
                    style: "font-size: 0.68rem; color: {DIM}; padding: 0 0.15rem;",
                    "+{extra} more"
                }
            }
        }
    }
}

/// One event chip inside a day cell: the title (prefixed with its time when the
/// `at` carried one), truncated, opening the event's detail on click.
fn event_chip(e: &EventItem, mut selected: Signal<Option<Selected>>) -> Element {
    let id = e.id.clone();
    let time = e.at.as_deref().and_then(event_time);
    let title = if e.title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        e.title.clone()
    };
    let label = match &time {
        Some(t) => format!("{t} {title}"),
        None => title,
    };
    rsx! {
        button {
            id: "cal-event-{e.id}",
            style: "display: block; width: 100%; text-align: left; background: {BG}; \
                    border: 1px solid {EDGE}; border-radius: 6px; color: {INK}; font: inherit; \
                    font-size: 0.72rem; padding: 0.15rem 0.35rem; cursor: pointer; \
                    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;",
            onclick: move |ev| {
                ev.stop_propagation();
                selected.set(Some(Selected::Event(id.clone())));
            },
            "{label}"
        }
    }
}

/// The "Undated / unscheduled" list below the grid — events whose `at` is null
/// or unparseable, so journal-emerged vague events are never hidden.
fn undated_view(undated: &[EventItem], selected: Signal<Option<Selected>>) -> Element {
    if undated.is_empty() {
        return rsx! {
            div { id: "cal-undated", style: "display: none;" }
        };
    }
    rsx! {
        div {
            id: "cal-undated",
            style: "margin-top: 1.4rem;",
            div {
                style: "font-size: 0.78rem; font-weight: 700; letter-spacing: 0.05em; \
                        text-transform: uppercase; color: {FAINT}; margin-bottom: 0.6rem;",
                "Undated / unscheduled"
            }
            for e in undated.iter() {
                {undated_row(e, selected)}
            }
        }
    }
}

/// One undated event row: title + its raw `at` text (if any), opening the
/// detail so a real date can be set.
fn undated_row(e: &EventItem, mut selected: Signal<Option<Selected>>) -> Element {
    let id = e.id.clone();
    let raw = e.at.clone().filter(|s| !s.trim().is_empty());
    let title = if e.title.trim().is_empty() {
        "(untitled event)".to_string()
    } else {
        e.title.clone()
    };
    rsx! {
        button {
            id: "cal-undated-{e.id}",
            style: "display: flex; align-items: center; gap: 0.7rem; width: 100%; text-align: left; \
                    background: {PANEL}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.6rem 0.9rem; margin-bottom: 0.5rem; color: {INK}; font: inherit; cursor: pointer;",
            onclick: move |_| selected.set(Some(Selected::Event(id.clone()))),
            div {
                style: "flex: 1; min-width: 0;",
                div { style: "font-weight: 600; font-size: 0.92rem;", "{title}" }
                if let Some(r) = raw {
                    div {
                        style: "font-size: 0.76rem; color: {FAINT}; margin-top: 0.1rem;",
                        "when: {r}"
                    }
                }
            }
        }
    }
}

/// Shared style for the small month-nav buttons.
fn cal_nav_btn_style() -> String {
    format!(
        "background: {PANEL}; color: {INK}; border: 1px solid {EDGE}; border-radius: 8px; \
         padding: 0.4rem 0.7rem; font: inherit; font-size: 0.95rem; font-weight: 700; cursor: pointer;"
    )
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
    // Who "you" are, as a slug — the target of set-owner/claim/take-over. Resolved
    // once at mount (and on refresh) from identity.owner via owner_slug(). Empty
    // until resolved; rows fall back to "no owner designated yet" copy.
    let owner = use_signal(String::new);
    // A merge/claim/set-owner in flight — disables every mutating control so a
    // second click can't race the first (the confirm especially is irreversible).
    let busy = use_signal(|| false);
    // The single open take-over preview: (from_slug, counts). None = closed. Only
    // one at a time (the spec) — opening another replaces it.
    let preview = use_signal(|| Option::<(String, ActorMergeResult)>::None);

    // Resolve the owner slug once (re-runs when refresh bumps, e.g. after a
    // set-owner). Writes into `owner` so rows can compare synchronously.
    {
        let mut owner = owner;
        let _ = use_resource(move || {
            let store = store();
            async move {
                let _ = refresh();
                owner.set(owner_slug(&store).await);
            }
        });
    }

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

            // One-line explainer: what the owner is and how claim vs take-over differ.
            div {
                id: "identity-explainer",
                style: "color: {DIM}; font-size: 0.85rem; line-height: 1.55; margin-top: 0.7rem;",
                "Identities are who authors entries. The owner is you — "
                span { style: "color: {INK};", "claim" }
                " an AI identity to mark it yours, or "
                span { style: "color: {INK};", "take over" }
                " a human identity that is actually you to merge its whole history into yours."
            }

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
            }

            // Errors from any action (create, set-owner, claim, take-over) surface
            // here, in-pane, above the roster — never a panic.
            if let Some(e) = err() {
                div {
                    id: "identity-error",
                    style: "color: #e07a5f; font-size: 0.85rem; background: {PANEL}; \
                            border: 1px solid #e07a5f; border-radius: 8px; padding: 0.6rem 0.8rem; \
                            margin: 0 0 0.8rem;",
                    "{e}"
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
                            {identity_row(store, person, active, section, owner, busy, preview, refresh, err)}
                        }
                    }
                },
            }
        }
    }
}

/// One identity row: badge + name + slug + a Switch/Active control (persists
/// identity.active and hops to the Journal so the switch is felt), plus the
/// ownership controls — set-as-owner, claim (AI), and take-over (human, opens an
/// inline merge preview). Plain fn: Person and the signals lack the PartialEq
/// component props need.
#[allow(clippy::too_many_arguments)]
fn identity_row(
    store: ReadOnlySignal<Store>,
    person: &Person,
    mut active: Signal<String>,
    mut section: Signal<Section>,
    owner: Signal<String>,
    mut busy: Signal<bool>,
    mut preview: Signal<Option<(String, ActorMergeResult)>>,
    mut refresh: Signal<u32>,
    mut err: Signal<Option<String>>,
) -> Element {
    let is_ai = person.kind == ActorKind::Ai;
    let is_active = active() == person.slug;
    let slug = person.slug.clone();
    // A `writer:`-prefixed id is a bare author with no people row (unioned in
    // from journal_writers); claiming one must materialise a real Person first.
    let is_writer = person.id.starts_with("writer:");
    let owner_slug = owner();
    // The owner is "you". Only a resolved (non-empty) owner can match, so an
    // unresolved owner leaves every row a plain non-owner (controls still show).
    let is_owner = !owner_slug.is_empty() && owner_slug == person.slug;
    // An AI already linked to you (Person.owner == owner). Writer rows carry the
    // owner journal_writers reported, so a claimed-then-forgotten row still reads.
    let is_owned_by_me = !owner_slug.is_empty() && person.owner.as_deref() == Some(&*owner_slug);
    // The owner badge wins; else AI vs human.
    let (badge, badge_bg) = if is_owner {
        ("you · owner", GOLD)
    } else if is_ai {
        ("AI", GOLD)
    } else {
        ("human", FAINT)
    };
    // Phase 3: per-identity mail credentials hang off the identity here.

    rsx! {
        // Column wrapper: the horizontal row line, then the inline take-over
        // preview panel (when this row is the one being taken over).
        div {
            style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.7rem 0.9rem; margin-bottom: 0.6rem;",
            div {
                // A stable, per-slug id so a script can target a specific identity row.
                id: "identity-row-{person.slug}",
                style: "display: flex; align-items: center; gap: 0.7rem;",
                span {
                    style: "display: inline-flex; align-items: center; justify-content: center; \
                            width: 2.1rem; height: 2.1rem; border-radius: 50%; background: {EDGE}; \
                            color: {GOLD}; font-size: 1rem; flex-shrink: 0;",
                    "⬡"
                }
                div {
                    style: "flex: 1; min-width: 0;",
                    div {
                        style: "display: flex; align-items: baseline; gap: 0.5rem; flex-wrap: wrap;",
                        span { style: "font-weight: 700; font-size: 0.98rem;", "{person.name}" }
                        span {
                            style: "font-size: 0.62rem; font-weight: 700; letter-spacing: 0.05em; \
                                    text-transform: uppercase; color: {BG}; background: {badge_bg}; \
                                    border-radius: 5px; padding: 0.1rem 0.35rem;",
                            "{badge}"
                        }
                        // Owned-by-you marker on claimed AI rows (not on the owner itself).
                        if is_owned_by_me && !is_owner {
                            span {
                                id: "identity-owned-{person.slug}",
                                style: "font-size: 0.62rem; font-weight: 700; letter-spacing: 0.04em; \
                                        text-transform: uppercase; color: {GOLD}; border: 1px solid {GOLD}; \
                                        border-radius: 5px; padding: 0.1rem 0.35rem;",
                                "owned by you"
                            }
                        }
                    }
                    div {
                        style: "font-size: 0.78rem; color: {FAINT}; margin-top: 0.15rem;",
                        "{person.slug}"
                    }
                }
                // Ownership controls — never on the owner's own row.
                if !is_owner {
                    div {
                        style: "display: flex; align-items: center; gap: 0.45rem; flex-wrap: wrap; \
                                justify-content: flex-end;",
                        // Set-as-owner: repoint identity.owner at this slug (cheap, allowed).
                        {
                            let set_slug = person.slug.clone();
                            rsx! {
                                button {
                                    id: "identity-setowner-{person.slug}",
                                    disabled: busy(),
                                    style: "background: {BG}; color: {DIM}; border: 1px solid {EDGE}; \
                                            border-radius: 999px; padding: 0.25rem 0.7rem; font: inherit; \
                                            font-size: 0.74rem; font-weight: 600; cursor: pointer;",
                                    onclick: move |_| {
                                        if busy() { return; }
                                        let set_slug = set_slug.clone();
                                        let store = store();
                                        busy.set(true);
                                        err.set(None);
                                        spawn(async move {
                                            match store.config_set(IDENTITY_OWNER_KEY, &set_slug).await {
                                                Ok(()) => refresh += 1,
                                                Err(e) => err.set(Some(format!("{e:#}"))),
                                            }
                                            busy.set(false);
                                        });
                                    },
                                    "Set as owner"
                                }
                            }
                        }
                        // AI: claim (link to you) unless already owned by you.
                        if is_ai && !is_owned_by_me {
                            {
                                let claim_slug = person.slug.clone();
                                let claim_name = person.name.clone();
                                rsx! {
                                    button {
                                        id: "identity-claim-{person.slug}",
                                        disabled: busy() || owner_slug.is_empty(),
                                        style: "background: {GOLD}; color: #14120e; border: none; \
                                                border-radius: 999px; padding: 0.25rem 0.8rem; font: inherit; \
                                                font-size: 0.74rem; font-weight: 700; cursor: pointer;",
                                        onclick: move |_| {
                                            if busy() { return; }
                                            let owner_slug = owner();
                                            if owner_slug.is_empty() { return; }
                                            let (claim_slug, claim_name) =
                                                (claim_slug.clone(), claim_name.clone());
                                            let store = store();
                                            busy.set(true);
                                            err.set(None);
                                            spawn(async move {
                                                // A writer-only row has no Person yet — materialise
                                                // one (owner set in the insert) via people_upsert;
                                                // a real AI row just gets its owner patched.
                                                let res = if is_writer {
                                                    store
                                                        .people_upsert(
                                                            &claim_slug,
                                                            &claim_name,
                                                            ActorKind::Ai,
                                                            Some(&owner_slug),
                                                        )
                                                        .await
                                                        .map(|_| ())
                                                } else {
                                                    store
                                                        .people_update(
                                                            &claim_slug,
                                                            PersonPatch {
                                                                owner: Some(Some(owner_slug.clone())),
                                                                ..Default::default()
                                                            },
                                                            &owner_slug,
                                                        )
                                                        .await
                                                        .map(|_| ())
                                                };
                                                match res {
                                                    Ok(()) => refresh += 1,
                                                    Err(e) => err.set(Some(format!("{e:#}"))),
                                                }
                                                busy.set(false);
                                            });
                                        },
                                        "Claim as mine"
                                    }
                                }
                            }
                        }
                        // Human: take over (merge history into you) — opens the preview.
                        if !is_ai {
                            {
                                let to_slug = person.slug.clone();
                                rsx! {
                                    button {
                                        id: "identity-takeover-{person.slug}",
                                        disabled: busy() || owner_slug.is_empty(),
                                        style: "background: {BG}; color: {INK}; border: 1px solid {GOLD}; \
                                                border-radius: 999px; padding: 0.25rem 0.8rem; font: inherit; \
                                                font-size: 0.74rem; font-weight: 700; cursor: pointer;",
                                        onclick: move |_| {
                                            if busy() { return; }
                                            let owner_slug = owner();
                                            if owner_slug.is_empty() { return; }
                                            let from = to_slug.clone();
                                            let store = store();
                                            busy.set(true);
                                            err.set(None);
                                            spawn(async move {
                                                match store
                                                    .actors_merge_preview(&from, &owner_slug)
                                                    .await
                                                {
                                                    Ok(res) => preview.set(Some((from, res))),
                                                    Err(e) => err.set(Some(format!("{e:#}"))),
                                                }
                                                busy.set(false);
                                            });
                                        },
                                        "This is me — take over"
                                    }
                                }
                            }
                        }
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
            // Inline take-over preview — only for the row being taken over.
            {takeover_preview(store, person, owner, busy, preview, refresh, err)}
        }
    }
}

/// The inline merge preview panel for a take-over: shows what `actors_merge`
/// would rewrite (per the preview counts) and gates the irreversible merge
/// behind an explicit confirm. Renders only when `preview` names this row.
#[allow(clippy::too_many_arguments)]
fn takeover_preview(
    store: ReadOnlySignal<Store>,
    person: &Person,
    owner: Signal<String>,
    mut busy: Signal<bool>,
    mut preview: Signal<Option<(String, ActorMergeResult)>>,
    mut refresh: Signal<u32>,
    mut err: Signal<Option<String>>,
) -> Element {
    // Show only when this row is the one under preview.
    let Some((from, res)) = preview() else {
        return rsx! {};
    };
    if from != person.slug {
        return rsx! {};
    }
    let owner_slug = owner();
    // Counts worth surfacing: journal (authored) + mentions ride journal, plus
    // the emerged entities and links the merge rewrites onto you.
    let confirm_from = from.clone();
    rsx! {
        div {
            id: "takeover-preview",
            style: "margin-top: 0.7rem; border-top: 1px solid {EDGE}; padding-top: 0.7rem;",
            div {
                style: "color: {INK}; font-size: 0.86rem; line-height: 1.55;",
                "This merges "
                span { style: "font-weight: 700;", "{from}" }
                " into "
                span { style: "font-weight: 700; color: {GOLD};", "{owner_slug}" }
                ". It rewrites "
                span { style: "font-weight: 700;", "{res.journal}" }
                " journal entries, "
                span { style: "font-weight: 700;", "{res.tasks}" }
                " tasks, "
                span { style: "font-weight: 700;", "{res.decisions}" }
                " decisions, "
                span { style: "font-weight: 700;", "{res.events}" }
                " events, "
                span { style: "font-weight: 700;", "{res.inbox}" }
                " inbox items, "
                span { style: "font-weight: 700;", "{res.entities}" }
                " entities and "
                span { style: "font-weight: 700;", "{res.sources}" }
                " sources to "
                span { style: "font-weight: 700; color: {GOLD};", "{owner_slug}" }
                ", then removes "
                span { style: "font-weight: 700;", "{from}" }
                ". This can't be undone."
            }
            div {
                style: "display: flex; gap: 0.5rem; margin-top: 0.7rem;",
                button {
                    id: "takeover-confirm",
                    disabled: busy() || owner_slug.is_empty(),
                    style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                            padding: 0.4rem 1rem; font: inherit; font-size: 0.82rem; \
                            font-weight: 700; cursor: pointer;",
                    onclick: move |_| {
                        if busy() { return; }
                        let owner_slug = owner();
                        if owner_slug.is_empty() { return; }
                        let from = confirm_from.clone();
                        let store = store();
                        busy.set(true);
                        err.set(None);
                        spawn(async move {
                            match store.actors_merge(&from, &owner_slug).await {
                                Ok(_) => {
                                    preview.set(None);
                                    refresh += 1;
                                }
                                Err(e) => err.set(Some(format!("{e:#}"))),
                            }
                            busy.set(false);
                        });
                    },
                    "Merge into {owner_slug}"
                }
                button {
                    id: "takeover-cancel",
                    disabled: busy(),
                    style: "background: {BG}; color: {DIM}; border: 1px solid {EDGE}; \
                            border-radius: 8px; padding: 0.4rem 1rem; font: inherit; \
                            font-size: 0.82rem; font-weight: 600; cursor: pointer;",
                    onclick: move |_| {
                        if busy() { return; }
                        preview.set(None);
                    },
                    "Cancel"
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

// ── contacts + tasks + the reusable entity-detail view (Phase 3, slice 1) ─────
//
// A contact card is the canonical rich person record: a custom entity of the
// built-in `contact` type (core seeds it via ensure_contact_type). The Tasks
// pane groups the task table by status. Both open the SAME EntityDetail view
// (typed fields + "Related in your journal" backlinks), so the structure is
// built once. slice 2: identities link to a contact card via a Ref field.

/// The `contact` type slug, mirrored from hive_core::store::contacts. The
/// Contacts pane filters instances by it and the detail view saves against it.
const CONTACT_TYPE_SLUG: &str = "contact";

/// Years between a `YYYY-MM-DD` birthday and a `YYYY-MM-DD` "today", or None if
/// the birthday isn't a plain date (an ISO timestamp, empty, or malformed).
/// Pure and total: integer calendar math, no chrono, so it unit-tests without
/// a clock. The birthday hasn't-occurred-yet-this-year case subtracts one.
fn age_years(birthday: &str, today: &str) -> Option<i64> {
    let ymd = |s: &str| -> Option<(i64, i64, i64)> {
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
        Some((n(0, 4)?, n(5, 7)?, n(8, 10)?))
    };
    let (by, bm, bd) = ymd(birthday)?;
    let (ty, tm, td) = ymd(today)?;
    let mut age = ty - by;
    if (tm, td) < (bm, bd) {
        age -= 1; // birthday hasn't come round yet this year
    }
    if age < 0 {
        None // a future birthday has no age
    } else {
        Some(age)
    }
}

/// Today as `YYYY-MM-DD` (UTC), from the store's ISO clock — the age fn's
/// second argument at the call site.
fn today_ymd() -> String {
    hive_core::store::now_iso()
        .get(0..10)
        .unwrap_or_default()
        .to_string()
}

// ── a small, chrono-free calendar library (integer math, unit-tested) ─────────
//
// The month grid and event placement need only: which (y,m,d) an event falls
// on, whether a year is a leap year, how many days a month has, and the weekday
// of a date. All pure integer math (like age_years/today_ymd) so it tests
// without a clock and never pulls chrono into the UI crate.

/// Is `year` a leap year (proleptic Gregorian)?
fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in `month` (1-12) of `year`; 0 for an out-of-range month.
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Weekday of a date, 0 = Sunday … 6 = Saturday (Sakamoto's algorithm). Valid
/// for the proleptic Gregorian calendar; month must be 1-12.
fn weekday(year: i32, month: u32, day: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year;
    if month < 3 {
        y -= 1;
    }
    let m = month as i32;
    let d = day as i32;
    (((y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d) % 7) + 7) as u32 % 7
}

/// Step one month, wrapping the year: (2026, 12) → (2027, 1), (2026, 1) prev →
/// (2025, 12). `forward` picks the direction.
fn step_month(year: i32, month: u32, forward: bool) -> (i32, u32) {
    if forward {
        if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        }
    } else if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// The 42 cells (6 weeks × 7 days, Sunday-first) of the month view: `None`
/// before the 1st and after the last day, `Some(day)` for the month's days.
/// Six rows is the fixed maximum any Gregorian month needs, so the grid never
/// reflows height between months.
fn month_grid(year: i32, month: u32) -> Vec<Option<u32>> {
    let lead = weekday(year, month, 1) as usize; // blanks before the 1st
    let days = days_in_month(year, month) as usize;
    let mut cells: Vec<Option<u32>> = Vec::with_capacity(42);
    for _ in 0..lead {
        cells.push(None);
    }
    for d in 1..=days {
        cells.push(Some(d as u32));
    }
    while cells.len() < 42 {
        cells.push(None);
    }
    cells
}

/// Parse a free-form event `at` string leniently into (year, month, day),
/// accepting the shapes journal emergence and the editor actually write:
///
///   - the frozen 24-char ISO `YYYY-MM-DDTHH:MM:SS.sssZ`
///   - an ISO date `YYYY-MM-DD`
///   - an ISO datetime `YYYY-MM-DDTHH:MM[:SS]` (any timezone suffix ignored)
///   - `YYYY-MM-DD HH:MM[:SS]` (space separator)
///
/// Anything else (vague prose like "next Tuesday", empty, malformed) → None, so
/// it lands in the "Undated / unscheduled" list rather than a wrong cell. The
/// date is validated (real month, real day-of-month) so 2026-02-30 is rejected.
fn event_day(at: &str) -> Option<(i32, u32, u32)> {
    let s = at.trim();
    let b = s.as_bytes();
    // Require at least `YYYY-MM-DD` with the two dashes in place; the char after
    // the date (if any) must be a separator we recognize (T, t, or space).
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if b.len() > 10 && !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i32>().ok();
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    // Reject stray signs/plus that parse::<i32> would tolerate on the sub-slice.
    if !s.as_bytes()[0..10]
        .iter()
        .all(|c| c.is_ascii_digit() || *c == b'-')
    {
        return None;
    }
    if !(1..=12).contains(&month) {
        return None;
    }
    let m = month as u32;
    let d = day as u32;
    if day < 1 || d > days_in_month(year, m) {
        return None;
    }
    Some((year, m, d))
}

/// The `HH:MM` of an event `at` if it carried a time (datetime or dotted ISO),
/// else None — used to show a time beside a day-cell chip. Requires a valid
/// date prefix first (so a bare time-less date shows no time).
fn event_time(at: &str) -> Option<String> {
    event_day(at)?; // only meaningful for a real date
    let s = at.trim();
    let b = s.as_bytes();
    if b.len() < 16 || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    // HH:MM at positions 11..16, both numeric, with the colon in place.
    let hh = s.get(11..13)?;
    let mm = s.get(14..16)?;
    if b[13] != b':'
        || !hh.bytes().all(|c| c.is_ascii_digit())
        || !mm.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let (h, mi) = (hh.parse::<u32>().ok()?, mm.parse::<u32>().ok()?);
    if h > 23 || mi > 59 {
        return None;
    }
    Some(format!("{hh}:{mm}"))
}

/// Format a (year, month, day) as the `YYYY-MM-DD` key used for cell ids and
/// as the clean value written into `at` from the date picker.
fn ymd_key(year: i32, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// Parse `today_ymd()`-shaped `YYYY-MM-DD` back into (year, month, day) for the
/// initial calendar position and the today highlight.
fn parse_ymd(s: &str) -> Option<(i32, u32, u32)> {
    event_day(s)
}

/// The month label a header shows, e.g. "July 2026".
fn month_label(year: i32, month: u32) -> String {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let name = NAMES
        .get((month.saturating_sub(1)) as usize)
        .copied()
        .unwrap_or("");
    format!("{name} {year}")
}

/// A JSON field value as the string an `<input>`/`<textarea>` shows: strings
/// verbatim, numbers/bools stringified, everything else empty.
fn value_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// The contact card's display name (its entity title), with a fallback so a
/// blank card is still targetable.
fn contact_display(e: &CustomEntity) -> String {
    if e.title.trim().is_empty() {
        "(unnamed contact)".to_string()
    } else {
        e.title.clone()
    }
}

/// The one-line hint under a contact row: org / title / nickname if present.
fn contact_hint(e: &CustomEntity) -> Option<String> {
    for key in ["organization", "title", "nickname"] {
        let v = value_str(e.fields.get(key));
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    None
}

/// The Contacts section: the card list + a create field. Selecting a row (or
/// creating a card) opens the reusable detail view. The `contact` type is
/// seeded idempotently on mount (ensure_contact_type), so the very first use
/// works with no setup.
#[component]
fn ContactsPane(
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    refresh: Signal<u32>,
) -> Element {
    let mut new_name = use_signal(String::new);
    let mut err = use_signal(|| Option::<String>::None);

    // Seed the type, then list its instances. `refresh` re-pulls after a
    // create (here or via journal [contact:] emergence). ensure_contact_type
    // is idempotent, so running it every load is cheap and self-healing.
    let contacts = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            store
                .ensure_contact_type()
                .await
                .map_err(|e| format!("{e:#}"))?;
            store
                .custom_entities_list(&EntityFilter {
                    type_slug: CONTACT_TYPE_SLUG.to_string(),
                    limit: 500,
                    offset: 0,
                    sort: Some("title".to_string()),
                    desc: false,
                    fields: Vec::new(),
                })
                .await
                .map_err(entity_err)
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
            // Ensure the type first (a fresh store may not have it yet), then
            // create the card and open it.
            if let Err(e) = store.ensure_contact_type().await {
                err.set(Some(format!("{e:#}")));
                return;
            }
            match store
                .custom_entities_create(
                    NewCustomEntity {
                        type_slug: CONTACT_TYPE_SLUG.to_string(),
                        title: name,
                        fields: serde_json::Map::new(),
                        scope: None,
                    },
                    "system",
                    None,
                )
                .await
            {
                Ok(e) => {
                    new_name.set(String::new());
                    err.set(None);
                    refresh += 1;
                    selected.set(Some(Selected::Contact(e.id)));
                }
                Err(e) => err.set(Some(entity_err(e))),
            }
        });
    };

    rsx! {
        div {
            id: "contacts-pane",
            style: "max-width: 760px; margin: 0 auto; padding: 1.6rem 1.2rem 3rem;",
            {pane_header("Contacts", "The people you know. Each contact is a card — standard \
                                     details plus any fields you add — and the journal entries \
                                     that mention them gather on the card automatically.")}

            // create a contact
            div {
                style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                        padding: 1rem 1.1rem; margin: 1rem 0 1.3rem;",
                div { style: "font-weight: 700; font-size: 1rem;", "New contact" }
                div {
                    style: "display: flex; gap: 0.6rem; margin-top: 0.8rem;",
                    input {
                        id: "contact-new-name",
                        style: "flex: 1; box-sizing: border-box; background: {BG}; color: {INK}; \
                                border: 1px solid {EDGE}; border-radius: 8px; padding: 0.6rem 0.75rem; \
                                font: inherit; font-size: 0.92rem; outline: none;",
                        placeholder: "Full name, e.g. Jane Doe",
                        value: "{new_name}",
                        oninput: move |e| new_name.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                create();
                            }
                        },
                    }
                    button {
                        id: "contact-create",
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

            // the card list
            match contacts() {
                None => muted("loading contacts…"),
                Some(Err(e)) => muted(&format!("contacts unavailable: {e}")),
                Some(Ok(list)) if list.is_empty() => muted(
                    "No contacts yet. Add the first one above — or write [contact: a name] in \
                     a journal entry and a card appears here, already holding that entry.",
                ),
                Some(Ok(list)) => rsx! {
                    div {
                        id: "contact-list",
                        for c in list.iter() {
                            {contact_row(c, selected)}
                        }
                    }
                },
            }
        }
    }
}

/// One contact row: name + optional hint, opening the detail view. Plain fn:
/// CustomEntity lacks the PartialEq component props need.
fn contact_row(c: &CustomEntity, mut selected: Signal<Option<Selected>>) -> Element {
    let id = c.id.clone();
    let hint = contact_hint(c);
    rsx! {
        button {
            id: "contact-row-{c.id}",
            style: "display: flex; align-items: center; gap: 0.7rem; width: 100%; text-align: left; \
                    background: {PANEL}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.7rem 0.9rem; margin-bottom: 0.6rem; color: {INK}; font: inherit; \
                    cursor: pointer;",
            onclick: move |_| selected.set(Some(Selected::Contact(id.clone()))),
            span {
                style: "display: inline-flex; align-items: center; justify-content: center; \
                        width: 2.1rem; height: 2.1rem; border-radius: 50%; background: {EDGE}; \
                        color: {GOLD}; font-size: 1rem; flex-shrink: 0;",
                "☺"
            }
            div {
                style: "flex: 1; min-width: 0;",
                div { style: "font-weight: 700; font-size: 0.98rem;", "{contact_display(c)}" }
                if let Some(h) = hint {
                    div {
                        style: "font-size: 0.78rem; color: {FAINT}; margin-top: 0.15rem; \
                                overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                        "{h}"
                    }
                }
            }
        }
    }
}

/// A friendly one-liner for an entity write error (the detail/create paths
/// surface it verbatim).
fn entity_err(e: hive_core::store::custom_entities::EntityWriteError) -> String {
    use hive_core::store::custom_entities::EntityWriteError as E;
    match e {
        E::Issues(issues) => issues
            .iter()
            .map(|i| i.message.clone())
            .collect::<Vec<_>>()
            .join("; "),
        E::UnknownType => "that type no longer exists".to_string(),
        E::ArchivedType => "that type is archived".to_string(),
        E::Other(err) => format!("{err:#}"),
    }
}

// ── tasks pane ────────────────────────────────────────────────────────────────

/// The four status columns, in board order.
const STATUS_COLUMNS: [TaskStatus; 4] = [
    TaskStatus::Todo,
    TaskStatus::Doing,
    TaskStatus::Blocked,
    TaskStatus::Done,
];

/// Group tasks into the four status buckets, preserving each bucket's incoming
/// order (tasks_list already sorts priority-then-recency). Pure, so the
/// grouping is unit-tested without a store.
fn group_by_status(tasks: Vec<Task>) -> Vec<(TaskStatus, Vec<Task>)> {
    let mut out: Vec<(TaskStatus, Vec<Task>)> =
        STATUS_COLUMNS.iter().map(|s| (*s, Vec::new())).collect();
    for t in tasks {
        if let Some(slot) = out.iter_mut().find(|(s, _)| *s == t.status) {
            slot.1.push(t);
        }
    }
    out
}

/// The Tasks section: the task table grouped into status columns. Each row
/// opens the reusable detail view; an inline status control changes status in
/// place. Tasks emerge from journal [task:] tokens and anchors (there is no
/// manual create here — that stays the journal's job).
#[component]
fn TasksPane(store: ReadOnlySignal<Store>, selected: Signal<Option<Selected>>) -> Element {
    // Re-list whenever a detail edit bumps the shared tick (status changes from
    // a row, or a save in the detail view). `selected` going back to None after
    // an edit also re-runs this via the tick.
    let tick = use_signal(|| 0u32);
    let tasks = use_resource(move || {
        let store = store();
        async move {
            let _ = tick();
            store
                .tasks_list(TaskFilter::default())
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    rsx! {
        div {
            id: "tasks-pane",
            style: "max-width: 900px; margin: 0 auto; padding: 1.6rem 1.2rem 3rem;",
            {pane_header("Tasks", "Everything that emerged as a task from your journal — write \
                                  [task: …] or anchor a line, and it lands here, grouped by \
                                  where it stands.")}
            div { style: "height: 1rem;" }
            match tasks() {
                None => muted("loading tasks…"),
                Some(Err(e)) => muted(&format!("tasks unavailable: {e}")),
                Some(Ok(list)) if list.is_empty() => muted(
                    "No tasks yet. In the journal, wrap an intention in [task: …] or anchor a \
                     sentence, and it shows up here to track.",
                ),
                Some(Ok(list)) => rsx! {
                    div {
                        style: "display: flex; gap: 0.8rem; align-items: flex-start; flex-wrap: wrap;",
                        for (status, items) in group_by_status(list) {
                            {task_column(status, items, store, selected, tick)}
                        }
                    }
                },
            }
        }
    }
}

/// One status column with its task rows. Plain fn: Task lacks PartialEq.
fn task_column(
    status: TaskStatus,
    items: Vec<Task>,
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    tick: Signal<u32>,
) -> Element {
    rsx! {
        div {
            style: "flex: 1; min-width: 190px; background: {PANEL}; border: 1px solid {EDGE}; \
                    border-radius: 12px; padding: 0.7rem 0.7rem 0.9rem;",
            div {
                style: "display: flex; align-items: center; gap: 0.4rem; margin-bottom: 0.6rem; \
                        font-size: 0.72rem; font-weight: 700; letter-spacing: 0.07em; \
                        text-transform: uppercase; color: {DIM};",
                span { style: "color: {GOLD};", "{status_label(status)}" }
                span { style: "color: {FAINT};", "{items.len()}" }
            }
            if items.is_empty() {
                div {
                    style: "color: {FAINT}; font-size: 0.8rem; padding: 0.4rem 0.2rem;",
                    "—"
                }
            }
            for t in items.iter() {
                {task_row(t, store, selected, tick)}
            }
        }
    }
}

fn status_label(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Todo => "To do",
        TaskStatus::Doing => "Doing",
        TaskStatus::Blocked => "Blocked",
        TaskStatus::Done => "Done",
    }
}

/// One task card in a column: title, optional due/assignee, an inline status
/// select, and a click-through to the detail view. Plain fn (Task: no
/// PartialEq). The status select lives on the row so a quick status flip needs
/// no drill-in; it bumps `tick` so the board re-groups.
fn task_row(
    t: &Task,
    store: ReadOnlySignal<Store>,
    mut selected: Signal<Option<Selected>>,
    mut tick: Signal<u32>,
) -> Element {
    let id = t.id.clone();
    let id_for_status = t.id.clone();
    let due = t.due.clone().filter(|d| !d.trim().is_empty());
    let assignee = t.assignees.first().cloned();
    let current = t.status;
    rsx! {
        div {
            id: "task-row-{t.id}",
            style: "background: {BG}; border: 1px solid {EDGE}; border-radius: 9px; \
                    padding: 0.55rem 0.6rem; margin-bottom: 0.5rem;",
            button {
                style: "display: block; width: 100%; text-align: left; background: none; \
                        border: none; color: {INK}; font: inherit; font-size: 0.9rem; \
                        font-weight: 600; cursor: pointer; padding: 0;",
                onclick: move |_| selected.set(Some(Selected::Task(id.clone()))),
                "{t.title}"
            }
            if due.is_some() || assignee.is_some() {
                div {
                    style: "display: flex; flex-wrap: wrap; gap: 0.5rem; margin-top: 0.3rem; \
                            font-size: 0.72rem; color: {FAINT};",
                    if let Some(d) = due {
                        span { "due {d}" }
                    }
                    if let Some(a) = assignee {
                        span { "· {a}" }
                    }
                }
            }
            select {
                id: "task-status-{t.id}",
                style: "margin-top: 0.45rem; width: 100%; box-sizing: border-box; background: {PANEL}; \
                        color: {INK}; border: 1px solid {EDGE}; border-radius: 7px; \
                        padding: 0.3rem 0.4rem; font: inherit; font-size: 0.78rem; cursor: pointer;",
                value: "{current.as_str()}",
                onchange: move |e| {
                    let want = e.value();
                    let id = id_for_status.clone();
                    let store = store();
                    spawn(async move {
                        if let Some(status) = TaskStatus::parse(&want) {
                            let patch = TaskPatch {
                                status: Some(status),
                                ..Default::default()
                            };
                            let _ = store.tasks_update(&id, patch, "system").await;
                            tick += 1;
                        }
                    });
                },
                for s in TASK_STATUSES.iter() {
                    option { value: "{s.as_str()}", "{status_label(*s)}" }
                }
            }
        }
    }
}

// ── the reusable entity-detail view (contacts + tasks share it) ──────────────

/// What the detail view loaded: the display name, the ordered field specs, the
/// current field values, and the id to resolve journal backlinks against. A
/// contact carries its type slug (for saves + add-field); a task carries None
/// (its fields are synthesized and saved via tasks_update). Built once per
/// target load, then the editors stage into a working copy.
#[derive(Clone)]
struct DetailData {
    name: String,
    /// The chip beside the name: "contact" or "task".
    kind_label: String,
    specs: Vec<EntityField>,
    values: serde_json::Map<String, Value>,
}

/// Synthesize the editable field specs for a task, so the SAME field editors
/// (and the same detail scaffold) render a task as they do a contact. Status
/// and priority are choices; due is a date; title/body are text; assignees is
/// a comma-list text field. Slugs match the TaskPatch mapping in `save`.
fn task_field_specs() -> Vec<EntityField> {
    let f = |slug: &str, label: &str, ft: FieldType, options: Vec<String>| EntityField {
        id: format!("taskfield_{slug}"),
        slug: slug.to_string(),
        label: label.to_string(),
        field_type: ft,
        required: false,
        position: 0,
        options,
        ref_kind: None,
        archived: false,
    };
    vec![
        f("title", "Title", FieldType::Text, Vec::new()),
        f(
            "status",
            "Status",
            FieldType::Choice,
            TASK_STATUSES
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
        ),
        f(
            "priority",
            "Priority",
            FieldType::Choice,
            PRIORITIES.iter().map(|p| p.as_str().to_string()).collect(),
        ),
        f("due", "Due", FieldType::Date, Vec::new()),
        f("assignees", "Assignees", FieldType::Text, Vec::new()),
        f("body", "Notes", FieldType::Text, Vec::new()),
    ]
}

/// The current task field values as the synthesized-spec map `save` reads back.
fn task_field_values(t: &Task) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("title".into(), Value::String(t.title.clone()));
    m.insert(
        "status".into(),
        Value::String(t.status.as_str().to_string()),
    );
    m.insert(
        "priority".into(),
        Value::String(t.priority.as_str().to_string()),
    );
    if let Some(d) = &t.due {
        m.insert("due".into(), Value::String(d.clone()));
    }
    m.insert("assignees".into(), Value::String(t.assignees.join(", ")));
    m.insert("body".into(), Value::String(t.body.clone()));
    m
}

/// Synthesize the editable field specs for an event, so the SAME field editors
/// render an event as they do a task/contact. `date` + `time` compose into a
/// clean ISO `at`; `at_raw` only appears when the stored `at` couldn't be
/// parsed (vague journal text) so its original value is never silently lost.
/// Slugs match the EventPatch mapping in `save_detail`.
fn event_field_specs(has_raw_at: bool) -> Vec<EntityField> {
    let f = |slug: &str, label: &str, ft: FieldType| EntityField {
        id: format!("eventfield_{slug}"),
        slug: slug.to_string(),
        label: label.to_string(),
        field_type: ft,
        required: false,
        position: 0,
        options: Vec::new(),
        ref_kind: None,
        archived: false,
    };
    let mut specs = vec![
        f("title", "Title", FieldType::Text),
        f("date", "Date", FieldType::Date),
        f("time", "Time (optional, HH:MM)", FieldType::Text),
    ];
    // Only shown when the original `at` wasn't a clean date/datetime — editing
    // it (or filling Date above) replaces it; leaving it keeps the prose.
    if has_raw_at {
        specs.push(f("at_raw", "When (as written)", FieldType::Text));
    }
    specs.push(f("assignees", "Assignees", FieldType::Text));
    specs.push(f("body", "Notes", FieldType::Text));
    specs
}

/// The current event field values as the synthesized-spec map `save_detail`
/// reads back. A parseable `at` splits into `date` (+ `time` if it had one); an
/// unparseable `at` is surfaced verbatim as `at_raw`.
fn event_field_values(e: &EventItem) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("title".into(), Value::String(e.title.clone()));
    match e.at.as_deref() {
        Some(at) if !at.trim().is_empty() => match event_day(at) {
            Some((y, mo, d)) => {
                m.insert("date".into(), Value::String(ymd_key(y, mo, d)));
                if let Some(hm) = event_time(at) {
                    m.insert("time".into(), Value::String(hm));
                }
            }
            None => {
                m.insert("at_raw".into(), Value::String(at.to_string()));
            }
        },
        _ => {}
    }
    m.insert("assignees".into(), Value::String(e.assignees.join(", ")));
    m.insert("body".into(), Value::String(e.body.clone()));
    m
}

/// Compose the `at` string an event edit should write from the staged fields:
/// a `date` plus optional `time` become a clean ISO (`YYYY-MM-DD` or the frozen
/// 24-char `…T HH:MM:00.000Z`); a `date` alone writes the bare date; no date
/// but an `at_raw` keeps the original prose; nothing → None (unscheduled).
fn compose_event_at(values: &serde_json::Map<String, Value>) -> Option<String> {
    let get = |k: &str| {
        values
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let date = get("date");
    if !date.is_empty() {
        let time = get("time");
        // Accept HH:MM (or HH:MM:SS) → normalize to the frozen 24-char ISO so
        // it round-trips through event_day/event_time; a bad time is dropped
        // rather than corrupting the date.
        if !time.is_empty() {
            let tb = time.as_bytes();
            let ok = tb.len() >= 5
                && tb[2] == b':'
                && time
                    .get(0..2)
                    .is_some_and(|h| h.bytes().all(|c| c.is_ascii_digit()))
                && time
                    .get(3..5)
                    .is_some_and(|mm| mm.bytes().all(|c| c.is_ascii_digit()));
            if ok {
                let hh = &time[0..2];
                let mm = &time[3..5];
                return Some(format!("{date}T{hh}:{mm}:00.000Z"));
            }
        }
        return Some(date);
    }
    let raw = get("at_raw");
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// The reusable detail view: a header, a typed editor per field, a Notes
/// section, an add-a-field affordance (contacts only), and the emergent
/// "Related in your journal" backlinks — used by BOTH contacts and tasks.
/// Fields stage into a working-copy signal; Save persists them the right way
/// for the target (custom_entities_update for a contact, tasks_update for a
/// task). `refresh` bumps so the pane behind re-pulls after a save.
#[component]
fn EntityDetail(
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    refresh: Signal<u32>,
    target: Selected,
) -> Element {
    // Working copy of the field values (staged edits). Reloaded from the store
    // whenever the target changes or a save/reload bumps `reload`.
    let mut edits = use_signal(serde_json::Map::<String, Value>::new);
    let mut reload = use_signal(|| 0u32);
    let mut status_msg = use_signal(|| Option::<String>::None);
    // Add-a-field form state (contacts only).
    let mut new_field_label = use_signal(String::new);
    let mut new_field_type = use_signal(|| FieldType::Text.as_str().to_string());
    // Two-step delete confirm (events): first click arms it, second commits.
    let mut confirm_delete = use_signal(|| false);

    let target_load = target.clone();
    let data = use_resource(move || {
        let store = store();
        let target = target_load.clone();
        async move {
            let _ = reload();
            let loaded = load_detail(&store, &target).await?;
            // Seed the working copy from the freshly loaded values.
            edits.set(loaded.values.clone());
            Ok::<DetailData, String>(loaded)
        }
    });

    // The backlinks resolve independently (they don't change on field edits).
    let target_links = target.clone();
    let backlinks = use_resource(move || {
        let store = store();
        let target = target_links.clone();
        async move {
            let _ = reload();
            let ref_id = detail_ref_id(&target);
            related_journal_entries(&store, &ref_id).await
        }
    });

    let target_save = target.clone();
    let save = move || {
        let store = store();
        let target = target_save.clone();
        let values = edits.peek().clone();
        let mut refresh = refresh;
        spawn(async move {
            match save_detail(&store, &target, &values).await {
                Ok(()) => {
                    status_msg.set(Some("Saved.".to_string()));
                    reload += 1; // re-pull the canonical row
                    refresh += 1; // re-pull the pane behind
                }
                Err(e) => status_msg.set(Some(e)),
            }
        });
    };

    // Delete (events only for now): tombstones the row, then returns to the
    // pane behind. Two-step — the first click arms `confirm_delete`, the
    // second (the "Really delete?" button) actually deletes.
    let target_delete = target.clone();
    let delete = move || {
        let Selected::Event(id) = target_delete.clone() else {
            return;
        };
        let store = store();
        let mut selected = selected;
        let mut refresh = refresh;
        spawn(async move {
            match store.events_delete(&id, "system").await {
                Ok(_) => {
                    refresh += 1; // the calendar re-pulls without the event
                    selected.set(None); // back to the pane
                }
                Err(e) => status_msg.set(Some(format!("{e:#}"))),
            }
        });
    };

    let target_addfield = target.clone();
    let add_field = move || {
        let label = new_field_label().trim().to_string();
        if label.is_empty() {
            return;
        }
        let Selected::Contact(_) = &target_addfield else {
            return; // tasks have a fixed schema
        };
        let ft = new_field_type();
        let store = store();
        spawn(async move {
            // Editing the TYPE (shared across every contact): append an
            // EntityField to the contact type. It then renders for every card.
            let patch = EntityTypePatch {
                add_fields: vec![NewEntityField {
                    slug: None, // derived from the label
                    label,
                    field_type: ft,
                    required: false,
                    position: None,
                    options: Vec::new(),
                    ref_kind: None,
                }],
                ..Default::default()
            };
            match store
                .entity_types_update(CONTACT_TYPE_SLUG, patch, "system")
                .await
            {
                Ok(_) => {
                    new_field_label.set(String::new());
                    reload += 1; // the new field shows on this card immediately
                }
                Err(e) => {
                    let msg = match e {
                        hive_core::store::entity_types::TypeWriteError::Issues(issues) => issues
                            .iter()
                            .map(|i| i.message.clone())
                            .collect::<Vec<_>>()
                            .join("; "),
                        hive_core::store::entity_types::TypeWriteError::Other(err) => {
                            format!("{err:#}")
                        }
                    };
                    status_msg.set(Some(msg));
                }
            }
        });
    };

    let is_contact = matches!(target, Selected::Contact(_));

    rsx! {
        div {
            id: "entity-detail",
            style: "max-width: 760px; margin: 0 auto; padding: 1.4rem 1.2rem 3rem;",

            // back to the list
            button {
                id: "detail-back",
                style: "background: none; border: none; color: {GOLD}; font: inherit; \
                        font-size: 0.85rem; cursor: pointer; padding: 0; margin-bottom: 0.9rem;",
                onclick: move |_| selected.set(None),
                "← Back"
            }

            match data() {
                None => muted("loading…"),
                Some(Err(e)) => muted(&format!("couldn't open this: {e}")),
                Some(Ok(d)) => rsx! {
                    // header: display name + kind chip
                    div {
                        style: "display: flex; align-items: baseline; gap: 0.6rem; margin-bottom: 0.3rem;",
                        div { id: "detail-name", style: "font-size: 1.5rem; font-weight: 700;", "{d.name}" }
                        span {
                            style: "font-size: 0.66rem; font-weight: 700; letter-spacing: 0.06em; \
                                    text-transform: uppercase; color: {BG}; background: {GOLD}; \
                                    border-radius: 5px; padding: 0.12rem 0.4rem;",
                            "{d.kind_label}"
                        }
                    }

                    // fields
                    div {
                        style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                                padding: 1rem 1.1rem; margin: 1rem 0;",
                        for spec in d.specs.iter().filter(|f| !f.archived) {
                            {field_editor(spec, edits)}
                        }

                        // Save (+ Delete for events)
                        div {
                            style: "display: flex; align-items: center; gap: 0.7rem; margin-top: 1.1rem;",
                            button {
                                id: "detail-save",
                                style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                        padding: 0.5rem 1.2rem; font-weight: 700; font-size: 0.9rem; cursor: pointer;",
                                onclick: move |_| save(),
                                "Save"
                            }
                            if let Some(m) = status_msg() {
                                span { style: "font-size: 0.82rem; color: {DIM};", "{m}" }
                            }
                            // Delete lives at the right edge; events only.
                            if matches!(target, Selected::Event(_)) {
                                span { style: "flex: 1;" }
                                if confirm_delete() {
                                    span {
                                        style: "font-size: 0.82rem; color: #e07a5f;",
                                        "Delete this event?"
                                    }
                                    button {
                                        id: "detail-delete",
                                        style: "background: #e07a5f; color: #14120e; border: none; border-radius: 8px; \
                                                padding: 0.5rem 1rem; font-weight: 700; font-size: 0.85rem; cursor: pointer;",
                                        onclick: move |_| delete(),
                                        "Really delete"
                                    }
                                    button {
                                        id: "detail-delete-cancel",
                                        style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                                                border-radius: 8px; padding: 0.5rem 0.9rem; font: inherit; \
                                                font-size: 0.85rem; cursor: pointer;",
                                        onclick: move |_| confirm_delete.set(false),
                                        "Cancel"
                                    }
                                } else {
                                    button {
                                        id: "detail-delete-arm",
                                        style: "background: none; border: 1px solid #e07a5f; color: #e07a5f; \
                                                border-radius: 8px; padding: 0.5rem 1rem; font: inherit; font-weight: 700; \
                                                font-size: 0.85rem; cursor: pointer;",
                                        onclick: move |_| confirm_delete.set(true),
                                        "Delete"
                                    }
                                }
                            }
                        }
                    }

                    // add a custom field (contacts only — edits the shared type)
                    if is_contact {
                        div {
                            style: "background: {PANEL}; border: 1px dashed {EDGE}; border-radius: 12px; \
                                    padding: 0.9rem 1.1rem; margin-bottom: 1rem;",
                            div {
                                style: "font-size: 0.74rem; font-weight: 700; letter-spacing: 0.06em; \
                                        text-transform: uppercase; color: {FAINT};",
                                "Add a field"
                            }
                            div {
                                style: "color: {FAINT}; font-size: 0.78rem; margin-top: 0.25rem; line-height: 1.5;",
                                "Adds to the contact type — it appears on every contact card."
                            }
                            div {
                                style: "display: flex; gap: 0.5rem; margin-top: 0.7rem; flex-wrap: wrap;",
                                input {
                                    id: "field-add-label",
                                    style: "flex: 1; min-width: 9rem; box-sizing: border-box; background: {BG}; \
                                            color: {INK}; border: 1px solid {EDGE}; border-radius: 8px; \
                                            padding: 0.5rem 0.7rem; font: inherit; font-size: 0.88rem; outline: none;",
                                    placeholder: "Field name, e.g. Spouse",
                                    value: "{new_field_label}",
                                    oninput: move |e| new_field_label.set(e.value()),
                                }
                                select {
                                    id: "field-add-type",
                                    style: "background: {BG}; color: {INK}; border: 1px solid {EDGE}; \
                                            border-radius: 8px; padding: 0.5rem 0.6rem; font: inherit; \
                                            font-size: 0.88rem; cursor: pointer;",
                                    value: "{new_field_type}",
                                    onchange: move |e| new_field_type.set(e.value()),
                                    option { value: "text", "Text" }
                                    option { value: "number", "Number" }
                                    option { value: "bool", "Yes / No" }
                                    option { value: "date", "Date" }
                                }
                                button {
                                    id: "field-add-submit",
                                    style: "background: {EDGE}; color: {INK}; border: none; border-radius: 8px; \
                                            padding: 0.5rem 1rem; font: inherit; font-weight: 700; \
                                            font-size: 0.85rem; cursor: pointer;",
                                    onclick: move |_| add_field(),
                                    "Add"
                                }
                            }
                        }
                    }

                    // related journal entries (emergent backlinks)
                    {related_journal(backlinks())}
                }
            }
        }
    }
}

/// The "Related in your journal" section: the entries linked to this entity,
/// newest first, each rendered as a compact card (sanitized markdown, reusing
/// render_markdown). This is where "related journal entries emerge" — the
/// links_for_entity edges resolve to their journal entries.
fn related_journal(backlinks: Option<Result<Vec<JournalEntryView>, String>>) -> Element {
    rsx! {
        div {
            style: "margin-top: 0.4rem;",
            div {
                style: "font-size: 0.9rem; font-weight: 700; margin-bottom: 0.6rem;",
                "Related in your journal"
            }
            div {
                id: "detail-backlinks",
                match backlinks {
                    None => muted("looking for mentions…"),
                    Some(Err(e)) => muted(&format!("couldn't load mentions: {e}")),
                    Some(Ok(entries)) if entries.is_empty() => muted(
                        "Mention this in a journal entry — like [contact: a name] or [task: …] — \
                         and it shows up here.",
                    ),
                    Some(Ok(entries)) => rsx! {
                        for view in entries {
                            {entry_card(&view)}
                        }
                    },
                }
            }
        }
    }
}

/// One typed field editor keyed by FieldType, staging into the shared `edits`
/// map. Text → input (multiline textarea for `notes`/`body`), Number → number
/// input, Bool → checkbox, Date → date input, Choice → select over options,
/// Ref → an id text input (slice-2 identity refs will upgrade this to a picker
/// over the ref_kind's entities). For a `birthday` Date field, a read-only
/// calculated age renders beside it. Plain fn: EntityField lacks PartialEq.
fn field_editor(spec: &EntityField, edits: Signal<serde_json::Map<String, Value>>) -> Element {
    let slug = spec.slug.clone();
    let current = value_str(edits.read().get(&slug));
    let input_id = format!("field-{slug}");
    let multiline = matches!(spec.slug.as_str(), "notes" | "body");

    // Each arm's event closure owns its own clone of the slug (a `move` closure
    // would otherwise move the shared binding, and the borrow checker sees all
    // arms even though one runs) — this mirrors the file's per-closure-clone
    // idiom.
    let editor = match spec.field_type {
        FieldType::Text if multiline => {
            let slug = slug.clone();
            rsx! {
                textarea {
                    id: "{input_id}",
                    style: "{text_input_style()} min-height: 5rem; resize: vertical; line-height: 1.5;",
                    value: "{current}",
                    oninput: move |e| stage_str(edits, &slug, e.value()),
                }
            }
        }
        FieldType::Text => {
            let slug = slug.clone();
            rsx! {
                input {
                    id: "{input_id}",
                    style: "{text_input_style()}",
                    value: "{current}",
                    oninput: move |e| stage_str(edits, &slug, e.value()),
                }
            }
        }
        FieldType::Number => {
            let slug = slug.clone();
            rsx! {
                input {
                    id: "{input_id}",
                    r#type: "number",
                    style: "{text_input_style()}",
                    value: "{current}",
                    oninput: move |e| {
                        let v = e.value();
                        let mut edits = edits;
                        let mut m = edits.write();
                        match v.trim().parse::<f64>() {
                            Ok(n) => {
                                m.insert(slug.clone(), serde_json::json!(n));
                            }
                            Err(_) => {
                                m.remove(&slug);
                            }
                        }
                    },
                }
            }
        }
        FieldType::Bool => {
            let on = matches!(edits.read().get(&slug), Some(Value::Bool(true)));
            let slug = slug.clone();
            rsx! {
                label {
                    style: "display: inline-flex; align-items: center; gap: 0.45rem; margin-top: 0.35rem; \
                            font-size: 0.9rem; color: {INK}; cursor: pointer;",
                    input {
                        id: "{input_id}",
                        r#type: "checkbox",
                        checked: on,
                        onchange: move |e| {
                            let mut edits = edits;
                            let mut m = edits.write();
                            m.insert(slug.clone(), Value::Bool(e.checked()));
                        },
                    }
                    span { {if on { "Yes" } else { "No" }} }
                }
            }
        }
        FieldType::Date => {
            let is_birthday = spec.slug == "birthday";
            let age = age_years(&current, &today_ymd());
            let slug = slug.clone();
            rsx! {
                div {
                    style: "display: flex; align-items: center; gap: 0.7rem;",
                    input {
                        id: "{input_id}",
                        r#type: "date",
                        style: "{text_input_style()} max-width: 12rem;",
                        value: "{current}",
                        oninput: move |e| stage_str(edits, &slug, e.value()),
                    }
                    // calculated age beside a birthday (read-only, computed)
                    if is_birthday {
                        if let Some(age) = age {
                            span {
                                id: "field-birthday-age",
                                style: "font-size: 0.82rem; color: {DIM};",
                                "age {age}"
                            }
                        }
                    }
                }
            }
        }
        FieldType::Choice => {
            let options = spec.options.clone();
            let slug = slug.clone();
            rsx! {
                select {
                    id: "{input_id}",
                    style: "{text_input_style()} cursor: pointer; max-width: 16rem;",
                    value: "{current}",
                    onchange: move |e| stage_str(edits, &slug, e.value()),
                    option { value: "", "—" }
                    for opt in options.iter() {
                        option { value: "{opt}", "{opt}" }
                    }
                }
            }
        }
        FieldType::Ref => {
            let ref_kind = spec.ref_kind.clone().unwrap_or_default();
            let slug = slug.clone();
            rsx! {
                // slice 2: identities link here via a Ref field — a picker over
                // the ref_kind's entities will replace this raw id input.
                input {
                    id: "{input_id}",
                    style: "{text_input_style()}",
                    placeholder: "id of the linked {ref_kind}",
                    value: "{current}",
                    oninput: move |e| stage_str(edits, &slug, e.value()),
                }
            }
        }
    };

    rsx! {
        div {
            style: "margin-bottom: 0.2rem;",
            {field_label(&spec.label)}
            {editor}
        }
    }
}

/// Stage a string value into the working-copy map: empty clears the key
/// (so a blanked field removes it), anything else sets it as a JSON string.
fn stage_str(mut edits: Signal<serde_json::Map<String, Value>>, slug: &str, v: String) {
    let mut m = edits.write();
    if v.is_empty() {
        m.remove(slug);
    } else {
        m.insert(slug.to_string(), Value::String(v));
    }
}

/// The ref id `links_for_entity` resolves backlinks against, for any target.
fn detail_ref_id(target: &Selected) -> String {
    match target {
        Selected::Contact(id) | Selected::Task(id) | Selected::Event(id) => id.clone(),
    }
}

/// Load the detail data for a target: a contact card (custom entity + its type
/// specs) or a task (synthesized specs + values).
async fn load_detail(store: &Store, target: &Selected) -> Result<DetailData, String> {
    match target {
        Selected::Contact(id) => {
            let entity = store
                .custom_entities_get(id)
                .await
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| "this contact no longer exists".to_string())?;
            let ty = store
                .entity_types_get(&entity.type_slug)
                .await
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| "the contact type is missing".to_string())?;
            Ok(DetailData {
                name: contact_display(&entity),
                kind_label: "contact".to_string(),
                specs: ty.fields,
                values: entity.fields,
            })
        }
        Selected::Task(id) => {
            let task = store
                .tasks_get(id)
                .await
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| "this task no longer exists".to_string())?;
            Ok(DetailData {
                name: task.title.clone(),
                kind_label: "task".to_string(),
                specs: task_field_specs(),
                values: task_field_values(&task),
            })
        }
        Selected::Event(id) => {
            let event = store
                .events_get(id)
                .await
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| "this event no longer exists".to_string())?;
            let has_raw_at = event
                .at
                .as_deref()
                .is_some_and(|at| !at.trim().is_empty() && event_day(at).is_none());
            Ok(DetailData {
                name: if event.title.trim().is_empty() {
                    "(untitled event)".to_string()
                } else {
                    event.title.clone()
                },
                kind_label: "event".to_string(),
                specs: event_field_specs(has_raw_at),
                values: event_field_values(&event),
            })
        }
    }
}

/// Persist staged field edits for a target the right way: a contact merges its
/// values via custom_entities_update (title tracks the `title`/name); a task
/// maps the synthesized fields back into a TaskPatch.
async fn save_detail(
    store: &Store,
    target: &Selected,
    values: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    match target {
        Selected::Contact(id) => {
            // The card's display name is its entity title. If the type has a
            // `name` field we'd use it, but the model puts the primary name in
            // the title, so a contact has no `name` field — title is edited via
            // its own field row only if present. Persist all field values as a
            // full replacement patch (merge semantics: present keys set, absent
            // keys are left as-is by the fold, so we send the whole working map).
            let patch = CustomEntityPatch {
                title: None,
                fields: Some(values.clone()),
                scope: None,
            };
            store
                .custom_entities_update(id, patch, "system", None)
                .await
                .map_err(entity_err)?;
            Ok(())
        }
        Selected::Task(id) => {
            let get = |k: &str| {
                values
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let assignees: Vec<String> = get("assignees")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let patch = TaskPatch {
                title: Some(get("title")),
                body: Some(get("body")),
                status: TaskStatus::parse(&get("status")),
                priority: Some(Priority::from_str_lossy(&get("priority"))),
                assignees: Some(assignees),
                tags: None,
            };
            store
                .tasks_update(id, patch, "system")
                .await
                .map_err(|e| format!("{e:#}"))?;
            Ok(())
        }
        Selected::Event(id) => {
            let get = |k: &str| {
                values
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let assignees: Vec<String> = get("assignees")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // `at` is a double Option in the patch: Some(Some) sets it,
            // Some(None) clears it. We always send the composed value, so a
            // blanked date+raw clears `at` to unscheduled.
            let patch = EventPatch {
                title: Some(get("title")),
                body: Some(get("body")),
                at: Some(compose_event_at(values)),
                assignees: Some(assignees),
                tags: None,
            };
            store
                .events_update(id, patch, "system")
                .await
                .map_err(|e| format!("{e:#}"))?;
            Ok(())
        }
    }
}

/// Resolve the journal entries linked to `ref_id` (the emergent backlinks):
/// every links edge touching the id, keep the ones whose OTHER end is a
/// journal entry, fetch each entry, newest first, de-duplicated. This is the
/// cheapest correct source — the link rows already exist (emergence writes
/// them); no extra scan of journal bodies is needed.
async fn related_journal_entries(
    store: &Store,
    ref_id: &str,
) -> Result<Vec<JournalEntryView>, String> {
    let links: Vec<Link> = store
        .links_for_entity(ref_id)
        .await
        .map_err(|e| format!("{e:#}"))?;
    // Collect the journal-entry ids on the far side of each edge.
    let mut entry_ids: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for l in &links {
        let entry_id = if l.source_kind == "journal" && l.source_id != ref_id {
            Some(l.source_id.clone())
        } else if l.target_kind == "journal" && l.target_id != ref_id {
            Some(l.target_id.clone())
        } else {
            None
        };
        if let Some(eid) = entry_id {
            if seen.insert(eid.clone()) {
                entry_ids.push(eid);
            }
        }
    }
    let mut out = Vec::new();
    for eid in entry_ids {
        if let Ok(Some(view)) = store.journal_get(&eid).await {
            out.push(view);
        }
    }
    // Newest first by entry timestamp.
    out.sort_by(|a, b| b.entry.created_at.cmp(&a.entry.created_at));
    Ok(out)
}

// ── settings pane ────────────────────────────────────────────────────────────

/// Progress of a "Re-embed everything" run. Streamed to the UI off-thread.
#[derive(Clone, PartialEq)]
enum ReembedState {
    Idle,
    /// A pass is running; `done`/`total` come from embedding_stats between the
    /// backfill's time-boxed cycles.
    Running {
        done: i64,
        total: i64,
    },
    /// Finished a full drain (no pending items left). `embedded` counts what
    /// this run (re)computed across all cycles.
    Done {
        embedded: i64,
    },
    /// The engine degraded to the hash fallback mid-run (model unavailable):
    /// nothing was persisted under the real model tag; search stays keyword-only
    /// until the next launch. Honest, not a silent no-op.
    Latched,
    Failed(String),
}

/// The Settings section: the owner display name (identity.owner), a TRUTHFUL
/// readout of the embedder actually running, the backend config (native ONNX
/// with auto CPU/GPU, or optional manual Ollama), and a working "Re-embed
/// everything" action. Device is automatic — there is no device dropdown.
#[component]
fn SettingsPane(store: ReadOnlySignal<Store>, refresh: Signal<u32>) -> Element {
    // Load every persisted value once, into editable signals. Backend defaults
    // to native (the engine's own default) so a fresh store reads as native.
    let mut owner = use_signal(String::new);
    let mut backend = use_signal(|| Backend::Native.as_str().to_string());
    let mut model = use_signal(String::new);
    let mut ollama_url = use_signal(|| DEFAULT_OLLAMA_URL.to_string());
    let mut rerank_on = use_signal(|| false);
    let mut rerank_model = use_signal(String::new);
    let mut saved = use_signal(|| false);
    let mut reembed = use_signal(|| ReembedState::Idle);
    // Bumped after a re-embed cycle to re-pull the embedded/total stat.
    let mut stats_tick = use_signal(|| 0u32);

    let _load = use_resource(move || {
        let store = store();
        async move {
            owner.set(owner_name(&store).await);
            if let Ok(Some(v)) = store.config_get(EMBEDDER_BACKEND_KEY).await {
                // Normalize legacy/scaffold values ("onnx-local", "hash") to the
                // live domain so the dropdown reflects a real choice.
                backend.set(Backend::parse(&v).as_str().to_string());
            }
            if let Ok(Some(v)) = store.config_get(EMBEDDER_MODEL_KEY).await {
                model.set(v);
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

    // The TRUTH about the running engine (backend/device/model/latched) — shown
    // in the readout, never the pending pick.
    let running = store().embedder_state();

    // Embedding corpus truth: total embedded vs embeddable (cheap; one stat).
    // Re-pulled when `stats_tick` bumps so the readout tracks a re-embed.
    let stats = use_resource(move || {
        let store = store();
        async move {
            let _ = stats_tick();
            store.embedding_stats().await.ok()
        }
    });

    let save = move || {
        let store = store();
        let mut refresh = refresh;
        let data_dir = store.data_dir().to_path_buf();
        let backend_parsed = Backend::parse(&backend());
        let cfg = EmbedConfig {
            backend: backend_parsed,
            ollama_model: model().trim().to_string(),
            ollama_url: ollama_url().trim().to_string(),
        };
        let vals = [
            (IDENTITY_OWNER_KEY, owner().trim().to_string()),
            (EMBEDDER_BACKEND_KEY, backend_parsed.as_str().to_string()),
            (EMBEDDER_MODEL_KEY, model().trim().to_string()),
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
            // Also write the plaintext sidecar boot() reads before the store
            // opens — this is what makes the backend choice take effect (on the
            // next launch). Best-effort: a failed sidecar write just means the
            // durable config table still holds the choice for a later reconcile.
            let _ = cfg.save(&data_dir);
            saved.set(true);
            // Owner may have changed — nudge the shell to re-read the roster/name.
            refresh += 1;
        });
    };

    // Kick off a full re-embed: repeatedly call the time-boxed backfill (which
    // already embeds OFF the writer's critical path via spawn_blocking) until
    // no items are pending, streaming done/total between cycles. All async on
    // the UI's runtime — each await yields, so the window never freezes.
    let mut start_reembed = move || {
        if matches!(reembed(), ReembedState::Running { .. }) {
            return; // already running
        }
        let store = store();
        reembed.set(ReembedState::Running { done: 0, total: 0 });
        spawn(async move {
            let mut embedded_total: i64 = 0;
            loop {
                // Progress snapshot before the next cycle.
                if let Ok(s) = store.embedding_stats().await {
                    let done = (s.embeddable - s.pending).max(0);
                    reembed.set(ReembedState::Running {
                        done,
                        total: s.embeddable,
                    });
                    if s.pending == 0 {
                        reembed.set(ReembedState::Done {
                            embedded: embedded_total,
                        });
                        stats_tick += 1;
                        break;
                    }
                }
                match store.backfill_embeddings().await {
                    Ok(n) => {
                        embedded_total += n;
                        stats_tick += 1;
                        // The engine degraded to the hash fallback: the backfill
                        // pauses (returns 0 and latches) rather than poison the
                        // corpus. Say so instead of spinning forever.
                        if store.embedder_state().latched {
                            reembed.set(ReembedState::Latched);
                            break;
                        }
                        // A cycle that did nothing AND left items pending with no
                        // latch shouldn't happen, but guard against an infinite
                        // loop: treat it as done.
                        if n == 0 {
                            reembed.set(ReembedState::Done {
                                embedded: embedded_total,
                            });
                            break;
                        }
                    }
                    Err(e) => {
                        reembed.set(ReembedState::Failed(format!("{e:#}")));
                        break;
                    }
                }
            }
        });
    };

    let backend_val = backend();
    let is_ollama = backend_val == "ollama";
    // Does the saved/pending backend differ from what's actually running? If so
    // the readout says "restart to apply" — honesty over pretending it's live.
    let pending_restart = saved() && running.backend != Backend::parse(&backend_val).as_str();

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

                // TRUTHFUL running state — the actual engine, device, and count.
                div {
                    id: "embedder-current-state",
                    style: "background: {BG}; border: 1px solid {EDGE}; border-radius: 8px; \
                            padding: 0.7rem 0.85rem; margin-top: 0.6rem; font-size: 0.85rem; \
                            line-height: 1.55; color: {DIM};",
                    div {
                        "Running now: "
                        span { style: "color: {INK}; font-weight: 600;", "{running_engine_label(&running)}" }
                    }
                    if let Some(why) = running_engine_why(&running) {
                        div { style: "margin-top: 0.3rem; color: {FAINT};", "{why}" }
                    }
                    if let Some(Some(s)) = stats() {
                        div {
                            style: "margin-top: 0.4rem; color: {FAINT};",
                            "Embedded: {s.total} vector row(s) across {embedded_items(&s)} of {s.embeddable} item(s)"
                            if s.pending > 0 { " · {s.pending} pending" }
                        }
                    }
                }

                // Re-embed everything — runs the backfill to completion off-thread.
                div {
                    style: "display: flex; align-items: center; gap: 0.8rem; margin-top: 0.8rem;",
                    button {
                        id: "settings-reembed",
                        style: "background: {EDGE}; color: {INK}; border: 1px solid {FAINT}; \
                                border-radius: 8px; padding: 0.5rem 1.1rem; font-weight: 600; \
                                font-size: 0.88rem; cursor: pointer;",
                        disabled: matches!(reembed(), ReembedState::Running { .. }),
                        onclick: move |_| start_reembed(),
                        "Re-embed everything"
                    }
                    {reembed_status(&reembed())}
                }

                {field_label("Embedding backend")}
                select {
                    id: "settings-embedder-backend",
                    style: text_input_style(),
                    value: "{backend_val}",
                    onchange: move |e| { backend.set(e.value()); saved.set(false); },
                    option { value: "native", "On-device (ONNX BGE — auto GPU, CPU fallback)" }
                    option { value: "ollama", "Ollama (local server — uses its own GPU)" }
                }

                if is_ollama {
                    {field_label("Ollama model")}
                    input {
                        id: "settings-embedder-model",
                        style: text_input_style(),
                        placeholder: "e.g. nomic-embed-text",
                        value: "{model}",
                        oninput: move |e| { model.set(e.value()); saved.set(false); },
                    }
                    {field_label("Ollama server URL")}
                    input {
                        id: "settings-embedder-ollama-url",
                        style: text_input_style(),
                        placeholder: "{DEFAULT_OLLAMA_URL}",
                        value: "{ollama_url}",
                        oninput: move |e| { ollama_url.set(e.value()); saved.set(false); },
                    }
                }

                if pending_restart {
                    div {
                        id: "settings-restart-hint",
                        style: "font-size: 0.82rem; color: {GOLD}; line-height: 1.5; margin-top: 0.6rem;",
                        "Saved. Restart hive to switch the engine to your new backend — "
                        "the readout above keeps showing what's actually running until then."
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
                    div {
                        style: "font-size: 0.78rem; color: {FAINT}; line-height: 1.5; margin-top: 0.4rem;",
                        {reranker_note(&running)}
                    }
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

/// A friendly model name for the readout: "BGE-small" for the default BGE repo,
/// the ollama tag for ollama, "hash vectors" for the fallback, else the raw id.
fn friendly_model(state: &hive_core::store::EmbedderState) -> String {
    let m = &state.model;
    if state.backend == "hash" {
        return "hash vectors".to_string();
    }
    if let Some(tag) = m.strip_prefix("ollama:") {
        return tag.to_string();
    }
    let low = m.to_lowercase();
    if low.contains("bge-small") {
        "BGE-small".to_string()
    } else if low.contains("bge-large") {
        "BGE-large".to_string()
    } else if low.contains("bge-base") {
        "BGE-base".to_string()
    } else {
        // Strip an org prefix ("Xenova/…") for display.
        m.rsplit('/').next().unwrap_or(m).to_string()
    }
}

/// The truthful one-line running-engine label, e.g. "BGE-small on CPU",
/// "Ollama: nomic-embed-text", or "keyword search (hash vectors)".
fn running_engine_label(state: &hive_core::store::EmbedderState) -> String {
    if state.latched {
        return "keyword search only (model unavailable)".to_string();
    }
    match state.backend.as_str() {
        "ollama" => format!("Ollama: {}", friendly_model(state)),
        "hash" => "keyword search + hash vectors (CPU)".to_string(),
        _ => format!("{} on {}", friendly_model(state), state.device),
    }
}

/// The honest "why" line under the label when the running state might surprise
/// the user (latched, or CPU where they might expect GPU in-sandbox).
fn running_engine_why(state: &hive_core::store::EmbedderState) -> Option<String> {
    if state.latched {
        return Some(
            "The configured model couldn't load, so search fell back to keywords. \
             Restart to retry the model, or switch to Ollama."
                .to_string(),
        );
    }
    if state.backend == "native" && state.device == "CPU" {
        return Some(
            "Running on CPU. A native GPU isn't reachable from the sandbox; \
             for GPU, enable Ollama below (it runs on your GPU)."
                .to_string(),
        );
    }
    None
}

/// Items whose current embedding is present and fresh (the readout's "of N
/// items" numerator). `total` counts vector ROWS (chunks), not items, so the
/// truthful item count is embeddable minus pending.
fn embedded_items(s: &hive_shared::EmbeddingStats) -> i64 {
    (s.embeddable - s.pending).max(0)
}

/// The reranker-note truth: whether the toggle actually does anything right now.
fn reranker_note(state: &hive_core::store::EmbedderState) -> String {
    if state.backend == "native" && !state.latched {
        "Active when a reranker model is loaded on the native engine.".to_string()
    } else {
        "No effect with the current backend — reranking needs the native ONNX engine.".to_string()
    }
}

/// The re-embed progress/done indicator beside the button.
fn reembed_status(state: &ReembedState) -> Element {
    match state {
        ReembedState::Idle => rsx! {},
        ReembedState::Running { done, total } => rsx! {
            span {
                id: "settings-reembed-progress",
                style: "font-size: 0.85rem; color: {DIM};",
                if *total > 0 {
                    "Embedding… {done} / {total}"
                } else {
                    "Embedding…"
                }
            }
        },
        ReembedState::Done { embedded } => rsx! {
            span {
                id: "settings-reembed-done",
                style: "font-size: 0.85rem; color: {GOLD}; font-weight: 600;",
                "Done — {embedded} item(s) (re)embedded."
            }
        },
        ReembedState::Latched => rsx! {
            span {
                id: "settings-reembed-latched",
                style: "font-size: 0.85rem; color: #e07a5f;",
                "Paused: the model is unavailable, so search stays on keywords."
            }
        },
        ReembedState::Failed(err) => rsx! {
            span {
                id: "settings-reembed-error",
                style: "font-size: 0.85rem; color: #e07a5f;",
                "Re-embed failed: {err}"
            }
        },
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

    /// The calculated age: whole years, with the not-yet-this-year case
    /// subtracting one, and non-plain-date inputs yielding None.
    #[test]
    fn age_years_is_whole_calendar_years() {
        use super::age_years;
        // Birthday already passed this year.
        assert_eq!(age_years("1990-01-15", "2026-07-11"), Some(36));
        // Birthday still ahead this year → one less.
        assert_eq!(age_years("1990-12-31", "2026-07-11"), Some(35));
        // Birthday is exactly today.
        assert_eq!(age_years("2000-07-11", "2026-07-11"), Some(26));
        // Day before the birthday.
        assert_eq!(age_years("2000-07-12", "2026-07-11"), Some(25));
        // A future birthday has no age.
        assert_eq!(age_years("2030-01-01", "2026-07-11"), None);
        // Non-plain-date inputs (ISO timestamp, empty, garbage) → None.
        assert_eq!(age_years("1990-01-15T00:00:00Z", "2026-07-11"), None);
        assert_eq!(age_years("", "2026-07-11"), None);
        assert_eq!(age_years("not-a-date", "2026-07-11"), None);
    }

    /// Task grouping buckets every status column in board order and preserves
    /// the incoming (priority-then-recency) order within each bucket.
    #[test]
    fn group_by_status_buckets_all_four_columns() {
        use super::group_by_status;
        use hive_shared::{Priority, Task, TaskStatus};

        let task = |id: &str, status: TaskStatus| Task {
            id: id.to_string(),
            title: id.to_string(),
            body: String::new(),
            status,
            priority: Priority::Normal,
            tags: Vec::new(),
            assignees: Vec::new(),
            project: None,
            phase: None,
            due: None,
            origin_entry_id: None,
            anchor_text: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let grouped = group_by_status(vec![
            task("a", TaskStatus::Doing),
            task("b", TaskStatus::Todo),
            task("c", TaskStatus::Doing),
            task("d", TaskStatus::Done),
        ]);
        // Four columns, in board order, even the empty one (blocked).
        let cols: Vec<TaskStatus> = grouped.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            cols,
            vec![
                TaskStatus::Todo,
                TaskStatus::Doing,
                TaskStatus::Blocked,
                TaskStatus::Done
            ]
        );
        let doing: Vec<&str> = grouped[1].1.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(doing, vec!["a", "c"], "input order preserved in-bucket");
        assert_eq!(grouped[2].1.len(), 0, "blocked column present but empty");
    }

    // ── calendar library ──

    /// event_day accepts every shape the store/editor actually writes and
    /// rejects garbage (which then lands in the undated list, not a wrong cell).
    #[test]
    fn event_day_parses_accepted_shapes_and_rejects_garbage() {
        use super::event_day;
        // ISO date.
        assert_eq!(event_day("2026-07-15"), Some((2026, 7, 15)));
        // Frozen 24-char ISO (the op-log/now_iso shape).
        assert_eq!(event_day("2026-08-01T09:30:00.000Z"), Some((2026, 8, 1)));
        // ISO datetime without millis / Z.
        assert_eq!(event_day("2026-12-31T23:59"), Some((2026, 12, 31)));
        assert_eq!(event_day("2026-12-31t23:59:00"), Some((2026, 12, 31)));
        // Space-separated datetime (2024 is a leap year, so Feb 29 is valid).
        assert_eq!(event_day("2024-02-29 14:00"), Some((2024, 2, 29)));
        // Leading/trailing whitespace tolerated.
        assert_eq!(event_day("  2026-07-15  "), Some((2026, 7, 15)));

        // Garbage / vague / malformed → None.
        assert_eq!(event_day(""), None);
        assert_eq!(event_day("next Tuesday"), None);
        assert_eq!(event_day("2026/07/15"), None); // wrong separators
        assert_eq!(event_day("2026-13-01"), None); // month out of range
        assert_eq!(event_day("2026-00-10"), None); // month 0
        assert_eq!(event_day("2026-04-31"), None); // April has 30 days
        assert_eq!(event_day("2026-02-30"), None); // never
        assert_eq!(event_day("2026-07-15X10:00"), None); // bad separator after date
        assert_eq!(event_day("2026-7-15"), None); // unpadded month
    }

    /// 2026 is NOT a leap year, so Feb 29 2026 must be rejected; 2024/2000 are.
    #[test]
    fn event_day_honors_leap_years() {
        use super::event_day;
        assert_eq!(event_day("2026-02-29"), None, "2026 is not a leap year");
        assert_eq!(
            event_day("2024-02-29"),
            Some((2024, 2, 29)),
            "2024 is a leap year"
        );
        assert_eq!(
            event_day("2000-02-29"),
            Some((2000, 2, 29)),
            "2000 is a leap year"
        );
        assert_eq!(
            event_day("1900-02-29"),
            None,
            "1900 is NOT a leap year (century)"
        );
    }

    /// event_time pulls HH:MM only from a real datetime; a bare date has none.
    #[test]
    fn event_time_extracts_only_when_timed() {
        use super::event_time;
        assert_eq!(event_time("2026-07-15"), None);
        assert_eq!(
            event_time("2026-08-01T09:30:00.000Z").as_deref(),
            Some("09:30")
        );
        assert_eq!(event_time("2026-12-31 23:59").as_deref(), Some("23:59"));
        assert_eq!(event_time("2026-12-31T24:00"), None, "hour out of range");
        assert_eq!(event_time("next week"), None);
    }

    #[test]
    fn leap_and_month_lengths() {
        use super::{days_in_month, is_leap};
        assert!(is_leap(2024) && is_leap(2000) && !is_leap(2026) && !is_leap(1900));
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 12), 31);
        assert_eq!(days_in_month(2026, 13), 0);
    }

    /// Weekday of known dates (0 = Sunday). Jul 1 2026 is a Wednesday;
    /// Jul 11 2026 (today in this codebase) is a Saturday; the US epoch
    /// Jan 1 1970 is a Thursday.
    #[test]
    fn weekday_of_known_dates() {
        use super::weekday;
        assert_eq!(weekday(2026, 7, 1), 3, "2026-07-01 is Wednesday");
        assert_eq!(weekday(2026, 7, 11), 6, "2026-07-11 is Saturday");
        assert_eq!(weekday(1970, 1, 1), 4, "1970-01-01 is Thursday");
        assert_eq!(weekday(2000, 1, 1), 6, "2000-01-01 is Saturday");
        assert_eq!(weekday(2024, 2, 29), 4, "leap day 2024-02-29 is Thursday");
    }

    /// The month grid is always 42 cells; day 1 sits at its weekday offset and
    /// the last day is the month's length, with blanks filling the rest.
    #[test]
    fn month_grid_layout_and_leap_february() {
        use super::{month_grid, weekday};
        // July 2026: starts Wednesday (offset 3), 31 days.
        let grid = month_grid(2026, 7);
        assert_eq!(grid.len(), 42, "grid is a fixed 6x7");
        assert_eq!(grid[0], None);
        assert_eq!(grid[2], None);
        assert_eq!(grid[3], Some(1), "day 1 lands on its weekday (Wed)");
        assert_eq!(grid[3 + 30], Some(31), "last day present");
        assert_eq!(grid[3 + 31], None, "cell after the last day is blank");
        assert_eq!(grid.iter().flatten().count(), 31, "exactly 31 real days");

        // February 2024 (leap): 29 days, starts Thursday (offset 4).
        let feb = month_grid(2024, 2);
        assert_eq!(feb[weekday(2024, 2, 1) as usize], Some(1));
        assert_eq!(feb.iter().flatten().count(), 29, "leap Feb has 29 days");
        assert_eq!(*feb.iter().flatten().max().unwrap(), 29);

        // February 2026 (non-leap): 28 days.
        let feb26 = month_grid(2026, 2);
        assert_eq!(feb26.iter().flatten().count(), 28);
    }

    #[test]
    fn step_month_wraps_the_year() {
        use super::step_month;
        assert_eq!(step_month(2026, 12, true), (2027, 1));
        assert_eq!(step_month(2026, 1, false), (2025, 12));
        assert_eq!(step_month(2026, 7, true), (2026, 8));
        assert_eq!(step_month(2026, 7, false), (2026, 6));
    }

    /// compose_event_at recombines the split date/time editor fields into a
    /// clean ISO string that round-trips through event_day/event_time, keeps
    /// raw prose when there's no date, and clears to None when empty.
    #[test]
    fn compose_event_at_round_trips() {
        use super::{compose_event_at, event_day, event_time};
        use serde_json::json;

        let m = |v: serde_json::Value| v.as_object().unwrap().clone();

        // date + time → frozen ISO.
        let at = compose_event_at(&m(json!({"date": "2026-07-15", "time": "09:30"}))).unwrap();
        assert_eq!(event_day(&at), Some((2026, 7, 15)));
        assert_eq!(event_time(&at).as_deref(), Some("09:30"));

        // date alone → bare date.
        assert_eq!(
            compose_event_at(&m(json!({"date": "2026-07-15"}))).as_deref(),
            Some("2026-07-15")
        );

        // bad time is dropped, date kept.
        assert_eq!(
            compose_event_at(&m(json!({"date": "2026-07-15", "time": "nope"}))).as_deref(),
            Some("2026-07-15")
        );

        // no date but raw prose kept verbatim.
        assert_eq!(
            compose_event_at(&m(json!({"at_raw": "sometime next spring"}))).as_deref(),
            Some("sometime next spring")
        );

        // nothing → unscheduled.
        assert_eq!(compose_event_at(&m(json!({}))), None);
        assert_eq!(
            compose_event_at(&m(json!({"date": "", "at_raw": ""}))),
            None
        );
    }

    /// Placement buckets events by parsed day and routes the undated/vague ones
    /// to the undated list; a timed event sorts before an untimed same-day one.
    #[test]
    fn placed_and_undated_split_events() {
        use super::{placed_events, undated_events};
        use hive_shared::EventItem;

        let ev = |id: &str, title: &str, at: Option<&str>| EventItem {
            id: id.into(),
            title: title.into(),
            body: String::new(),
            at: at.map(str::to_string),
            tags: Vec::new(),
            assignees: Vec::new(),
            origin_entry_id: None,
            anchor_text: None,
            created_at: String::new(),
        };
        let list = vec![
            ev("e1", "Morning", Some("2026-07-15T08:00:00.000Z")),
            ev("e2", "All-day-ish", Some("2026-07-15")),
            ev("e3", "Other month", Some("2026-08-02")),
            ev("e4", "Vague", Some("next spring")),
            ev("e5", "Null", None),
        ];

        let placed = placed_events(&list);
        let day = placed.get(&(2026, 7, 15)).unwrap();
        assert_eq!(day.len(), 2, "both July 15 events placed together");
        assert_eq!(day[0].id, "e1", "timed event sorts first");
        assert_eq!(day[1].id, "e2");
        assert!(placed.contains_key(&(2026, 8, 2)));
        assert!(!placed.contains_key(&(2026, 7, 16)));

        let undated = undated_events(&list);
        let ids: Vec<&str> = undated.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["e4", "e5"], "vague + null are undated, in order");
    }
}
