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
use std::time::Duration;

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_wry_event_handler, Config, WindowBuilder};
use dioxus::prelude::*;
use hive_core::keys::{KeySource, KeychainKeySource, MemoryKeySource};
use hive_core::store::custom_entities::EntityFilter;
use hive_core::store::events::EventCreate;
use hive_core::store::mail::{
    EmailAddr, MailAccountAdminView, MailAccountEdit, MailAttachmentChip, MailMailboxView,
    MailMessageSummary, MailReplyMeta, MailThreadMessage,
};
use hive_core::store::mail_sync::{MailAddress, OutgoingEmail};
use hive_core::store::tasks::{TaskCreate, TaskFilter};
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

/// Whether the Shell spawns the mail sync driver at store open. On by default
/// (the flatpak ships `--share=network`); dev/CI opt OUT with
/// `HIVE_MAIL_ENABLED=0` so no test or headless run reaches for a JMAP server.
/// (Named to match the pre-pivot daemon's `HIVE_MAIL_ENABLED`, sense inverted
/// to default-on since the driver now lives in the shipping app, not a separate
/// binary you'd choose to launch.)
fn mail_driver_enabled() -> bool {
    !matches!(
        std::env::var("HIVE_MAIL_ENABLED")
            .unwrap_or_default()
            .trim(),
        "0" | "false" | "no" | "off"
    )
}

/// Seconds between mail sync driver ticks: `HIVE_MAIL_TICK`, default 30. Read
/// once when the driver spawns.
fn mail_driver_tick_secs() -> u64 {
    std::env::var("HIVE_MAIL_TICK")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(30)
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

    // The mail sync DRIVER (Slice A). Spawned ONCE at store open (Shell mounts
    // only in Journal mode), it drives jmap-sync's intact engine: every tick,
    // one bounded sync pass per due account (`mail_sync_tick`). Fire-and-forget
    // on the UI's tokio runtime — each await yields, and the heavy JMAP/network
    // work happens off the store's single writer thread inside the tick. A
    // freshly-added enabled account is picked up on the next tick with no
    // relaunch. Off by default in dev/CI (HIVE_MAIL_ENABLED); the flatpak turns
    // it on (it has --share=network).
    use_hook(|| {
        if !mail_driver_enabled() {
            return;
        }
        let store = store.peek().clone();
        let tick = Duration::from_secs(mail_driver_tick_secs());
        spawn(async move {
            loop {
                store.mail_sync_tick().await;
                tokio::time::sleep(tick).await;
            }
        });
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
                    EntityDetail { store, selected, refresh, target: sel, embedded: false }
                } else {
                    match section() {
                        Section::Journal => rsx! { JournalPane { store, active } },
                        Section::Tasks => rsx! { TasksPane { store, selected } },
                        Section::Contacts => rsx! { ContactsPane { store, refresh } },
                        Section::Identities => rsx! {
                            IdentitiesPane { store, section, active, refresh }
                        },
                        Section::Settings => rsx! { SettingsPane { store, refresh } },
                        Section::Mail => rsx! { MailPane { store } },
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

/// The owner as a FIRMLY-BOUND identity — who "you" are, for ownership/merge
/// ops. `identity.owner` stores an EXACT slug (set by "Set as owner" on a real
/// row); this returns the matching identity ONLY on an exact match, so no fuzzy
/// display-name guess can ever masquerade as an owner that owns no real row.
///
/// The match is drawn from the SAME roster the pane lists: first a real people
/// row by exact slug, else a `journal_writers` author whose slug equals the
/// stored value exactly (the synthesized `writer:` authors from the import).
/// Returns None when the stored value is empty/unset or names no real identity —
/// the pane then asks the user to pick which identity is them before offering
/// any claim/take-over. Note this deliberately does NOT fall back to
/// `owner_name()`/$USER: that stays a JOURNAL-AUTHOR default only (see the
/// composer), never an implicit owner identity.
async fn owner_binding(store: &Store) -> Option<Person> {
    let stored = store.config_get(IDENTITY_OWNER_KEY).await.ok().flatten()?;
    let stored = stored.trim();
    if stored.is_empty() {
        return None;
    }
    // A real people row wins on an exact slug match.
    if let Ok(Some(p)) = store.people_by_slug(stored).await {
        return Some(p);
    }
    // Else a bare author with journal history but no people row yet (an
    // imported/legacy writer the roster synthesizes) — matched by exact slug.
    if let Ok(writers) = store.journal_writers().await {
        if let Some(w) = writers.into_iter().find(|w| w.slug == stored) {
            return Some(Person {
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
    None
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

// ── mail pane (Slice A: accounts screen — the reading UI is a later slice) ────
//
// The sync driver (store/mail_sync.rs, spawned in Shell) runs jmap-sync's
// intact engine; this pane is where you connect a server and give each identity
// its own credentialed mailbox. Reading (folders / threads / labels / compose)
// arrives in the next update. Every add stores the password through the
// cc_credentials vault (mail_account_create does the cc_cred_put itself — the
// UI never persists or logs the raw secret); the driver's first pass discovers
// the real jmap_account_id.

/// A pending two-step delete: which account row is armed (the second click
/// commits). None = nothing armed.
type ArmedDelete = Option<String>;

// ── Mail: the Apple-Mail three-pane reader (Slice B, READ-ONLY) ───────────────
//
// Left: accounts grouped by owner identity, each with its mailboxes (folder +
// unread badge) and a global search box + an Accounts gear that flips to the
// Slice-A account manager. Middle: the selected mailbox's (or search's) message
// list, newest first. Right: the selected message's whole thread, each body
// rendered SAFELY (`render_email_body`) with a per-message "Load remote images"
// opt-in and attachment chips served from the blockstore as data: URLs.
//
// READ-ONLY: no mark-read-on-open, no label editing, no move/delete, no
// compose — those need JMAP write-back (a later slice). Read-state, labels, and
// folders all come FROM the sync.

/// What the middle list is showing: a selected mailbox, or search results.
#[derive(Clone, PartialEq)]
enum MailListSource {
    None,
    Mailbox { id: String, name: String },
    Search { query: String },
}

/// The prefill for the compose window. `None` for a fresh "New Message"; a
/// reply/reply-all builds one from the open message. `account_id` preselects the
/// From account (empty = let the user pick). Carries the reply threading headers
/// so the send keeps the conversation intact.
#[derive(Clone, PartialEq, Default)]
struct ComposeSeed {
    account_id: String,
    to: String,
    cc: String,
    subject: String,
    body: String,
    in_reply_to: Option<String>,
    references: Vec<String>,
}

#[component]
fn MailPane(store: ReadOnlySignal<Store>) -> Element {
    // Flip between the reader and the Slice-A account manager (the gear).
    let managing = use_signal(|| false);
    // Re-pulls the account list after add / toggle / resync / delete.
    let refresh = use_signal(|| 0u32);
    // Pane-local selection so the three panes switch in place.
    let source = use_signal(|| MailListSource::None);
    let selected_msg = use_signal(|| Option::<MailMessageSummary>::None);
    let search = use_signal(String::new);
    // Some(seed) = the compose overlay is open (fresh or a reply prefill).
    let compose = use_signal(|| Option::<ComposeSeed>::None);
    // Bumped by every message action (read/flag/label/move/archive/delete) so the
    // list, reader thread, and sidebar unread badges re-pull the optimistically
    // patched rows immediately.
    let mail_refresh = use_signal(|| 0u32);

    if managing() {
        return rsx! { MailAccountsManager { store, refresh, managing } };
    }

    // Decide the full-pane empty state vs. the three-pane reader by whether any
    // mail account exists. Re-pulled when `refresh` bumps (add/delete an
    // account) so the empty state clears the moment the first mailbox lands.
    let accounts = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            store
                .mail_accounts_admin_list()
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    // No accounts yet → the prominent onboarding empty state (keeps the gear
    // reachable via its own Add button, which lands on the add form).
    if let Some(Ok(list)) = accounts() {
        if list.is_empty() {
            return rsx! { MailEmptyState { managing } };
        }
    }

    rsx! {
        div {
            id: "mail-pane",
            style: "display: flex; height: 100%; min-height: 0; background: {BG}; position: relative;",
            MailSidebar { store, refresh, source, selected_msg, search, managing, compose, mail_refresh }
            MailList { store, source, selected_msg, mail_refresh }
            MailReader { store, selected_msg, compose, mail_refresh }
            if compose().is_some() {
                ComposeWindow { store, compose }
            }
        }
    }
}

/// The full-pane onboarding empty state shown when no mail account is connected:
/// a mark, a heading, a line, and a prominent Add Account button that opens the
/// account manager (landing on the add form). Replaces the three empty reader
/// panes so the first-run call to action is unmistakable.
#[component]
fn MailEmptyState(managing: Signal<bool>) -> Element {
    rsx! {
        div {
            id: "mail-empty",
            style: "display: flex; flex-direction: column; align-items: center; justify-content: center; \
                    height: 100%; min-height: 0; background: {BG}; text-align: center; padding: 2rem;",
            div { style: "font-size: 3.4rem; color: {GOLD}; line-height: 1;", "✉" }
            div {
                style: "font-size: 1.5rem; font-weight: 800; color: {INK}; margin-top: 1rem;",
                "Set up Mail"
            }
            div {
                style: "color: {DIM}; font-size: 0.95rem; line-height: 1.6; margin-top: 0.5rem; max-width: 30rem;",
                "Connect a mailbox to start syncing. Each identity can have its own."
            }
            button {
                id: "mail-empty-add",
                style: "margin-top: 1.4rem; background: {GOLD}; color: #14120e; border: none; \
                        border-radius: 10px; padding: 0.7rem 1.6rem; font: inherit; font-weight: 700; \
                        font-size: 0.95rem; cursor: pointer;",
                onclick: move |_| managing.set(true),
                "Add Account"
            }
        }
    }
}

/// Left column: search box, Accounts gear, then accounts grouped by owner with
/// each account's mailboxes (folder name + unread badge). Selecting a mailbox
/// sets the list source; submitting the search sets a Search source.
#[component]
fn MailSidebar(
    store: ReadOnlySignal<Store>,
    refresh: Signal<u32>,
    source: Signal<MailListSource>,
    selected_msg: Signal<Option<MailMessageSummary>>,
    search: Signal<String>,
    managing: Signal<bool>,
    compose: Signal<Option<ComposeSeed>>,
    mail_refresh: Signal<u32>,
) -> Element {
    let accounts = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            store
                .mail_accounts_admin_list()
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    let run_search = move |_| {
        let q = search().trim().to_string();
        if q.is_empty() {
            return;
        }
        let (mut selected_msg, mut source) = (selected_msg, source);
        selected_msg.set(None);
        source.set(MailListSource::Search { query: q });
    };

    rsx! {
        div {
            id: "mail-sidebar",
            style: "width: 250px; flex: none; height: 100%; overflow-y: auto; \
                    border-right: 1px solid {EDGE}; background: {PANEL}; \
                    padding: 0.9rem 0.7rem 2rem;",

            // header row: title + Accounts gear
            div {
                style: "display: flex; align-items: center; justify-content: space-between; \
                        margin-bottom: 0.7rem;",
                div {
                    style: "display: flex; align-items: baseline; gap: 0.4rem;",
                    div { style: "font-size: 1.1rem; font-weight: 700;", "Mail" }
                    div { style: "color: {GOLD};", "✉" }
                }
                button {
                    id: "mail-accounts-manage",
                    style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                            border-radius: 8px; padding: 0.3rem 0.5rem; font-size: 0.9rem; \
                            cursor: pointer;",
                    title: "Manage accounts",
                    onclick: move |_| managing.set(true),
                    "⚙"
                }
            }

            // New Message — opens the compose overlay with a blank seed.
            button {
                id: "mail-compose-new",
                style: "width: 100%; box-sizing: border-box; background: {GOLD}; color: #14120e; \
                        border: none; border-radius: 8px; padding: 0.5rem 0.8rem; font: inherit; \
                        font-weight: 700; font-size: 0.86rem; cursor: pointer; margin-bottom: 0.7rem;",
                onclick: move |_| {
                    let mut compose = compose;
                    compose.set(Some(ComposeSeed::default()));
                },
                "✎ New Message"
            }

            // global search
            input {
                id: "mail-search",
                style: "{text_input_style()} margin-top: 0;",
                r#type: "search",
                placeholder: "Search all mail…",
                value: "{search}",
                oninput: move |e| search.set(e.value()),
                onkeydown: move |e| {
                    if e.key() == Key::Enter {
                        run_search(());
                    }
                },
            }

            // accounts → mailboxes
            div {
                style: "margin-top: 1rem;",
                match accounts() {
                    None => muted("loading…"),
                    Some(Err(e)) => muted(&format!("accounts unavailable: {e}")),
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div {
                            style: "color: {FAINT}; font-size: 0.85rem; padding: 0.8rem 0.2rem; line-height: 1.5;",
                            "No mailboxes yet. Tap ⚙ to connect one."
                        }
                    },
                    Some(Ok(list)) => {
                        let groups = group_accounts_by_owner(list);
                        rsx! {
                            for (owner, accts) in groups.into_iter() {
                                div {
                                    style: "margin-bottom: 1rem;",
                                    div {
                                        style: "font-size: 0.72rem; font-weight: 700; letter-spacing: 0.07em; \
                                                text-transform: uppercase; color: {GOLD}; margin: 0.2rem 0.2rem 0.4rem;",
                                        "{owner}"
                                    }
                                    for acct in accts.into_iter() {
                                        {mail_account_group(store, acct, source, selected_msg, mail_refresh)}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One account block in the sidebar: its address, then its mailboxes as
/// selectable rows with an unread badge. A plain fn (its `MailAccountAdminView`
/// has no `PartialEq`).
fn mail_account_group(
    store: ReadOnlySignal<Store>,
    acct: MailAccountAdminView,
    source: Signal<MailListSource>,
    selected_msg: Signal<Option<MailMessageSummary>>,
    mail_refresh: Signal<u32>,
) -> Element {
    let account_id = acct.id.clone();
    let mailboxes = use_resource(move || {
        let store = store();
        let account_id = account_id.clone();
        async move {
            // Re-pull unread badges after every message action.
            let _ = mail_refresh();
            store
                .mail_mailboxes_list(&account_id)
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    rsx! {
        div {
            id: "mail-account-{acct.id}",
            style: "margin-bottom: 0.5rem;",
            div {
                style: "font-size: 0.82rem; color: {DIM}; padding: 0.1rem 0.2rem 0.25rem; \
                        overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                title: "{acct.address}",
                "{acct.address}"
            }
            match mailboxes() {
                None => rsx! {
                    div { style: "color: {FAINT}; font-size: 0.78rem; padding: 0.2rem 0.4rem;", "…" }
                },
                Some(Err(e)) => rsx! {
                    div { style: "color: #e07a5f; font-size: 0.76rem; padding: 0.2rem 0.4rem;", "{e}" }
                },
                Some(Ok(boxes)) if boxes.is_empty() => rsx! {
                    div {
                        style: "color: {FAINT}; font-size: 0.76rem; padding: 0.2rem 0.4rem;",
                        "no folders synced yet"
                    }
                },
                Some(Ok(boxes)) => rsx! {
                    for mbox in boxes.into_iter() {
                        {mail_mailbox_row(mbox, source, selected_msg)}
                    }
                },
            }
        }
    }
}

/// One selectable mailbox row: folder icon + name + unread badge, highlighted
/// when it is the current list source.
fn mail_mailbox_row(
    mbox: MailMailboxView,
    source: Signal<MailListSource>,
    selected_msg: Signal<Option<MailMessageSummary>>,
) -> Element {
    let is_active = matches!(source(), MailListSource::Mailbox { ref id, .. } if id == &mbox.id);
    let bg = if is_active { EDGE } else { "transparent" };
    let weight = if mbox.unread > 0 { "700" } else { "500" };
    let icon = mailbox_icon(mbox.role.as_deref(), &mbox.name);
    let mbox_id = mbox.id.clone();
    let mbox_name = mbox.name.clone();

    rsx! {
        div {
            id: "mail-mailbox-{mbox.id}",
            style: "display: flex; align-items: center; gap: 0.45rem; padding: 0.35rem 0.5rem; \
                    border-radius: 7px; cursor: pointer; background: {bg}; margin: 0.05rem 0;",
            onclick: move |_| {
                let (mut selected_msg, mut source) = (selected_msg, source);
                selected_msg.set(None);
                source.set(MailListSource::Mailbox { id: mbox_id.clone(), name: mbox_name.clone() });
            },
            span { style: "font-size: 0.85rem; width: 1rem; text-align: center;", "{icon}" }
            span {
                style: "flex: 1; min-width: 0; font-size: 0.86rem; font-weight: {weight}; \
                        color: {INK}; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                "{mbox.name}"
            }
            if mbox.unread > 0 {
                span {
                    style: "font-size: 0.72rem; font-weight: 700; color: #14120e; background: {GOLD}; \
                            border-radius: 999px; padding: 0.05rem 0.42rem; min-width: 1rem; text-align: center;",
                    "{mbox.unread}"
                }
            }
        }
    }
}

/// Middle column: the selected mailbox's or search's messages, newest first.
#[component]
fn MailList(
    store: ReadOnlySignal<Store>,
    source: Signal<MailListSource>,
    selected_msg: Signal<Option<MailMessageSummary>>,
    mail_refresh: Signal<u32>,
) -> Element {
    // Re-pulls whenever the source changes OR an action patched a row.
    let messages = use_resource(move || {
        let store = store();
        let src = source();
        async move {
            let _ = mail_refresh();
            match src {
                MailListSource::None => Ok(Vec::new()),
                MailListSource::Mailbox { id, .. } => store
                    .mail_messages_by_mailbox(&id, 200)
                    .await
                    .map_err(|e| format!("{e:#}")),
                MailListSource::Search { query } => store
                    .mail_search(&query, 200)
                    .await
                    .map_err(|e| format!("{e:#}")),
            }
        }
    });

    let header = match source() {
        MailListSource::None => String::new(),
        MailListSource::Mailbox { name, .. } => name,
        MailListSource::Search { query } => format!("Search: {query}"),
    };

    rsx! {
        div {
            id: "mail-list",
            style: "width: 340px; flex: none; height: 100%; overflow-y: auto; \
                    border-right: 1px solid {EDGE};",
            if !header.is_empty() {
                div {
                    style: "position: sticky; top: 0; background: {BG}; z-index: 1; \
                            padding: 0.8rem 1rem 0.6rem; border-bottom: 1px solid {EDGE}; \
                            font-weight: 700; font-size: 0.95rem; overflow: hidden; \
                            text-overflow: ellipsis; white-space: nowrap;",
                    "{header}"
                }
            }
            match messages() {
                None if matches!(source(), MailListSource::None) => rsx! {
                    div {
                        style: "color: {FAINT}; font-size: 0.88rem; padding: 2rem 1rem; text-align: center; line-height: 1.6;",
                        "Pick a mailbox on the left, or search, to read your mail."
                    }
                },
                None => muted("loading messages…"),
                Some(Err(e)) => muted(&format!("could not load: {e}")),
                Some(Ok(list)) if list.is_empty() => rsx! {
                    div {
                        style: "color: {FAINT}; font-size: 0.88rem; padding: 2rem 1rem; text-align: center;",
                        "No messages here."
                    }
                },
                Some(Ok(list)) => rsx! {
                    for msg in list.into_iter() {
                        {mail_list_row(msg, selected_msg)}
                    }
                },
            }
        }
    }
}

/// One message row in the middle list: unread dot, sender, subject, snippet,
/// short date, and small label/flag chips. Click selects it for the reader.
fn mail_list_row(
    msg: MailMessageSummary,
    selected_msg: Signal<Option<MailMessageSummary>>,
) -> Element {
    let is_selected = selected_msg()
        .as_ref()
        .map(|m| m.id == msg.id)
        .unwrap_or(false);
    let unread = !msg.labels.iter().any(|l| l == "seen");
    let bg = if is_selected { EDGE } else { "transparent" };
    let sender_weight = if unread { "700" } else { "600" };
    let sender = sender_display(&msg.from);
    let subject = if msg.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        msg.subject.clone()
    };
    let date = short_relative(&msg.received_at);
    // Chips: everything but the "seen" marker (that's the unread dot's job).
    let chips: Vec<String> = msg
        .labels
        .iter()
        .filter(|l| l.as_str() != "seen")
        .cloned()
        .collect();
    let msg_for_click = msg.clone();

    rsx! {
        div {
            id: "mail-msg-{msg.id}",
            style: "display: flex; gap: 0.5rem; padding: 0.6rem 0.9rem; cursor: pointer; \
                    background: {bg}; border-bottom: 1px solid {EDGE};",
            onclick: move |_| {
                let mut selected_msg = selected_msg;
                selected_msg.set(Some(msg_for_click.clone()));
            },
            // unread dot column
            div {
                style: "flex: none; width: 0.5rem; padding-top: 0.35rem;",
                if unread {
                    div { style: "width: 0.5rem; height: 0.5rem; border-radius: 50%; background: {GOLD};" }
                }
            }
            // main column
            div {
                style: "flex: 1; min-width: 0;",
                div {
                    style: "display: flex; align-items: baseline; gap: 0.5rem;",
                    div {
                        style: "flex: 1; min-width: 0; font-weight: {sender_weight}; \
                                font-size: 0.88rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                        "{sender}"
                    }
                    div {
                        style: "flex: none; color: {FAINT}; font-size: 0.74rem;",
                        "{date}"
                    }
                }
                div {
                    style: "font-size: 0.85rem; color: {INK}; margin-top: 0.1rem; \
                            overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                    "{subject}"
                }
                if let Some(snip) = msg.snippet.as_ref().filter(|s| !s.trim().is_empty()) {
                    div {
                        style: "font-size: 0.8rem; color: {DIM}; margin-top: 0.1rem; \
                                overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                        "{snip}"
                    }
                }
                if msg.has_attachments || !chips.is_empty() {
                    div {
                        style: "display: flex; flex-wrap: wrap; gap: 0.3rem; margin-top: 0.35rem; align-items: center;",
                        if msg.has_attachments {
                            span { style: "font-size: 0.72rem; color: {DIM};", "📎" }
                        }
                        for chip in chips.into_iter() {
                            {label_chip(&chip)}
                        }
                    }
                }
            }
        }
    }
}

/// Right column: the selected message's whole thread. Prior messages collapse
/// to a one-line header; the selected message expands with its sanitized body,
/// label chips, and attachment chips.
#[component]
fn MailReader(
    store: ReadOnlySignal<Store>,
    selected_msg: Signal<Option<MailMessageSummary>>,
    compose: Signal<Option<ComposeSeed>>,
    mail_refresh: Signal<u32>,
) -> Element {
    let thread = use_resource(move || {
        let store = store();
        let sel = selected_msg();
        async move {
            let _ = mail_refresh();
            match sel {
                None => Ok(None),
                Some(msg) => store
                    .mail_thread_get(&msg.thread_id)
                    .await
                    .map(Some)
                    .map_err(|e| format!("{e:#}")),
            }
        }
    });

    // Auto-mark-read on open (Apple behavior): opening an unread message marks
    // it read. Keyed on the selected id so it fires once per open, off the UI
    // thread; the optimistic patch + mail_refresh bump repaint the unread dots.
    use_effect(move || {
        let Some(msg) = selected_msg() else { return };
        let unread = !msg.labels.iter().any(|l| l == "seen");
        if !unread {
            return;
        }
        let store = store();
        let mut mail_refresh = mail_refresh;
        let id = msg.id.clone();
        spawn(async move {
            if store.mail_mark_read(&id, true).await.is_ok() {
                mail_refresh.set(mail_refresh() + 1);
            }
        });
    });

    let focused_id = selected_msg().map(|m| m.id);

    rsx! {
        div {
            id: "mail-reader",
            style: "flex: 1; min-width: 0; height: 100%; overflow-y: auto; background: {BG};",
            match (selected_msg().is_some(), thread()) {
                (false, _) => rsx! {
                    div {
                        style: "height: 100%; display: flex; align-items: center; justify-content: center; \
                                color: {FAINT}; font-size: 0.9rem; text-align: center; padding: 2rem;",
                        "Select a message to read it here."
                    }
                },
                (true, None) => muted("opening…"),
                (true, Some(Err(e))) => muted(&format!("could not open thread: {e}")),
                (true, Some(Ok(None))) => muted("message not found."),
                (true, Some(Ok(Some(thread)))) => {
                    let subject = if thread.subject.trim().is_empty() {
                        "(no subject)".to_string()
                    } else {
                        thread.subject.clone()
                    };
                    let count = thread.messages.len();
                    rsx! {
                        div {
                            style: "max-width: 780px; margin: 0 auto; padding: 1.6rem 1.6rem 4rem;",
                            div {
                                style: "font-size: 1.3rem; font-weight: 700; line-height: 1.35;",
                                "{subject}"
                            }
                            if count > 1 {
                                div {
                                    style: "color: {FAINT}; font-size: 0.8rem; margin-top: 0.25rem;",
                                    "{count} messages in this conversation"
                                }
                            }
                            div {
                                style: "margin-top: 1.2rem;",
                                for tmsg in thread.messages.into_iter() {
                                    {mail_thread_message(store, tmsg, focused_id.as_deref(), compose, mail_refresh)}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One message inside the reading pane. The focused message (the one clicked in
/// the list) renders expanded; the rest of the thread collapses to a clickable
/// one-line header that expands on demand. A plain fn (`MailThreadMessage` has
/// no `PartialEq`).
fn mail_thread_message(
    store: ReadOnlySignal<Store>,
    tmsg: MailThreadMessage,
    focused_id: Option<&str>,
    compose: Signal<Option<ComposeSeed>>,
    mail_refresh: Signal<u32>,
) -> Element {
    let is_focused = focused_id == Some(tmsg.summary.id.as_str());
    let mut expanded = use_signal(|| is_focused);
    let msg_id = tmsg.summary.id.clone();
    // Read/flag state + membership drive the action bar's toggles.
    let is_seen = tmsg.summary.labels.iter().any(|l| l == "seen");
    let is_flagged = tmsg.summary.labels.iter().any(|l| l == "flagged");
    let account_id = tmsg.summary.account_id.clone();
    let mailbox_ids = tmsg.summary.mailbox_ids.clone();
    // The body we quote into a reply (the sanitized plaintext already stored).
    let original_body = tmsg.body_text.clone();
    let sender = tmsg.summary.from.clone();
    let date = short_time(&tmsg.summary.received_at);
    let to_line = if tmsg.summary.to.is_empty() {
        String::new()
    } else {
        format!("to {}", tmsg.summary.to.join(", "))
    };
    let chips: Vec<String> = tmsg
        .summary
        .labels
        .iter()
        .filter(|l| l.as_str() != "seen")
        .cloned()
        .collect();

    if !expanded() {
        // Collapsed prior message: a single header line.
        let snippet = tmsg.summary.snippet.clone().unwrap_or_default();
        return rsx! {
            div {
                style: "border: 1px solid {EDGE}; border-radius: 10px; padding: 0.6rem 0.9rem; \
                        margin-bottom: 0.7rem; cursor: pointer; background: {PANEL};",
                onclick: move |_| expanded.set(true),
                div {
                    style: "display: flex; gap: 0.6rem; align-items: baseline;",
                    div {
                        style: "flex: 1; min-width: 0; font-weight: 600; font-size: 0.86rem; \
                                overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                        "{sender}"
                    }
                    div { style: "flex: none; color: {FAINT}; font-size: 0.74rem;", "{date}" }
                }
                if !snippet.trim().is_empty() {
                    div {
                        style: "font-size: 0.8rem; color: {DIM}; margin-top: 0.15rem; \
                                overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                        "{snippet}"
                    }
                }
            }
        };
    }

    // Open the compose overlay prefilled as a reply (all = reply-all) to this
    // message: pull the parent's threading headers + participants, then seed.
    let open_reply = move |all: bool| {
        let store = store();
        let id = msg_id.clone();
        // Clone the quoted body per call so this closure stays `Fn` (the async
        // block must not move a capture out of the closure's environment).
        let original_body = original_body.clone();
        let mut compose = compose;
        spawn(async move {
            let Ok(Some(meta)) = store.mail_message_reply_meta(&id).await else {
                return;
            };
            let (to, cc) = if all {
                reply_all_recipients(&meta)
            } else {
                (reply_to_recipients(&meta), Vec::new())
            };
            let sender = meta
                .from
                .first()
                .map(addr_label)
                .unwrap_or_else(|| meta.account_address.clone());
            let seed = ComposeSeed {
                account_id: meta.account_id.clone(),
                to: addrs_to_field(&to),
                cc: addrs_to_field(&cc),
                subject: reply_subject(&meta.subject),
                body: format!(
                    "\n\n{}",
                    quote_reply_body(&meta.received_at, &sender, &original_body)
                ),
                in_reply_to: meta.message_id_hdr.clone(),
                references: reply_references(&meta),
            };
            compose.set(Some(seed));
        });
    };

    rsx! {
        div {
            style: "border: 1px solid {EDGE}; border-radius: 10px; padding: 0.9rem 1.1rem; \
                    margin-bottom: 0.9rem; background: {PANEL};",
            // header
            div {
                style: "display: flex; gap: 0.6rem; align-items: baseline;",
                div {
                    style: "flex: 1; min-width: 0; font-weight: 700; font-size: 0.92rem; \
                            overflow: hidden; text-overflow: ellipsis;",
                    "{sender}"
                }
                div { style: "flex: none; color: {FAINT}; font-size: 0.78rem;", "{date}" }
            }
            if !to_line.is_empty() {
                div {
                    style: "color: {DIM}; font-size: 0.78rem; margin-top: 0.15rem; \
                            overflow: hidden; text-overflow: ellipsis;",
                    "{to_line}"
                }
            }
            if !chips.is_empty() {
                div {
                    style: "display: flex; flex-wrap: wrap; gap: 0.3rem; margin-top: 0.5rem;",
                    for chip in chips.into_iter() {
                        {label_chip(&chip)}
                    }
                }
            }
            // reply / reply-all
            div {
                style: "display: flex; gap: 0.5rem; margin-top: 0.7rem;",
                button {
                    id: "mail-reply-{tmsg.summary.id}",
                    style: reply_button_style(),
                    onclick: {
                        let open_reply = open_reply.clone();
                        move |_| open_reply(false)
                    },
                    "↩ Reply"
                }
                button {
                    id: "mail-replyall-{tmsg.summary.id}",
                    style: reply_button_style(),
                    onclick: move |_| open_reply(true),
                    "↩↩ Reply all"
                }
            }
            // per-message actions: read/flag/move/labels/archive/delete
            MailActionBar {
                store,
                message_id: tmsg.summary.id.clone(),
                account_id,
                is_seen,
                is_flagged,
                mailbox_ids,
                mail_refresh,
            }
            // the SANITIZED body + its remote-image opt-in
            MailBodyView { id: tmsg.summary.id.clone(), body: tmsg.body_text.clone() }
            // attachments
            if !tmsg.attachments.is_empty() {
                div {
                    style: "display: flex; flex-wrap: wrap; gap: 0.5rem; margin-top: 0.9rem; \
                            padding-top: 0.7rem; border-top: 1px solid {EDGE};",
                    for att in tmsg.attachments.into_iter() {
                        {mail_attachment_chip(store, att)}
                    }
                }
            }
        }
    }
}

/// The per-message action row on the focused reader message: mark unread/read,
/// flag, a Move-to menu over the account's mailboxes, membership toggles for the
/// account's label mailboxes (role-less), Archive, and Delete (→ Trash, or a
/// two-step permanent delete when the message is already in Trash). Every action
/// calls the store (which optimistically patches the derived row + enqueues the
/// server write), then bumps `mail_refresh` so the list, thread, and unread
/// badges repaint. A `#[component]` because all its props derive PartialEq
/// (String/bool/Vec/signals) — the reader structs don't, so this stays split out.
#[component]
fn MailActionBar(
    store: ReadOnlySignal<Store>,
    message_id: String,
    account_id: String,
    is_seen: bool,
    is_flagged: bool,
    mailbox_ids: Vec<String>,
    mail_refresh: Signal<u32>,
) -> Element {
    // The account's mailboxes drive the Move menu + label toggles. Re-pulled on
    // action so membership badges stay live.
    let acct_for_boxes = account_id.clone();
    let mailboxes = use_resource(move || {
        let store = store();
        let acct = acct_for_boxes.clone();
        async move {
            let _ = mail_refresh();
            store.mail_mailboxes_list(&acct).await.unwrap_or_default()
        }
    });
    let boxes = mailboxes().unwrap_or_default();

    let mut menu_open = use_signal(|| false);
    // Two-step permanent-delete confirm.
    let mut confirm_perm = use_signal(|| false);
    let mut err = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);

    // Is the message currently in the Trash mailbox? (permanent-delete gate)
    let trash_jmap = boxes
        .iter()
        .find(|b| b.role.as_deref() == Some("trash"))
        .map(|b| b.jmap_id.clone());
    let in_trash = trash_jmap
        .as_ref()
        .map(|t| mailbox_ids.iter().any(|m| m == t))
        .unwrap_or(false);

    // Run a store action off the UI thread, then repaint. `busy` guards against
    // double-fire; errors surface in-pane (never a panic). Captures a cloned id
    // (leaving `message_id` free for the element-id format strings) and only Copy
    // signals besides, so it is itself `Clone` — cloned into each onclick below.
    let run = {
        let mid = message_id.clone();
        move |fut_kind: MailActionKind| {
            if busy() {
                return;
            }
            busy.set(true);
            err.set(None);
            menu_open.set(false);
            confirm_perm.set(false);
            let store = store();
            let id = mid.clone();
            let mut mail_refresh = mail_refresh;
            let mut busy = busy;
            let mut err = err;
            spawn(async move {
                let res = match fut_kind {
                    MailActionKind::MarkRead(read) => store.mail_mark_read(&id, read).await,
                    MailActionKind::Flag(on) => store.mail_set_flagged(&id, on).await,
                    MailActionKind::Move(target) => store.mail_move(&id, &target).await,
                    MailActionKind::AddLabel(m) => store.mail_add_label(&id, &m).await,
                    MailActionKind::RemoveLabel(m) => store.mail_remove_label(&id, &m).await,
                    MailActionKind::Archive => store.mail_archive(&id).await,
                    MailActionKind::Delete => store.mail_delete(&id).await,
                    MailActionKind::DeletePermanently => store.mail_delete_permanently(&id).await,
                };
                match res {
                    Ok(_) => mail_refresh.set(mail_refresh() + 1),
                    Err(e) => err.set(Some(format!("{e:#}"))),
                }
                busy.set(false);
            });
        }
    };

    // Label mailboxes = role-less ones (folders carry a role). Membership is a
    // toggle reflecting the current mailbox_ids.
    let label_boxes: Vec<MailMailboxView> =
        boxes.iter().filter(|b| b.role.is_none()).cloned().collect();

    rsx! {
        div {
            style: "display: flex; flex-wrap: wrap; gap: 0.4rem; align-items: center; \
                    margin-top: 0.6rem; position: relative;",

            // mark unread / read
            button {
                id: "mail-read-{message_id}",
                style: reply_button_style(),
                disabled: busy(),
                title: if is_seen { "Mark as unread" } else { "Mark as read" },
                onclick: {
                    let mut run = run.clone();
                    move |_| run(MailActionKind::MarkRead(!is_seen))
                },
                if is_seen { "✉ Mark unread" } else { "✓ Mark read" }
            }

            // flag / unflag
            button {
                id: "mail-flag-{message_id}",
                style: reply_button_style(),
                disabled: busy(),
                title: if is_flagged { "Remove flag" } else { "Flag" },
                onclick: {
                    let mut run = run.clone();
                    move |_| run(MailActionKind::Flag(!is_flagged))
                },
                if is_flagged { "⚑ Unflag" } else { "⚐ Flag" }
            }

            // move-to menu
            div {
                style: "position: relative;",
                button {
                    id: "mail-move-{message_id}",
                    style: reply_button_style(),
                    disabled: busy(),
                    title: "Move to a folder",
                    onclick: move |_| {
                        let now = menu_open();
                        menu_open.set(!now);
                    },
                    "🗂 Move ▾"
                }
                if menu_open() {
                    div {
                        id: "mail-move-menu-{message_id}",
                        style: "position: absolute; top: 100%; left: 0; z-index: 30; margin-top: 0.2rem; \
                                background: {PANEL}; border: 1px solid {EDGE}; border-radius: 10px; \
                                box-shadow: 0 12px 30px rgba(0,0,0,0.5); padding: 0.3rem; min-width: 180px; \
                                max-height: 260px; overflow-y: auto;",
                        if boxes.is_empty() {
                            div { style: "color: {FAINT}; font-size: 0.8rem; padding: 0.4rem 0.5rem;", "No folders" }
                        }
                        for mbox in boxes.iter().cloned() {
                            {
                                let target = mbox.jmap_id.clone();
                                let here = mailbox_ids.iter().any(|m| m == &target);
                                let icon = mailbox_icon(mbox.role.as_deref(), &mbox.name);
                                rsx! {
                                    button {
                                        id: "mail-move-{message_id}-{mbox.id}",
                                        style: "display: flex; width: 100%; align-items: center; gap: 0.45rem; \
                                                background: none; border: none; color: {INK}; text-align: left; \
                                                border-radius: 7px; padding: 0.4rem 0.5rem; font: inherit; \
                                                font-size: 0.84rem; cursor: pointer;",
                                        disabled: busy() || here,
                                        onclick: {
                                            let mut run = run.clone();
                                            move |_| run(MailActionKind::Move(target.clone()))
                                        },
                                        span { style: "width: 1rem; text-align: center;", "{icon}" }
                                        span { style: "flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;", "{mbox.name}" }
                                        if here {
                                            span { style: "color: {FAINT}; font-size: 0.72rem;", "here" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // archive
            button {
                id: "mail-archive-{message_id}",
                style: reply_button_style(),
                disabled: busy(),
                title: "Archive",
                onclick: {
                    let mut run = run.clone();
                    move |_| run(MailActionKind::Archive)
                },
                "🗄 Archive"
            }

            // delete — soft (→ Trash) or permanent (already in Trash, two-step)
            if in_trash {
                if confirm_perm() {
                    button {
                        id: "mail-delete-perm-{message_id}",
                        style: "background: #7a2e22; border: 1px solid #e07a5f; color: #ffe; \
                                border-radius: 8px; padding: 0.35rem 0.8rem; font: inherit; \
                                font-size: 0.82rem; font-weight: 700; cursor: pointer;",
                        disabled: busy(),
                        title: "Permanently delete — this cannot be undone",
                        onclick: {
                            let mut run = run.clone();
                            move |_| run(MailActionKind::DeletePermanently)
                        },
                        "Delete forever?"
                    }
                } else {
                    button {
                        id: "mail-delete-{message_id}",
                        style: reply_button_style(),
                        disabled: busy(),
                        title: "Delete permanently",
                        onclick: move |_| confirm_perm.set(true),
                        "🗑 Delete…"
                    }
                }
            } else {
                button {
                    id: "mail-delete-{message_id}",
                    style: reply_button_style(),
                    disabled: busy(),
                    title: "Move to Trash",
                    onclick: {
                        let mut run = run.clone();
                        move |_| run(MailActionKind::Delete)
                    },
                    "🗑 Delete"
                }
            }

            // label membership toggles (role-less mailboxes)
            if !label_boxes.is_empty() {
                div {
                    style: "display: flex; flex-wrap: wrap; gap: 0.3rem; align-items: center; \
                            width: 100%; margin-top: 0.35rem;",
                    span { style: "color: {FAINT}; font-size: 0.72rem;", "Labels:" }
                    for mbox in label_boxes.into_iter() {
                        {
                            let mjmap = mbox.jmap_id.clone();
                            let on = mailbox_ids.iter().any(|m| m == &mjmap);
                            let (bg, fg, border) = if on { (GOLD, "#14120e", GOLD) } else { ("transparent", DIM, EDGE) };
                            rsx! {
                                button {
                                    id: "mail-label-{message_id}-{mbox.id}",
                                    style: "font-size: 0.72rem; font-weight: 600; color: {fg}; background: {bg}; \
                                            border: 1px solid {border}; border-radius: 999px; padding: 0.1rem 0.5rem; \
                                            cursor: pointer;",
                                    disabled: busy(),
                                    title: if on { "Remove label" } else { "Add label" },
                                    onclick: {
                                        let mut run = run.clone();
                                        move |_| {
                                            if on {
                                                run(MailActionKind::RemoveLabel(mjmap.clone()))
                                            } else {
                                                run(MailActionKind::AddLabel(mjmap.clone()))
                                            }
                                        }
                                    },
                                    if on { "✓ {mbox.name}" } else { "{mbox.name}" }
                                }
                            }
                        }
                    }
                }
            }

            if let Some(e) = err() {
                div { style: "color: #e07a5f; font-size: 0.74rem; width: 100%;", "{e}" }
            }
        }
    }
}

/// The message action a reader button dispatches. Carried into the async task so
/// each `onclick` stays a plain `Fn` (no capture moved into the future).
#[derive(Clone, PartialEq)]
enum MailActionKind {
    MarkRead(bool),
    Flag(bool),
    Move(String),
    AddLabel(String),
    RemoveLabel(String),
    Archive,
    Delete,
    DeletePermanently,
}

/// The sanitized message body plus a per-message "Load remote images" toggle.
/// Default OFF: `render_email_body` blocks every remote fetch (tracking-pixel
/// defense). Flipping the toggle re-renders allowing http(s) `<img src>` only,
/// on this one message, as an explicit user action. Its own `#[component]` so
/// the toggle owns a Copy signal that re-renders just this body.
#[component]
fn MailBodyView(id: String, body: String) -> Element {
    let mut allow_remote = use_signal(|| false);
    // Recompute only when the toggle flips (body is per-message constant).
    let rendered = render_email_body(&body, allow_remote());
    let toggle_id = format!("mail-remote-{id}");

    rsx! {
        div {
            style: "margin-top: 0.7rem;",
            // The reader body reuses the journal's scoped .md-body dark theme so
            // links, lists, tables, and quotes read consistently.
            div {
                class: "md-body",
                style: "font-size: 0.9rem; overflow-wrap: anywhere;",
                dangerous_inner_html: "{rendered}",
            }
            div {
                style: "margin-top: 0.7rem;",
                button {
                    id: "{toggle_id}",
                    style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                            border-radius: 999px; padding: 0.25rem 0.7rem; font: inherit; \
                            font-size: 0.76rem; cursor: pointer;",
                    onclick: move |_| {
                        let now = allow_remote();
                        allow_remote.set(!now);
                    },
                    if allow_remote() {
                        "Hide remote images"
                    } else {
                        "Load remote images"
                    }
                }
                if !allow_remote() {
                    span {
                        style: "color: {FAINT}; font-size: 0.72rem; margin-left: 0.6rem;",
                        "Remote images are blocked to protect your privacy."
                    }
                }
            }
        }
    }
}

/// An attachment chip. Clicking fetches the bytes from the local blockstore
/// (`mail_attachment_serve`), builds a `data:` URL, and opens it in the OS
/// browser (save/preview). Unstored (oversize/pending) attachments render
/// dimmed and inert. A plain fn (`MailAttachmentChip` has no `PartialEq`).
fn mail_attachment_chip(store: ReadOnlySignal<Store>, att: MailAttachmentChip) -> Element {
    let mut err = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);
    let att_id = att.id.clone();
    let size_label = human_size(att.size);
    let stored = att.stored;

    let open = move |_| {
        if busy() || !stored {
            return;
        }
        let store = store();
        let att_id = att_id.clone();
        busy.set(true);
        err.set(None);
        spawn(async move {
            match store.mail_attachment_serve(&att_id).await {
                Ok(Some(serve)) => match serve.data {
                    Some(bytes) => {
                        // Bytes come from OUR local blockstore, base64 into a
                        // data: URL, then a hidden in-webview anchor with a
                        // `download` attribute is clicked to save/open it — no
                        // external `open` crate, no network request. base64 and
                        // the (sanitized) mime hold no JS-string metacharacters;
                        // the filename is JSON-escaped for the same reason.
                        use base64::Engine as _;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let data_url = format!("data:{};base64,{}", serve.mime, b64);
                        let fname = serde_json::to_string(&serve.filename)
                            .unwrap_or_else(|_| "\"attachment\"".to_string());
                        let script = format!(
                            "const a=document.createElement('a');\
                             a.href='{data_url}';a.download={fname};\
                             document.body.appendChild(a);a.click();a.remove();"
                        );
                        dioxus::document::eval(&script);
                    }
                    None => err.set(Some("not downloaded yet".into())),
                },
                Ok(None) => err.set(Some("attachment missing".into())),
                Err(e) => err.set(Some(format!("{e:#}"))),
            }
            busy.set(false);
        });
    };

    let opacity = if stored { "1" } else { "0.5" };
    let cursor = if stored { "pointer" } else { "default" };

    rsx! {
        div {
            style: "display: inline-flex; flex-direction: column; gap: 0.15rem;",
            button {
                id: "mail-attachment-{att.id}",
                disabled: !stored || busy(),
                style: "display: inline-flex; align-items: center; gap: 0.4rem; \
                        background: {BG}; border: 1px solid {EDGE}; color: {INK}; \
                        border-radius: 8px; padding: 0.35rem 0.6rem; font: inherit; \
                        font-size: 0.8rem; cursor: {cursor}; opacity: {opacity}; max-width: 260px;",
                title: if stored { "Open / save this attachment" } else { "Not downloaded yet" },
                onclick: open,
                span { "📎" }
                span {
                    style: "overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                    "{att.filename}"
                }
                span { style: "color: {FAINT}; flex: none;", "{size_label}" }
            }
            if let Some(e) = err() {
                span { style: "color: #e07a5f; font-size: 0.72rem;", "{e}" }
            }
        }
    }
}

/// How far a queued send has gotten, for the compose status line.
#[derive(Clone, PartialEq)]
enum SendState {
    Idle,
    /// Enqueued; the job id we're tracking through the outbox.
    Queued(String),
    Sent,
    Failed(String),
}

/// The compose overlay: From (a picker over the enabled mail accounts), To, Cc,
/// Subject, Body, and Send / Cancel. Sending enqueues an `OutgoingEmail` on the
/// durable outbox (`mail_send_enqueue`) and then polls that job to turn "Queued"
/// into "Sent"/"Failed" as the driver flushes it. Plaintext body this slice.
#[component]
fn ComposeWindow(store: ReadOnlySignal<Store>, compose: Signal<Option<ComposeSeed>>) -> Element {
    // Snapshot the seed once (the overlay owns its own field state from here).
    let seed = compose().unwrap_or_default();

    let mut from_id = use_signal(|| seed.account_id.clone());
    let mut to = use_signal(|| seed.to.clone());
    let mut cc = use_signal(|| seed.cc.clone());
    let mut subject = use_signal(|| seed.subject.clone());
    let mut body = use_signal(|| seed.body.clone());
    let mut error = use_signal(|| Option::<String>::None);
    let mut state = use_signal(|| SendState::Idle);
    // The reply threading headers ride alongside; they never show in the UI.
    let in_reply_to = seed.in_reply_to.clone();
    let references = seed.references.clone();

    // Enabled accounts are the From choices (address + id).
    let accounts = use_resource(move || {
        let store = store();
        async move {
            store
                .mail_accounts_admin_list()
                .await
                .map(|list| list.into_iter().filter(|a| a.enabled).collect::<Vec<_>>())
                .map_err(|e| format!("{e:#}"))
        }
    });
    let account_list: Vec<MailAccountAdminView> = match accounts() {
        Some(Ok(list)) => list,
        _ => Vec::new(),
    };
    // Default the From to the first enabled account when the seed didn't set one
    // (or set one that isn't enabled/present).
    let from_valid = account_list.iter().any(|a| a.id == from_id());
    if !from_valid {
        if let Some(first) = account_list.first() {
            from_id.set(first.id.clone());
        }
    }
    let sending = matches!(state(), SendState::Queued(_));
    // Precomputed (rsx format strings can't hold an `if` expression).
    let send_opacity = if sending { "0.6" } else { "1" };

    let submit = move |_| {
        if sending {
            return;
        }
        error.set(None);
        let account_id = from_id();
        if account_id.is_empty() {
            error.set(Some("Pick which account to send from.".into()));
            return;
        }
        // Resolve the From address from the chosen account.
        let from_address = accounts()
            .and_then(|r| r.ok())
            .and_then(|list| list.into_iter().find(|a| a.id == account_id))
            .map(|a| a.address);
        let Some(from_address) = from_address else {
            error.set(Some("That account is no longer available.".into()));
            return;
        };
        let to_list = parse_recipients(&to());
        let cc_list = parse_recipients(&cc());
        if to_list.is_empty() && cc_list.is_empty() {
            error.set(Some("Add at least one recipient.".into()));
            return;
        }
        if body().trim().is_empty() {
            error.set(Some("The message body is empty.".into()));
            return;
        }
        let msg = OutgoingEmail {
            from_address,
            from_name: None,
            to: to_list,
            cc: cc_list,
            bcc: Vec::new(),
            subject: subject(),
            body_text: body(),
            in_reply_to: in_reply_to.clone(),
            references: references.clone(),
            drafts_mailbox_id: None,
            identity_id: None,
        };
        let store = store();
        state.set(SendState::Queued(String::new()));
        spawn(async move {
            match store.mail_send_enqueue(&account_id, msg).await {
                Ok(job_id) => {
                    state.set(SendState::Queued(job_id.clone()));
                    // Poll the job: the driver flushes on its next tick (≤ ~30s
                    // by default). Bounded so a wedged send doesn't spin forever
                    // — after the window it stays "Sending…" (still queued).
                    for _ in 0..40 {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        match store.outbox_get(&job_id).await {
                            Ok(Some(job)) => match job.status {
                                hive_shared::OutboxStatus::Done => {
                                    state.set(SendState::Sent);
                                    return;
                                }
                                hive_shared::OutboxStatus::Failed => {
                                    state.set(SendState::Failed(
                                        job.last_error.unwrap_or_else(|| "send failed".into()),
                                    ));
                                    return;
                                }
                                hive_shared::OutboxStatus::Pending => {}
                            },
                            // Row gone (pruned) — treat as sent.
                            Ok(None) => {
                                state.set(SendState::Sent);
                                return;
                            }
                            Err(_) => {}
                        }
                    }
                }
                Err(e) => state.set(SendState::Failed(format!("{e:#}"))),
            }
        });
    };

    let close = move |_| {
        let mut compose = compose;
        compose.set(None);
    };

    rsx! {
        // full-pane dimmed backdrop; clicking it closes (unless mid-send)
        div {
            id: "mail-compose-backdrop",
            style: "position: absolute; inset: 0; background: rgba(0,0,0,0.5); \
                    display: flex; align-items: center; justify-content: center; z-index: 20;",
            onclick: move |_| {
                if !sending {
                    let mut compose = compose;
                    compose.set(None);
                }
            },
            // the compose card — stop click-through so clicks inside don't close
            div {
                id: "mail-compose",
                style: "width: min(640px, 92%); max-height: 88%; overflow-y: auto; \
                        background: {PANEL}; border: 1px solid {EDGE}; border-radius: 14px; \
                        padding: 1.3rem 1.4rem 1.5rem; box-shadow: 0 18px 50px rgba(0,0,0,0.55);",
                onclick: move |e| e.stop_propagation(),

                // header
                div {
                    style: "display: flex; align-items: baseline; justify-content: space-between; \
                            margin-bottom: 0.9rem;",
                    div {
                        style: "display: flex; align-items: baseline; gap: 0.5rem;",
                        div { style: "font-size: 1.2rem; font-weight: 700;", "New message" }
                        div { style: "color: {GOLD};", "✉" }
                    }
                    button {
                        id: "mail-compose-close",
                        style: "background: none; border: none; color: {DIM}; font-size: 1.2rem; \
                                cursor: pointer; line-height: 1;",
                        title: "Close",
                        onclick: close,
                        "✕"
                    }
                }

                // From
                label {
                    style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700;",
                    "From"
                }
                if account_list.is_empty() {
                    div {
                        style: "color: {FAINT}; font-size: 0.84rem; margin-top: 0.35rem;",
                        "No enabled mail account to send from. Connect one from the ⚙ menu."
                    }
                } else {
                    select {
                        id: "compose-from",
                        style: "{text_input_style()} cursor: pointer;",
                        value: "{from_id}",
                        onchange: move |e| from_id.set(e.value()),
                        for a in account_list.iter() {
                            option { value: "{a.id}", "{a.address}" }
                        }
                    }
                }

                // To
                label {
                    style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                    "To"
                }
                input {
                    id: "compose-to",
                    style: "{text_input_style()}",
                    r#type: "text",
                    placeholder: "name@example.com, another@example.com",
                    value: "{to}",
                    oninput: move |e| to.set(e.value()),
                }

                // Cc
                label {
                    style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                    "Cc "
                    span { style: "color: {FAINT}; font-weight: 400;", "(optional)" }
                }
                input {
                    id: "compose-cc",
                    style: "{text_input_style()}",
                    r#type: "text",
                    placeholder: "optional",
                    value: "{cc}",
                    oninput: move |e| cc.set(e.value()),
                }

                // Subject
                label {
                    style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                    "Subject"
                }
                input {
                    id: "compose-subject",
                    style: "{text_input_style()}",
                    r#type: "text",
                    placeholder: "Subject",
                    value: "{subject}",
                    oninput: move |e| subject.set(e.value()),
                }

                // Body
                label {
                    style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                    "Message"
                }
                textarea {
                    id: "compose-body",
                    style: "{text_input_style()} min-height: 220px; resize: vertical; \
                            line-height: 1.5; font-family: inherit;",
                    placeholder: "Write your message…",
                    value: "{body}",
                    oninput: move |e| body.set(e.value()),
                }

                // actions + status
                div {
                    style: "display: flex; align-items: center; gap: 0.9rem; margin-top: 1rem;",
                    button {
                        id: "compose-send",
                        disabled: sending || account_list.is_empty(),
                        style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                                padding: 0.55rem 1.3rem; font-weight: 700; font-size: 0.9rem; \
                                cursor: pointer; opacity: {send_opacity};",
                        onclick: submit,
                        if sending { "Sending…" } else { "Send" }
                    }
                    button {
                        id: "compose-cancel",
                        style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                                border-radius: 8px; padding: 0.55rem 1rem; font: inherit; \
                                font-size: 0.88rem; cursor: pointer;",
                        onclick: close,
                        "Cancel"
                    }
                    {compose_status(state())}
                }
                if let Some(e) = error() {
                    div {
                        id: "compose-error",
                        style: "color: #e07a5f; font-size: 0.84rem; margin-top: 0.6rem;",
                        "{e}"
                    }
                }
            }
        }
    }
}

/// The compose status line beside the Send button, keyed off the outbox job.
fn compose_status(state: SendState) -> Element {
    match state {
        SendState::Idle => rsx! {},
        SendState::Queued(_) => rsx! {
            span { style: "color: {DIM}; font-size: 0.84rem;", "Queued — sending…" }
        },
        SendState::Sent => rsx! {
            span { style: "color: #7fb069; font-size: 0.84rem;", "Sent ✓" }
        },
        SendState::Failed(reason) => rsx! {
            span {
                id: "compose-failed",
                style: "color: #e07a5f; font-size: 0.84rem;",
                "Failed: {reason}"
            }
        },
    }
}

/// A small label/flag chip in the message list + reader. Flagged reads gold; the
/// rest read as muted pills.
fn label_chip(label: &str) -> Element {
    let (bg, fg, border) = if label == "flagged" {
        (GOLD, "#14120e", GOLD)
    } else {
        ("transparent", DIM, EDGE)
    };
    rsx! {
        span {
            style: "font-size: 0.7rem; font-weight: 600; color: {fg}; background: {bg}; \
                    border: 1px solid {border}; border-radius: 999px; padding: 0.05rem 0.45rem; \
                    white-space: nowrap;",
            if label == "flagged" { "⚑ flagged" } else { "{label}" }
        }
    }
}

/// Group the flat admin-account list by owner identity, preserving the
/// alphabetical owner-then-address order the store already returns.
fn group_accounts_by_owner(
    list: Vec<MailAccountAdminView>,
) -> Vec<(String, Vec<MailAccountAdminView>)> {
    let mut groups: Vec<(String, Vec<MailAccountAdminView>)> = Vec::new();
    for acct in list {
        match groups.last_mut() {
            Some((owner, accts)) if owner == &acct.owner => accts.push(acct),
            _ => groups.push((acct.owner.clone(), vec![acct])),
        }
    }
    groups
}

/// A folder glyph for a mailbox, keyed on its JMAP role (falling back to a name
/// sniff, then a generic folder).
fn mailbox_icon(role: Option<&str>, name: &str) -> &'static str {
    match role {
        Some("inbox") => "📥",
        Some("sent") => "📤",
        Some("drafts") => "📝",
        Some("trash") => "🗑",
        Some("junk") | Some("spam") => "🚫",
        Some("archive") => "🗄",
        _ => match name.to_ascii_lowercase().as_str() {
            "inbox" => "📥",
            "sent" => "📤",
            "drafts" => "📝",
            "trash" => "🗑",
            "spam" | "junk" => "🚫",
            "archive" => "🗄",
            _ => "📁",
        },
    }
}

/// The display name for a `Name <addr>` (or bare `addr`) sender: the name if
/// present, else the address' local part, else the whole address.
fn sender_display(from: &str) -> String {
    let from = from.trim();
    if let Some(idx) = from.find('<') {
        let name = from[..idx].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return name.to_string();
        }
        let addr = from[idx + 1..].trim_end_matches('>').trim();
        return addr.to_string();
    }
    from.to_string()
}

// ── compose pure helpers (unit-tested; no signals, no store) ─────────────────

/// Parse a comma/semicolon-separated recipient string into addresses. Accepts
/// `Name <email>` and bare `email`; trims, unquotes display names, and drops
/// empty/address-less entries. A duplicate address (case-insensitive) collapses
/// to its first occurrence.
fn parse_recipients(raw: &str) -> Vec<MailAddress> {
    let mut out: Vec<MailAddress> = Vec::new();
    for token in raw.split([',', ';']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (name, email) = if let Some(open) = token.find('<') {
            let name = token[..open].trim().trim_matches('"').trim();
            let email = token[open + 1..].trim_end_matches('>').trim();
            let name = if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            };
            (name, email.to_string())
        } else {
            (None, token.to_string())
        };
        if email.is_empty() {
            continue;
        }
        if out.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
            continue;
        }
        out.push(MailAddress { email, name });
    }
    out
}

/// The `Re:` subject for a reply — prefixed once, never doubled. An existing
/// `Re:`/`RE:`/`re:` (with any surrounding space) is left as-is.
fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("re:") {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {trimmed}")
    }
}

/// A display label for an address in an attribution line / recipient field:
/// `Name <email>` when a name exists, else the bare email.
fn addr_label(a: &EmailAddr) -> String {
    match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("{n} <{}>", a.email),
        None => a.email.clone(),
    }
}

/// Render an address list back to a comma-separated string for a recipient
/// input field.
fn addrs_to_field(addrs: &[EmailAddr]) -> String {
    addrs.iter().map(addr_label).collect::<Vec<_>>().join(", ")
}

/// The quoted body for a reply: an attribution line + the original body with
/// each line `> `-prefixed. Empty original → empty quote (no dangling header).
fn quote_reply_body(received_at: &str, sender: &str, body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    let quoted: String = body
        .lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("On {}, {} wrote:\n{}", received_at, sender, quoted)
}

/// The References chain for a reply: the parent's References plus its
/// Message-ID (RFC 5322 §3.6.4), de-duplicated, order preserved.
fn reply_references(meta: &MailReplyMeta) -> Vec<String> {
    let mut refs = meta.references.clone();
    if let Some(mid) = meta.message_id_hdr.as_ref().filter(|s| !s.is_empty()) {
        if !refs.iter().any(|r| r == mid) {
            refs.push(mid.clone());
        }
    }
    refs
}

/// Reply recipients: the original Reply-To if the sender set one, else the
/// original From. (Reply, not reply-all — just the one party.)
fn reply_to_recipients(meta: &MailReplyMeta) -> Vec<EmailAddr> {
    if !meta.reply_to.is_empty() {
        meta.reply_to.clone()
    } else {
        meta.from.clone()
    }
}

/// Reply-all recipients: (To = original sender + original To) and
/// (Cc = original Cc), both with the replying account's own address removed so
/// we don't reply to ourselves. Returns `(to, cc)`.
fn reply_all_recipients(meta: &MailReplyMeta) -> (Vec<EmailAddr>, Vec<EmailAddr>) {
    let self_addr = meta.account_address.to_ascii_lowercase();
    let is_self = |a: &EmailAddr| a.email.eq_ignore_ascii_case(&self_addr);

    let mut to: Vec<EmailAddr> = Vec::new();
    let push_unique = |list: &mut Vec<EmailAddr>, a: &EmailAddr| {
        if a.email.is_empty() || is_self(a) {
            return;
        }
        if !list.iter().any(|x| x.email.eq_ignore_ascii_case(&a.email)) {
            list.push(a.clone());
        }
    };
    for a in reply_to_recipients(meta).iter().chain(meta.to.iter()) {
        push_unique(&mut to, a);
    }
    let mut cc: Vec<EmailAddr> = Vec::new();
    for a in &meta.cc {
        // Don't duplicate a To recipient down into Cc.
        if to.iter().any(|x| x.email.eq_ignore_ascii_case(&a.email)) {
            continue;
        }
        push_unique(&mut cc, a);
    }
    (to, cc)
}

/// A short, human date for the message list. If it is today, show HH:MM; if this
/// year, show `Mon DD`; else `YYYY-MM-DD`. Parses the `now_iso` shape; falls
/// back to the raw string's date part.
fn short_relative(iso: &str) -> String {
    if iso.len() < 10 {
        return iso.to_string();
    }
    let today = today_ymd();
    if iso.len() >= 16 && iso[..10] == today[..today.len().min(10)] {
        return iso[11..16].to_string();
    }
    let year = &iso[..4];
    if today.len() >= 4 && year == &today[..4] {
        // "MM-DD" → "Mon DD"
        if let (Ok(m), Ok(d)) = (iso[5..7].parse::<usize>(), iso[8..10].parse::<u32>()) {
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            if (1..=12).contains(&m) {
                return format!("{} {}", MONTHS[m - 1], d);
            }
        }
    }
    iso[..10].to_string()
}

/// Human-readable byte size for an attachment chip.
fn human_size(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", b / (1024.0 * 1024.0 * 1024.0))
    }
}

/// The Slice-A account manager — connect/enable/resync/delete mailboxes. Reached
/// from the reader's ⚙; a Back button returns to the three-pane reader. This is
/// the original accounts-only pane, kept whole and reachable.
#[component]
fn MailAccountsManager(
    store: ReadOnlySignal<Store>,
    refresh: Signal<u32>,
    managing: Signal<bool>,
) -> Element {
    rsx! {
        div {
            id: "mail-pane",
            style: "max-width: 720px; margin: 0 auto; padding: 2.2rem 1.4rem 4rem;",

            // back to the reader
            button {
                id: "mail-accounts-back",
                style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                        border-radius: 8px; padding: 0.35rem 0.8rem; font: inherit; \
                        font-size: 0.82rem; cursor: pointer; margin-bottom: 1rem;",
                onclick: move |_| managing.set(false),
                "← Back to mail"
            }

            // header
            div {
                style: "display: flex; align-items: baseline; gap: 0.6rem;",
                div { style: "font-size: 1.5rem; font-weight: 700;", "Mail accounts" }
                div { style: "font-size: 1.6rem; color: {GOLD};", "✉" }
            }
            div {
                style: "color: {DIM}; font-size: 0.9rem; line-height: 1.6; margin-top: 0.4rem;",
                "Connect a JMAP mail server for each identity. Hive syncs every mailbox in the \
                 background and weaves messages into the same searchable memory as your journal."
            }

            // ── the reusable add-form + connected-list panel ─────────────────
            MailAccountsPanel { store, refresh }

            // quiet note about what's still to come
            div {
                style: "margin-top: 2rem; padding-top: 1rem; border-top: 1px solid {EDGE}; \
                        color: {FAINT}; font-size: 0.82rem; line-height: 1.6;",
                "Composing and replying — sending as any of your identities — arrive in the next \
                 update. Reading, folders, threads, labels, and search are live now."
            }
        }
    }
}

/// The reusable mail-accounts panel: the add-account form plus the list of
/// connected accounts (with their toggle / resync / delete controls). Mounted
/// BOTH in the Mail gear view (`MailAccountsManager`) and in the Settings →
/// Accounts card, so the add flow lives in exactly one place. Preserves every
/// existing element id (`mail-account-add`, `mail-add-*`, `mail-account-list`,
/// row toggle/resync/delete). Account ops are immediate (their own store
/// calls), independent of Settings' Save.
#[component]
fn MailAccountsPanel(store: ReadOnlySignal<Store>, refresh: Signal<u32>) -> Element {
    let accounts = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            store
                .mail_accounts_admin_list()
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    rsx! {
        // ── add-account form ─────────────────────────────────────────────────
        MailAddAccount { store, refresh }

        // ── connected accounts ───────────────────────────────────────────────
        div {
            style: "margin-top: 1.8rem;",
            div {
                style: "font-size: 0.78rem; font-weight: 700; letter-spacing: 0.08em; \
                        text-transform: uppercase; color: {DIM}; margin-bottom: 0.6rem;",
                "Connected"
            }
            div {
                id: "mail-account-list",
                match accounts() {
                    None => muted("loading accounts…"),
                    Some(Err(e)) => muted(&format!("accounts unavailable: {e}")),
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div {
                            style: "color: {FAINT}; font-size: 0.9rem; padding: 1.2rem 0;",
                            "No mailboxes yet. Add one above to start syncing."
                        }
                    },
                    Some(Ok(list)) => rsx! {
                        for acct in list.into_iter() {
                            {mail_account_row(store, acct, refresh)}
                        }
                    },
                }
            }
        }
    }
}

/// The add-account form. On submit, `mail_account_create` vaults the raw
/// password itself (cc_cred_put) and creates the account with an empty
/// jmap_account_id; the driver's first pass discovers the real one.
#[component]
fn MailAddAccount(store: ReadOnlySignal<Store>, refresh: Signal<u32>) -> Element {
    let mut owner = use_signal(String::new);
    let mut address = use_signal(String::new);
    let mut url = use_signal(String::new);
    let mut username = use_signal(String::new);
    let mut secret = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut ok = use_signal(|| false);
    let mut busy = use_signal(|| false);

    // Every identity that can own a mailbox (people_list — pia/maggie included).
    let identities = use_resource(move || {
        let store = store();
        async move { store.people_list().await.map_err(|e| format!("{e:#}")) }
    });
    let people: Vec<Person> = match identities() {
        Some(Ok(list)) => list,
        _ => Vec::new(),
    };
    // Precomputed (rsx format strings can't hold an `if` expression).
    let submit_opacity = if busy() { "0.6" } else { "1" };

    let submit = move |_| {
        if busy() {
            return;
        }
        let owner_v = owner().trim().to_string();
        let address_v = address().trim().to_string();
        let url_v = url().trim().to_string();
        let username_v = username().trim().to_string();
        let secret_v = secret(); // NOT trimmed — a password may hold spaces
                                 // Minimal client-side validation; the store re-checks and dedupes.
        if owner_v.is_empty() {
            error.set(Some("Pick which identity owns this mailbox.".into()));
            return;
        }
        if address_v.is_empty() || url_v.is_empty() || secret_v.is_empty() {
            error.set(Some(
                "Address, server URL, and password are all required.".into(),
            ));
            return;
        }
        let store = store();
        let mut refresh = refresh;
        busy.set(true);
        ok.set(false);
        error.set(None);
        spawn(async move {
            // `secret` is the RAW password: mail_account_create encrypts it into
            // the vault (cc_cred_put) and stores only the cred id on the account.
            // jmap_account_id is left empty — the driver discovers it.
            let uname = if username_v.is_empty() {
                None
            } else {
                Some(username_v.as_str())
            };
            let result = store
                .mail_account_create(&owner_v, &address_v, &url_v, uname, "", &secret_v)
                .await;
            busy.set(false);
            match result {
                Ok(_) => {
                    // Clear the sensitive field first, then the rest.
                    secret.set(String::new());
                    address.set(String::new());
                    url.set(String::new());
                    username.set(String::new());
                    ok.set(true);
                    refresh += 1;
                }
                Err(e) => error.set(Some(format!("{e:#}"))),
            }
        });
    };

    rsx! {
        div {
            id: "mail-account-add",
            style: "margin-top: 1.4rem; background: {PANEL}; border: 1px solid {EDGE}; \
                    border-radius: 12px; padding: 1.1rem 1.2rem;",
            div {
                style: "font-size: 1rem; font-weight: 700; margin-bottom: 0.2rem;",
                "Add a mailbox"
            }
            div {
                style: "color: {FAINT}; font-size: 0.82rem; margin-bottom: 0.9rem;",
                "Its password is encrypted in your local vault — never stored or logged in the clear."
            }

            // owner identity
            label {
                style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700;",
                "Owner identity"
            }
            select {
                id: "mail-add-owner",
                style: "{text_input_style()} cursor: pointer;",
                value: "{owner}",
                onchange: move |e| owner.set(e.value()),
                option { value: "", "Choose an identity…" }
                for p in people.iter() {
                    option {
                        value: "{p.slug}",
                        "{identity_option_label(p)}"
                    }
                }
            }

            // address
            label {
                style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                "Email address"
            }
            input {
                id: "mail-add-address",
                style: "{text_input_style()}",
                r#type: "email",
                placeholder: "you@example.com",
                value: "{address}",
                oninput: move |e| address.set(e.value()),
            }

            // JMAP URL
            label {
                style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                "JMAP server URL"
            }
            input {
                id: "mail-add-url",
                style: "{text_input_style()}",
                r#type: "url",
                placeholder: "https://mail.example.com",
                value: "{url}",
                oninput: move |e| url.set(e.value()),
            }
            div {
                style: "color: {FAINT}; font-size: 0.76rem; margin-top: 0.3rem;",
                "Your provider's JMAP session URL (often the base host, or its /.well-known/jmap)."
            }

            // username (optional)
            label {
                style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                "Username "
                span { style: "color: {FAINT}; font-weight: 400;", "(optional — defaults to the address)" }
            }
            input {
                id: "mail-add-username",
                style: "{text_input_style()}",
                r#type: "text",
                placeholder: "usually your email address",
                value: "{username}",
                oninput: move |e| username.set(e.value()),
            }

            // password (masked)
            label {
                style: "display: block; color: {DIM}; font-size: 0.82rem; font-weight: 700; margin-top: 0.7rem;",
                "Password"
            }
            input {
                id: "mail-add-secret",
                style: "{text_input_style()}",
                r#type: "password",
                placeholder: "app password or account password",
                value: "{secret}",
                oninput: move |e| secret.set(e.value()),
            }

            // submit + status
            div {
                style: "display: flex; align-items: center; gap: 0.8rem; margin-top: 1rem;",
                button {
                    id: "mail-add-submit",
                    disabled: busy(),
                    style: "background: {GOLD}; color: #14120e; border: none; border-radius: 8px; \
                            padding: 0.55rem 1.1rem; font-weight: 700; font-size: 0.88rem; \
                            cursor: pointer; opacity: {submit_opacity};",
                    onclick: submit,
                    if busy() { "Connecting…" } else { "Add mailbox" }
                }
                if ok() {
                    span {
                        style: "color: #7fb069; font-size: 0.84rem;",
                        "Mailbox added — syncing will begin shortly."
                    }
                }
            }
            if let Some(e) = error() {
                div {
                    id: "mail-add-error",
                    style: "color: #e07a5f; font-size: 0.84rem; margin-top: 0.6rem;",
                    "{e}"
                }
            }
        }
    }
}

/// One connected account: address, owner, status/last-sync + any error, and the
/// enabled toggle / force-resync / delete controls (delete is two-step). A
/// plain fn, not a `#[component]`: `MailAccountAdminView` has no `PartialEq`, so
/// it can't be a memoized prop.
fn mail_account_row(
    store: ReadOnlySignal<Store>,
    acct: MailAccountAdminView,
    refresh: Signal<u32>,
) -> Element {
    let mut armed = use_signal(|| None as ArmedDelete);
    let mut row_err = use_signal(|| Option::<String>::None);
    // A "Sync now" pass in flight — drives the button's label + disables it.
    let mut syncing = use_signal(|| false);
    // Inline edit form: open flag + prefilled connection fields (password blank
    // = keep current) + a save-in-flight flag.
    let mut editing = use_signal(|| false);
    let mut edit_busy = use_signal(|| false);
    let mut ed_address = use_signal({
        let v = acct.address.clone();
        move || v
    });
    let mut ed_url = use_signal({
        let v = acct.jmap_url.clone();
        move || v
    });
    let mut ed_username = use_signal({
        let v = acct.jmap_username.clone().unwrap_or_default();
        move || v
    });
    let mut ed_secret = use_signal(String::new);
    let id = acct.id.clone();

    let toggle = {
        let id = id.clone();
        let enabled_now = acct.enabled;
        move |_| {
            let store = store();
            let id = id.clone();
            let mut refresh = refresh;
            spawn(async move {
                match store.mail_account_set_enabled(&id, !enabled_now).await {
                    Ok(_) => refresh += 1,
                    Err(e) => row_err.set(Some(format!("{e:#}"))),
                }
            });
        }
    };

    let resync = {
        let id = id.clone();
        move |_| {
            let store = store();
            let id = id.clone();
            let mut refresh = refresh;
            spawn(async move {
                match store.mail_account_force_resync(&id).await {
                    Ok(_) => refresh += 1,
                    Err(e) => row_err.set(Some(format!("{e:#}"))),
                }
            });
        }
    };

    // Run one sync pass right now and show the outcome inline — the immediate,
    // feedback-giving force (Resync resets the cursor and waits for the tick;
    // this runs a pass and returns the exact error on failure).
    let sync_now = {
        let id = id.clone();
        move |_| {
            if syncing() {
                return;
            }
            let store = store();
            let id = id.clone();
            let mut refresh = refresh;
            syncing.set(true);
            row_err.set(None);
            spawn(async move {
                match store.mail_account_sync_now(&id).await {
                    Ok(()) => refresh += 1,
                    Err(e) => row_err.set(Some(format!("{e:#}"))),
                }
                syncing.set(false);
            });
        }
    };

    // Save the inline edit: update connection details (+ optional new password),
    // which re-syncs cleanly against the possibly-new server.
    let save = {
        let id = id.clone();
        move |_| {
            if edit_busy() {
                return;
            }
            let store = store();
            let id = id.clone();
            let mut refresh = refresh;
            let username = ed_username();
            let secret = ed_secret();
            let edit = MailAccountEdit {
                address: ed_address(),
                jmap_url: ed_url(),
                jmap_username: if username.trim().is_empty() {
                    None
                } else {
                    Some(username)
                },
                new_password: if secret.is_empty() {
                    None
                } else {
                    Some(secret)
                },
            };
            edit_busy.set(true);
            row_err.set(None);
            spawn(async move {
                match store.mail_account_update(&id, edit).await {
                    Ok(_) => {
                        ed_secret.set(String::new());
                        editing.set(false);
                        refresh += 1;
                    }
                    Err(e) => row_err.set(Some(format!("{e:#}"))),
                }
                edit_busy.set(false);
            });
        }
    };

    let delete = {
        let id = id.clone();
        move |_| {
            let store = store();
            let id = id.clone();
            let mut refresh = refresh;
            spawn(async move {
                match store.mail_account_delete(&id).await {
                    Ok(_) => refresh += 1,
                    Err(e) => row_err.set(Some(format!("{e:#}"))),
                }
            });
        }
    };

    rsx! {
        div {
            id: "mail-account-row-{acct.id}",
            style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.9rem 1rem; margin-bottom: 0.6rem;",
            div {
                style: "display: flex; align-items: center; gap: 0.8rem;",
                // address + owner + status
                div {
                    style: "flex: 1; min-width: 0;",
                    div {
                        style: "font-weight: 700; font-size: 0.96rem; overflow: hidden; \
                                text-overflow: ellipsis; white-space: nowrap;",
                        "{acct.address}"
                    }
                    div {
                        style: "color: {DIM}; font-size: 0.8rem; margin-top: 0.15rem;",
                        "owned by "
                        span { style: "color: {GOLD};", "{acct.owner}" }
                        " · "
                        span { "{account_status_line(&acct)}" }
                    }
                }
                // enabled toggle
                button {
                    id: "mail-account-toggle-{acct.id}",
                    style: mail_pill_style(acct.enabled),
                    onclick: toggle,
                    if acct.enabled { "Enabled" } else { "Disabled" }
                }
                // sync now — run one pass immediately, outcome shown inline
                button {
                    id: "mail-account-syncnow-{acct.id}",
                    disabled: syncing(),
                    style: "background: none; border: 1px solid {GOLD}; color: {GOLD}; \
                            border-radius: 999px; padding: 0.35rem 0.8rem; font: inherit; \
                            font-size: 0.8rem; font-weight: 700; cursor: pointer;",
                    title: "Sync this mailbox right now and show the result",
                    onclick: sync_now,
                    if syncing() { "Syncing…" } else { "Sync now" }
                }
                // force-resync (full re-check from scratch)
                button {
                    id: "mail-account-resync-{acct.id}",
                    style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                            border-radius: 999px; padding: 0.35rem 0.8rem; font: inherit; \
                            font-size: 0.8rem; cursor: pointer;",
                    title: "Re-check the whole mailbox against the server",
                    onclick: resync,
                    "Resync"
                }
                // edit connection details (toggles the inline form below)
                button {
                    id: "mail-account-edit-{acct.id}",
                    style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                            border-radius: 999px; padding: 0.35rem 0.8rem; font: inherit; \
                            font-size: 0.8rem; cursor: pointer;",
                    title: "Edit this mailbox's address, server URL, username, or password",
                    onclick: move |_| {
                        let open = !editing();
                        editing.set(open);
                        row_err.set(None);
                    },
                    if editing() { "Close" } else { "Edit" }
                }
                // delete (two-step)
                if armed().as_deref() == Some(acct.id.as_str()) {
                    button {
                        id: "mail-account-delete-{acct.id}",
                        style: "background: #e07a5f; color: #14120e; border: none; border-radius: 999px; \
                                padding: 0.35rem 0.8rem; font-weight: 700; font-size: 0.8rem; cursor: pointer;",
                        onclick: delete,
                        "Really delete"
                    }
                    button {
                        style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                                border-radius: 999px; padding: 0.35rem 0.7rem; font: inherit; \
                                font-size: 0.8rem; cursor: pointer;",
                        onclick: move |_| armed.set(None),
                        "Cancel"
                    }
                } else {
                    button {
                        style: "background: none; border: 1px solid #e07a5f; color: #e07a5f; \
                                border-radius: 999px; padding: 0.35rem 0.8rem; font: inherit; \
                                font-size: 0.8rem; cursor: pointer;",
                        title: "Disconnect this mailbox and delete its local copy",
                        onclick: {
                            let rid = acct.id.clone();
                            move |_| armed.set(Some(rid.clone()))
                        },
                        "Delete"
                    }
                }
            }
            // inline edit form (address / JMAP URL / username / new password)
            if editing() {
                div {
                    id: "mail-account-editform-{acct.id}",
                    style: "margin-top: 0.7rem; border-top: 1px solid {EDGE}; padding-top: 0.7rem; \
                            display: flex; flex-direction: column; gap: 0.45rem;",
                    label {
                        style: "color: {DIM}; font-size: 0.78rem; font-weight: 700;",
                        "Email address"
                    }
                    input {
                        id: "mail-edit-address-{acct.id}",
                        style: "{text_input_style()}",
                        r#type: "email",
                        value: "{ed_address}",
                        oninput: move |e| ed_address.set(e.value()),
                    }
                    label {
                        style: "color: {DIM}; font-size: 0.78rem; font-weight: 700; margin-top: 0.3rem;",
                        "JMAP server URL"
                    }
                    input {
                        id: "mail-edit-url-{acct.id}",
                        style: "{text_input_style()}",
                        value: "{ed_url}",
                        oninput: move |e| ed_url.set(e.value()),
                    }
                    label {
                        style: "color: {DIM}; font-size: 0.78rem; font-weight: 700; margin-top: 0.3rem;",
                        "Username (optional)"
                    }
                    input {
                        id: "mail-edit-username-{acct.id}",
                        style: "{text_input_style()}",
                        placeholder: "defaults to the address",
                        value: "{ed_username}",
                        oninput: move |e| ed_username.set(e.value()),
                    }
                    label {
                        style: "color: {DIM}; font-size: 0.78rem; font-weight: 700; margin-top: 0.3rem;",
                        "New password"
                    }
                    input {
                        id: "mail-edit-secret-{acct.id}",
                        style: "{text_input_style()}",
                        r#type: "password",
                        placeholder: "leave blank to keep the current password",
                        value: "{ed_secret}",
                        oninput: move |e| ed_secret.set(e.value()),
                    }
                    div {
                        style: "display: flex; gap: 0.5rem; margin-top: 0.3rem;",
                        button {
                            id: "mail-edit-save-{acct.id}",
                            disabled: edit_busy(),
                            style: "background: {GOLD}; color: #14120e; border: none; border-radius: 999px; \
                                    padding: 0.4rem 0.9rem; font: inherit; font-weight: 700; \
                                    font-size: 0.82rem; cursor: pointer;",
                            onclick: save,
                            if edit_busy() { "Saving…" } else { "Save changes" }
                        }
                        button {
                            id: "mail-edit-cancel-{acct.id}",
                            style: "background: none; border: 1px solid {EDGE}; color: {DIM}; \
                                    border-radius: 999px; padding: 0.4rem 0.9rem; font: inherit; \
                                    font-size: 0.82rem; cursor: pointer;",
                            onclick: move |_| {
                                editing.set(false);
                                ed_secret.set(String::new());
                                row_err.set(None);
                            },
                            "Cancel"
                        }
                    }
                    div {
                        style: "color: {FAINT}; font-size: 0.72rem;",
                        "Changing the server, username, or address re-syncs this mailbox from scratch."
                    }
                }
            }
            // last error, whenever the last attempt failed (independent of the
            // attempt counter, which a Resync resets — the reason must not hide
            // while the status still reads "failed").
            if let Some(err) = acct
                .last_error
                .as_ref()
                .filter(|_| acct.last_status.as_deref() == Some("error") || !acct.enabled)
            {
                div {
                    style: "color: #e07a5f; font-size: 0.78rem; margin-top: 0.5rem; overflow-wrap: anywhere;",
                    "Last error: {err}"
                }
            }
            if let Some(e) = row_err() {
                div {
                    style: "color: #e07a5f; font-size: 0.78rem; margin-top: 0.5rem;",
                    "{e}"
                }
            }
        }
    }
}

/// Dropdown label for an identity option: name, plus a kind hint for AI
/// identities so pia/maggie read distinctly from human owners.
fn identity_option_label(p: &Person) -> String {
    if matches!(p.kind, ActorKind::Ai) {
        format!("{} (AI)", p.name)
    } else {
        p.name.clone()
    }
}

/// The compact status line under an address: backfill phase + last outcome +
/// last-synced time, or the disabled/never-synced state. No secrets.
fn account_status_line(a: &MailAccountAdminView) -> String {
    if !a.enabled {
        return "disabled".to_string();
    }
    let phase = match a.backfill_status.as_str() {
        "complete" => "up to date",
        "in_progress" => "backfilling…",
        _ => "waiting to sync",
    };
    match (a.last_status.as_deref(), a.last_synced_at.as_deref()) {
        (Some("error"), _) => format!("{phase} · last attempt failed"),
        (_, Some(ts)) => format!("{phase} · synced {}", short_time(ts)),
        _ => phase.to_string(),
    }
}

/// The enabled/disabled pill style (gold when enabled, muted when off).
fn mail_pill_style(enabled: bool) -> String {
    if enabled {
        format!(
            "background: {GOLD}; color: #14120e; border: none; border-radius: 999px; \
             padding: 0.35rem 0.8rem; font-weight: 700; font-size: 0.8rem; cursor: pointer;"
        )
    } else {
        format!(
            "background: none; border: 1px solid {FAINT}; color: {DIM}; border-radius: 999px; \
             padding: 0.35rem 0.8rem; font: inherit; font-size: 0.8rem; cursor: pointer;"
        )
    }
}

/// ISO-8601 → a short `YYYY-MM-DD HH:MM` for the status line (the timestamps
/// are always the `now_iso` shape). Falls back to the raw string if it is
/// shorter than expected.
fn short_time(iso: &str) -> String {
    if iso.len() >= 16 {
        format!("{} {}", &iso[..10], &iso[11..16])
    } else {
        iso.to_string()
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

/// Which calendar layout is showing. A single cursor date drives all three;
/// the ‹/› buttons step by this view's unit (day / week / month).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CalView {
    Day,
    Week,
    Month,
}

/// The Calendar section: an Apple-Calendar-style Day / Week / Month switcher
/// over the same fold-safe events, an undated list, and a minimal create form.
/// A single cursor date (y, m, d) drives every view; selecting an event (or
/// creating one) opens the shared EntityDetail. `refresh` re-pulls after a
/// create/edit/delete.
#[component]
fn CalendarPane(
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    refresh: Signal<u32>,
) -> Element {
    // The cursor date, initialized to today (falls back to a fixed epoch only
    // if the clock string is somehow unparseable — it never is). One signal
    // per component, driving all three views; ‹/› step it by the view's unit.
    let (ty0, tm0, td0) = parse_ymd(&today_ymd()).unwrap_or((2026, 7, 11));
    let mut cur_y = use_signal(|| ty0);
    let mut cur_m = use_signal(|| tm0);
    let mut cur_d = use_signal(|| td0);
    let view = use_signal(|| CalView::Month);
    // Create-form state; a day/hour click prefills the date and the form sits
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

    let y = cur_y();
    let m = cur_m();
    let d = cur_d();
    let cur_view = view();
    let today = today_ymd();

    // The contextual header label depends on the active view's unit.
    let label = match cur_view {
        CalView::Day => day_label(y, m, d),
        CalView::Week => week_range_label(y, m, d),
        CalView::Month => month_label(y, m),
    };

    rsx! {
        div {
            id: "calendar-pane",
            style: "max-width: 960px; margin: 0 auto; padding: 1.6rem 1.2rem 3rem;",
            {pane_header("Calendar", "Your events by day, week, or month — the same happenings that \
                                     emerge from your journal, plus any you add here. Click a slot \
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

            // nav + contextual label + Day/Week/Month switcher
            div {
                style: "display: flex; align-items: center; gap: 0.6rem; margin-bottom: 0.8rem; \
                        flex-wrap: wrap;",
                button {
                    id: "cal-prev",
                    style: "{cal_nav_btn_style()}",
                    onclick: move |_| {
                        step_cursor(cur_view, false, &mut cur_y, &mut cur_m, &mut cur_d);
                    },
                    "‹"
                }
                button {
                    id: "cal-today",
                    style: "{cal_nav_btn_style()} padding-left: 0.9rem; padding-right: 0.9rem;",
                    onclick: move |_| {
                        if let Some((ty, tm, td)) = parse_ymd(&today_ymd()) {
                            cur_y.set(ty);
                            cur_m.set(tm);
                            cur_d.set(td);
                        }
                    },
                    "Today"
                }
                button {
                    id: "cal-next",
                    style: "{cal_nav_btn_style()}",
                    onclick: move |_| {
                        step_cursor(cur_view, true, &mut cur_y, &mut cur_m, &mut cur_d);
                    },
                    "›"
                }
                div {
                    id: "cal-label",
                    style: "font-size: 1.15rem; font-weight: 700; margin-left: 0.4rem;",
                    "{label}"
                }
                // segmented control, pushed to the right
                div {
                    style: "margin-left: auto; display: inline-flex; background: {PANEL}; \
                            border: 1px solid {EDGE}; border-radius: 8px; overflow: hidden;",
                    {cal_view_seg("cal-view-day", "Day", CalView::Day, cur_view, view)}
                    {cal_view_seg("cal-view-week", "Week", CalView::Week, cur_view, view)}
                    {cal_view_seg("cal-view-month", "Month", CalView::Month, cur_view, view)}
                }
            }

            match events() {
                None => muted("loading events…"),
                Some(Err(e)) => muted(&format!("events unavailable: {e}")),
                Some(Ok(list)) => {
                    let placed = placed_events(&list);
                    let undated = undated_events(&list);
                    match cur_view {
                        CalView::Month => rsx! {
                            {month_grid_view(y, m, &today, &placed, selected, new_date)}
                            {undated_view(&undated, selected)}
                        },
                        CalView::Week => rsx! {
                            {week_view(y, m, d, &today, &placed, selected, new_date)}
                            {undated_view(&undated, selected)}
                        },
                        CalView::Day => rsx! {
                            {day_view(y, m, d, &today, &placed, selected, new_date)}
                            {undated_view(&undated, selected)}
                        },
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
    let color = event_color(e);
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
                    border: 1px solid {EDGE}; border-left: 3px solid {color}; border-radius: 6px; \
                    color: {INK}; font: inherit; \
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

/// Step the cursor date by the active view's unit: Day → ±1 day (`step_day`,
/// wrapping months/years), Week → ±7 days, Month → ±1 month (`step_month`,
/// keeping the day-of-month but clamping to the target month's length so
/// e.g. Jan 31 → Feb 28). Mutates the three cursor signals in place.
fn step_cursor(
    view: CalView,
    forward: bool,
    cur_y: &mut Signal<i32>,
    cur_m: &mut Signal<u32>,
    cur_d: &mut Signal<u32>,
) {
    let (y, m, d) = (
        cur_y.peek().to_owned(),
        cur_m.peek().to_owned(),
        cur_d.peek().to_owned(),
    );
    let (ny, nm, nd) = match view {
        CalView::Day => step_day(y, m, d, forward),
        CalView::Week => {
            let mut cur = (y, m, d);
            for _ in 0..7 {
                cur = step_day(cur.0, cur.1, cur.2, forward);
            }
            cur
        }
        CalView::Month => {
            let (ny, nm) = step_month(y, m, forward);
            (ny, nm, d.min(days_in_month(ny, nm)))
        }
    };
    cur_y.set(ny);
    cur_m.set(nm);
    cur_d.set(nd);
}

/// One segment of the Day/Week/Month control: highlighted (gold) when it is the
/// active view, and a click switches to it. `mine` is this segment's view,
/// `current` the active one, `view` the signal to set.
fn cal_view_seg(
    id: &'static str,
    label: &'static str,
    mine: CalView,
    current: CalView,
    mut view: Signal<CalView>,
) -> Element {
    let active = mine == current;
    let (bg, fg) = if active {
        (GOLD.to_string(), "#14120e".to_string())
    } else {
        ("transparent".to_string(), INK.to_string())
    };
    rsx! {
        button {
            id: "{id}",
            style: "background: {bg}; color: {fg}; border: none; padding: 0.4rem 0.85rem; \
                    font: inherit; font-size: 0.85rem; font-weight: 700; cursor: pointer;",
            onclick: move |_| view.set(mine),
            "{label}"
        }
    }
}

/// A single day's events, ordered timed-first (by `event_time`), then untimed,
/// then by title — the same in-cell order `placed_events` uses, reused by the
/// Week columns and the Day view's all-day strip. Pulls the day out of the
/// pre-grouped placement map so it stays a simple lookup.
fn day_events(
    placed: &std::collections::HashMap<(i32, u32, u32), Vec<EventItem>>,
    year: i32,
    month: u32,
    day: u32,
) -> Vec<EventItem> {
    placed.get(&(year, month, day)).cloned().unwrap_or_default()
}

// ── week view (seven day-columns for the cursor's week) ───────────────────────

/// The Week view: seven Sunday→Saturday day-columns for the cursor's week, each
/// with a clickable header (weekday abbrev + day number, today highlighted) and
/// that day's colored event chips beneath. Horizontally scrollable if it
/// overflows; the body scrolls vertically. Plain fn (EventItem/HashMap lack the
/// PartialEq component props want).
fn week_view(
    year: i32,
    month: u32,
    day: u32,
    today: &str,
    placed: &std::collections::HashMap<(i32, u32, u32), Vec<EventItem>>,
    selected: Signal<Option<Selected>>,
    new_date: Signal<String>,
) -> Element {
    let days = week_days(year, month, day);
    let today_ymd = parse_ymd(today);
    rsx! {
        div {
            id: "cal-week",
            style: "overflow-x: auto;",
            div {
                style: "display: grid; grid-template-columns: repeat(7, minmax(7.5rem, 1fr)); \
                        gap: 4px; min-width: 46rem;",
                for (dy, dm, dd) in days.into_iter() {
                    {week_column(dy, dm, dd, today_ymd, placed, selected, new_date)}
                }
            }
        }
    }
}

/// One week day-column: its header (clickable → prefill create on that day) and
/// its colored event chips, sorted by time.
fn week_column(
    year: i32,
    month: u32,
    day: u32,
    today_ymd: Option<(i32, u32, u32)>,
    placed: &std::collections::HashMap<(i32, u32, u32), Vec<EventItem>>,
    selected: Signal<Option<Selected>>,
    mut new_date: Signal<String>,
) -> Element {
    let key = ymd_key(year, month, day);
    let is_today = today_ymd == Some((year, month, day));
    let wd = weekday_abbrev(weekday(year, month, day));
    let evs = day_events(placed, year, month, day);
    let border = if is_today {
        format!("2px solid {GOLD}")
    } else {
        format!("1px solid {EDGE}")
    };
    let num_color = if is_today { GOLD } else { INK };
    let date_for_click = key.clone();
    rsx! {
        div {
            id: "cal-weekcol-{key}",
            style: "background: {PANEL}; border: {border}; border-radius: 8px; \
                    padding: 0.4rem 0.4rem 0.6rem; display: flex; flex-direction: column; \
                    gap: 3px; min-height: 12rem; cursor: pointer;",
            onclick: move |_| new_date.set(date_for_click.clone()),
            div {
                style: "text-align: center; padding: 0.15rem 0 0.4rem; border-bottom: 1px solid {EDGE}; \
                        margin-bottom: 0.3rem;",
                div {
                    style: "font-size: 0.68rem; font-weight: 700; letter-spacing: 0.04em; \
                            text-transform: uppercase; color: {DIM};",
                    "{wd}"
                }
                div {
                    style: "font-size: 1.05rem; font-weight: 700; color: {num_color};",
                    "{day}"
                }
            }
            if evs.is_empty() {
                div {
                    style: "font-size: 0.7rem; color: {FAINT}; text-align: center; padding: 0.3rem 0;",
                    "—"
                }
            }
            for e in evs.iter() {
                {week_chip(e, selected)}
            }
        }
    }
}

/// One event chip in a week column: colored, time-prefixed when timed, title
/// truncated, opening the event's detail on click (stopping propagation so the
/// column's prefill click doesn't also fire).
fn week_chip(e: &EventItem, mut selected: Signal<Option<Selected>>) -> Element {
    let id = e.id.clone();
    let color = event_color(e);
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
            id: "cal-week-event-{e.id}",
            style: "display: block; width: 100%; text-align: left; background: {BG}; \
                    border: 1px solid {EDGE}; border-left: 3px solid {color}; border-radius: 6px; \
                    color: {INK}; font: inherit; font-size: 0.72rem; padding: 0.2rem 0.35rem; \
                    cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;",
            onclick: move |ev| {
                ev.stop_propagation();
                selected.set(Some(Selected::Event(id.clone())));
            },
            "{label}"
        }
    }
}

// ── day view (all-day strip + hourly timeline for the cursor day) ─────────────

/// The Day view: an all-day / untimed strip at top, then a 0–23h timeline with
/// each timed event placed in its start-hour row (stacked when several share an
/// hour). Colored blocks; clicking an empty hour prefills a create on that day
/// at that hour. Plain fn (EventItem/HashMap lack the PartialEq props want).
fn day_view(
    year: i32,
    month: u32,
    day: u32,
    today: &str,
    placed: &std::collections::HashMap<(i32, u32, u32), Vec<EventItem>>,
    selected: Signal<Option<Selected>>,
    new_date: Signal<String>,
) -> Element {
    let evs = day_events(placed, year, month, day);
    // Split into untimed (all-day strip) and timed (timeline), bucketing the
    // timed ones by their start hour. Untimed = no parseable HH:MM in `at`.
    let (timed, untimed): (Vec<EventItem>, Vec<EventItem>) = evs
        .into_iter()
        .partition(|e| e.at.as_deref().and_then(event_time).is_some());
    let is_today = parse_ymd(today) == Some((year, month, day));
    let key = ymd_key(year, month, day);
    rsx! {
        div {
            id: "cal-day",
            style: "border: 1px solid {EDGE}; border-radius: 10px; overflow: hidden;",
            // all-day / untimed strip
            div {
                id: "cal-day-allday",
                style: "display: flex; gap: 0.5rem; align-items: flex-start; padding: 0.5rem 0.6rem; \
                        background: {PANEL}; border-bottom: 1px solid {EDGE};",
                div {
                    style: "width: 3.4rem; flex-shrink: 0; font-size: 0.68rem; font-weight: 700; \
                            letter-spacing: 0.03em; text-transform: uppercase; color: {DIM}; \
                            padding-top: 0.2rem;",
                    "All-day"
                }
                div {
                    style: "flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px;",
                    if untimed.is_empty() {
                        div {
                            style: "font-size: 0.74rem; color: {FAINT}; padding: 0.15rem 0;",
                            "Nothing all-day."
                        }
                    }
                    for e in untimed.iter() {
                        {day_block(e, selected)}
                    }
                }
            }
            // hourly timeline
            for h in 0u32..24 {
                {day_hour_row(h, is_today, &key, &timed, selected, new_date)}
            }
        }
    }
}

/// One hour row of the Day timeline: the hour gutter label, and any timed events
/// whose start-hour is `h` (stacked). Clicking empty row space prefills a create
/// on this day at this hour.
fn day_hour_row(
    hour: u32,
    is_today: bool,
    key: &str,
    timed: &[EventItem],
    selected: Signal<Option<Selected>>,
    mut new_date: Signal<String>,
) -> Element {
    let here: Vec<EventItem> = timed
        .iter()
        .filter(|e| e.at.as_deref().and_then(event_hour) == Some(hour))
        .cloned()
        .collect();
    // Prefill the date with an ISO datetime at this hour so the editor lands on
    // the right day and time; a bare `YYYY-MMTHH` wouldn't round-trip, so use
    // the frozen 24-char shape event_day/event_time accept.
    let prefill = format!("{key}T{hour:02}:00:00.000Z");
    let label = format!("{hour:02}:00");
    // A faint gold left edge marks the current hour on today (current_hour()
    // already fails safe to no-marker if the clock is unreadable).
    let cur_hour = is_today && current_hour() == Some(hour);
    let row_border = if cur_hour {
        format!("border-left: 2px solid {GOLD};")
    } else {
        "border-left: 2px solid transparent;".to_string()
    };
    rsx! {
        div {
            id: "cal-day-hour-{hour}",
            style: "display: flex; gap: 0.5rem; align-items: stretch; min-height: 2.6rem; \
                    border-top: 1px solid {EDGE}; {row_border} cursor: pointer;",
            onclick: move |_| new_date.set(prefill.clone()),
            div {
                style: "width: 3.4rem; flex-shrink: 0; font-size: 0.7rem; color: {DIM}; \
                        padding: 0.25rem 0 0 0.35rem; text-align: right; box-sizing: border-box;",
                "{label}"
            }
            div {
                style: "flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px; \
                        padding: 0.2rem 0.4rem;",
                for e in here.iter() {
                    {day_block(e, selected)}
                }
            }
        }
    }
}

/// One event block in the Day view (all-day strip or a timeline hour): a colored
/// block with the title and its time, opening the detail on click.
fn day_block(e: &EventItem, mut selected: Signal<Option<Selected>>) -> Element {
    let id = e.id.clone();
    let color = event_color(e);
    let time = e.at.as_deref().and_then(event_time);
    let title = if e.title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        e.title.clone()
    };
    rsx! {
        button {
            id: "cal-day-event-{e.id}",
            style: "display: flex; align-items: baseline; gap: 0.5rem; width: 100%; text-align: left; \
                    background: {BG}; border: 1px solid {EDGE}; border-left: 4px solid {color}; \
                    border-radius: 6px; color: {INK}; font: inherit; font-size: 0.82rem; \
                    padding: 0.3rem 0.5rem; cursor: pointer; overflow: hidden;",
            onclick: move |ev| {
                ev.stop_propagation();
                selected.set(Some(Selected::Event(id.clone())));
            },
            span {
                style: "font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;",
                "{title}"
            }
            if let Some(t) = time {
                span { style: "color: {DIM}; font-size: 0.74rem; flex-shrink: 0;", "{t}" }
            }
        }
    }
}

/// The current hour (0–23) from the store's clock, for the Day view's now-marker.
/// None if the clock string is somehow unparseable.
fn current_hour() -> Option<u32> {
    let iso = hive_core::store::now_iso();
    iso.get(11..13)?.parse::<u32>().ok().filter(|h| *h <= 23)
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
    // Who "you" are, as an EXACT slug — the target of set-owner/claim/take-over.
    // Resolved once at mount (and on refresh) from identity.owner via
    // owner_binding(): the stored slug ONLY when it names a real identity, else
    // empty. Empty = no owner bound yet → the pane prompts "pick which identity
    // is you" and gates every claim/take-over control until one is set.
    let owner = use_signal(String::new);
    // Whether the owner resolution has completed at least once. Distinguishes
    // "still loading" (don't flash the prompt) from "resolved to no owner".
    let owner_resolved = use_signal(|| false);
    // A merge/claim/set-owner in flight — disables every mutating control so a
    // second click can't race the first (the confirm especially is irreversible).
    let busy = use_signal(|| false);
    // The single open take-over preview: (from_slug, counts). None = closed. Only
    // one at a time (the spec) — opening another replaces it.
    let preview = use_signal(|| Option::<(String, ActorMergeResult)>::None);

    // Resolve the owner binding once (re-runs when refresh bumps, e.g. after a
    // set-owner). Writes the EXACT bound slug (or empty) into `owner` so rows can
    // compare synchronously, and marks resolution done so the prompt is honest.
    {
        let mut owner = owner;
        let mut owner_resolved = owner_resolved;
        let _ = use_resource(move || {
            let store = store();
            async move {
                let _ = refresh();
                owner.set(
                    owner_binding(&store)
                        .await
                        .map(|p| p.slug)
                        .unwrap_or_default(),
                );
                owner_resolved.set(true);
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

            // Explainer: designate which identity is you (the owner), then what
            // claim vs take-over do for the OTHER identities.
            div {
                id: "identity-explainer",
                style: "color: {DIM}; font-size: 0.85rem; line-height: 1.55; margin-top: 0.7rem;",
                "Identities are who authors entries. First designate which identity is "
                span { style: "color: {INK};", "you — the owner" }
                ". Then, for other identities that are actually you, "
                span { style: "color: {INK};", "take over" }
                " a human one to merge its whole history into yours, or "
                span { style: "color: {INK};", "claim" }
                " an AI one to mark it yours."
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

            // No owner bound yet (resolved, empty): the first step is to pick
            // which real identity is you. Claim/take-over stay disabled on every
            // row until then; only "Set as owner" is offered.
            if owner_resolved() && owner().is_empty() {
                div {
                    id: "identity-pick-owner",
                    style: "color: {INK}; font-size: 0.88rem; line-height: 1.55; background: {PANEL}; \
                            border: 1px solid {GOLD}; border-radius: 10px; padding: 0.7rem 0.9rem; \
                            margin: 0 0 1rem;",
                    span { style: "font-weight: 700; color: {GOLD};", "Pick which identity is you." }
                    " Choose your identity below with "
                    span { style: "font-weight: 700;", "Set as owner" }
                    ". Until then, claiming and taking over other identities is disabled."
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
    // Whether an owner is firmly bound. When empty, no identity is the owner and
    // the ONLY control offered on any row is "Set as owner" — claim/take-over are
    // withheld entirely (not merely disabled) until the user picks who they are.
    let has_owner = !owner_slug.is_empty();
    // The owner is "you", by EXACT slug. Only a bound owner can match.
    let is_owner = has_owner && owner_slug == person.slug;
    // An AI already linked to you (Person.owner == owner). Writer rows carry the
    // owner journal_writers reported, so a claimed-then-forgotten row still reads.
    let is_owned_by_me = has_owner && person.owner.as_deref() == Some(&*owner_slug);
    // Belt-and-suspenders: take-over/claim may only ever target a DIFFERENT
    // identity than the owner. On a non-owner row with a bound owner this is
    // always true, but compute it explicitly so self-targeting can never render.
    let can_reconcile = has_owner && !is_owner;
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
                // Kind correction. An identity imported or created with the wrong
                // type is reclassified here. This is also the ESCAPE HATCH for an
                // owner that came in as an AI: "This is me" is withheld on AI rows,
                // so without this you could never designate yourself — and mail/
                // pickers would keep labelling you "(AI)". Marking your row a person
                // fixes both. Offered on every AI row (owner included); the reverse
                // ("Mark as AI") is offered on non-owner human rows only — the owner
                // is a person by definition.
                if is_ai {
                    {
                        let k_slug = person.slug.clone();
                        let k_author = if owner_slug.is_empty() {
                            person.slug.clone()
                        } else {
                            owner_slug.clone()
                        };
                        rsx! {
                            button {
                                id: "identity-kind-{person.slug}",
                                disabled: busy(),
                                style: "background: {BG}; color: {DIM}; border: 1px solid {EDGE}; \
                                        border-radius: 999px; padding: 0.25rem 0.7rem; font: inherit; \
                                        font-size: 0.74rem; font-weight: 600; cursor: pointer; white-space: nowrap;",
                                title: "This identity is a person, not an AI agent",
                                onclick: move |_| {
                                    if busy() { return; }
                                    let (k_slug, k_author) = (k_slug.clone(), k_author.clone());
                                    let store = store();
                                    busy.set(true);
                                    err.set(None);
                                    spawn(async move {
                                        match store
                                            .people_update(
                                                &k_slug,
                                                PersonPatch {
                                                    kind: Some(ActorKind::Human),
                                                    ..Default::default()
                                                },
                                                &k_author,
                                            )
                                            .await
                                        {
                                            Ok(_) => refresh += 1,
                                            Err(e) => err.set(Some(format!("{e:#}"))),
                                        }
                                        busy.set(false);
                                    });
                                },
                                "Not an AI — mark as person"
                            }
                        }
                    }
                } else if !is_owner {
                    {
                        let k_slug = person.slug.clone();
                        let k_author = if owner_slug.is_empty() {
                            person.slug.clone()
                        } else {
                            owner_slug.clone()
                        };
                        rsx! {
                            button {
                                id: "identity-kind-{person.slug}",
                                disabled: busy(),
                                style: "background: none; color: {FAINT}; border: 1px solid {EDGE}; \
                                        border-radius: 999px; padding: 0.25rem 0.7rem; font: inherit; \
                                        font-size: 0.74rem; font-weight: 600; cursor: pointer; white-space: nowrap;",
                                title: "Mark this identity as an AI agent",
                                onclick: move |_| {
                                    if busy() { return; }
                                    let (k_slug, k_author) = (k_slug.clone(), k_author.clone());
                                    let store = store();
                                    busy.set(true);
                                    err.set(None);
                                    spawn(async move {
                                        match store
                                            .people_update(
                                                &k_slug,
                                                PersonPatch {
                                                    kind: Some(ActorKind::Ai),
                                                    ..Default::default()
                                                },
                                                &k_author,
                                            )
                                            .await
                                        {
                                            Ok(_) => refresh += 1,
                                            Err(e) => err.set(Some(format!("{e:#}"))),
                                        }
                                        busy.set(false);
                                    });
                                },
                                "Mark as AI"
                            }
                        }
                    }
                }
                // Ownership controls — never on the owner's own row.
                if !is_owner {
                    div {
                        style: "display: flex; align-items: center; gap: 0.45rem; flex-wrap: wrap; \
                                justify-content: flex-end;",
                        // "This is me": designate the OWNER. Only a HUMAN identity can
                        // be the owner — an AI identity is owned BY you, never the
                        // owner, so this control is withheld on AI rows (clicking it
                        // on e.g. `pia` would have made pia the owner = made you pia).
                        if !is_ai {
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
                                        "This is me"
                                    }
                                }
                            }
                        }
                        // AI row with no human owner bound yet: it can't be claimed
                        // until you designate yourself, so guide instead of leaving a
                        // footgun. (Once an owner exists, "Claim as mine" appears below.)
                        if is_ai && !has_owner {
                            span {
                                style: "font-size: 0.72rem; color: {FAINT};",
                                "Set yourself (a human identity) as owner first, then claim this agent"
                            }
                        }
                        // AI: claim (link to you) unless already owned by you.
                        // Withheld entirely until an owner is bound (can_reconcile).
                        if can_reconcile && is_ai && !is_owned_by_me {
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
                        // Human: take over (merge history into you) — opens the
                        // preview. can_reconcile already excludes the owner's own
                        // row, so the merge target can never equal this row (no
                        // self-merge can ever be initiated); withheld until an
                        // owner is bound.
                        if can_reconcile && !is_ai {
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

// ── mail body rendering — SAFE (DIRECTION.md D17/D24) ─────────────────────────
//
// Mail bodies are the single biggest XSS surface in hive: untrusted,
// attacker-controlled markup rendered inside the app's own WebKit via
// `dangerous_inner_html`. Two dedicated renderers gate it — one for HTML, one
// for plaintext — and both make ZERO network requests by default (the
// tracking-pixel / privacy defense). The ammonia policy below is the security
// contract; its hostile-corpus unit tests (see the tests module) are what keep
// it honest.

/// Sanitize an untrusted email HTML body to markup that is SAFE to inject via
/// `dangerous_inner_html`. Strict allowlist, deny by default:
///   - DROPPED entirely: `<script>`, `<style>`, `<iframe>`, `<object>`,
///     `<embed>`, `<form>`, `<input>`, `<link>`, `<base>`, `<meta>` (ammonia's
///     default tag allowlist excludes them; script/style contents are removed
///     too, not just unwrapped).
///   - NO event handlers, NO inline `style` (kills CSS `url()` exfiltration /
///     `expression()`), NO `class`/`id` — only the allowlisted per-tag attrs
///     below survive; every other attribute is stripped.
///   - URL schemes are restricted to http/https/mailto, so `javascript:` and
///     `data:` URIs are neutralized wherever a URL can appear.
///   - Remote resource loads are blocked unless `allow_remote_images`: with it
///     OFF, `<img>` (and `<video>`/`<audio>`/`<source>`/`<picture>`) are
///     removed so the reader fetches nothing. With it ON — an explicit,
///     per-message user action — only `<img src>` returns, still http/https
///     only and still WITHOUT `srcset` (no alternate remote candidates).
///   - Links survive but are made inert: forced `target=_blank` +
///     `rel="noopener noreferrer nofollow"` so a click opens the OS browser and
///     can NEVER navigate the app webview.
fn sanitize_email_html(html: &str, allow_remote_images: bool) -> String {
    let mut b = ammonia::Builder::default();
    // Restrict navigable/loadable URLs to safe schemes everywhere: no
    // `javascript:`, no `data:` (blocks data-URI script vectors and inline
    // data: images alike).
    b.url_schemes(
        ["http", "https", "mailto"]
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
    )
    // Neutralize links: open externally, never in-app. `link_rel` stamps the
    // rel; `set_tag_attribute_value` forces target=_blank even when the source
    // `<a>` carried none.
    .link_rel(Some("noopener noreferrer nofollow"))
    .add_tag_attributes("a", ["target"])
    .set_tag_attribute_value("a", "target", "_blank");

    if allow_remote_images {
        // Opt-in: allow http(s) images only. `src` rides ammonia's default
        // url-relative handling under the restricted scheme set; `srcset` and
        // `style` stay disallowed so no alternate/CSS-driven fetch sneaks in.
        b.add_tags(["img"])
            .add_tag_attributes("img", ["src", "alt", "title", "width", "height"])
            // Any lingering remote media/frame tags stay dropped even in this mode.
            .rm_tags(["video", "audio", "source", "picture", "track", "srcset"]);
        b.clean(html).to_string()
    } else {
        // Default: strip every remote-fetching element so the render is inert.
        b.rm_tags(["img", "video", "audio", "source", "picture", "track"]);
        b.clean(html).to_string()
    }
}

/// Does this stored body look like HTML (vs. rendered plaintext)? The engine
/// stores plaintext for `plain` messages and html2text output for HTML-only
/// ones, so most bodies are already text — but a body that still carries tags
/// must go through the sanitizer, never straight to the DOM. A cheap sniff:
/// any `<tag …>` or a bare HTML entity is enough to route through ammonia.
fn body_looks_like_html(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    [
        "<a ",
        "<p>",
        "<p ",
        "<div",
        "<br",
        "<span",
        "<table",
        "<img",
        "<html",
        "<body",
        "<!doctype",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Render a plaintext email body as SAFE HTML: HTML-escape every character
/// (so `<script>` in a plaintext body is inert text, never markup), linkify
/// bare http(s) URLs into inert external links, and preserve whitespace. Makes
/// zero network requests. Used when the body is plaintext (the common case).
fn render_plaintext_email(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 32);
    for token in split_keep_urls(body) {
        match token {
            UrlToken::Url(url) => {
                // The URL text is escaped for display; the href is the same
                // escaped string (only http/https reach here) so it can't break
                // out of the attribute or carry a javascript: scheme.
                let esc = escape_html(url);
                out.push_str(&format!(
                    "<a href=\"{esc}\" target=\"_blank\" rel=\"noopener noreferrer nofollow\">{esc}</a>"
                ));
            }
            UrlToken::Text(text) => out.push_str(&escape_html(text)),
        }
    }
    out
}

enum UrlToken<'a> {
    Url(&'a str),
    Text(&'a str),
}

/// Split text into alternating plain runs and bare http(s) URLs. A URL runs to
/// the first whitespace or angle bracket and is trimmed of trailing sentence
/// punctuation so "see https://x.test." doesn't swallow the period.
fn split_keep_urls(text: &str) -> Vec<UrlToken<'_>> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut run_start = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            if run_start < i {
                tokens.push(UrlToken::Text(&text[run_start..i]));
            }
            let end_rel = rest
                .find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"')
                .unwrap_or(rest.len());
            let mut url = &rest[..end_rel];
            // Drop trailing punctuation that is almost never part of the URL.
            url = url.trim_end_matches(['.', ',', ')', ']', '}', '!', '?', ';', ':', '\'', '"']);
            if url.is_empty() {
                url = &rest[..end_rel];
            }
            tokens.push(UrlToken::Url(url));
            i += url.len();
            run_start = i;
        } else {
            // Advance by one full char to stay on UTF-8 boundaries.
            i += rest.chars().next().map(char::len_utf8).unwrap_or(1);
        }
    }
    if run_start < text.len() {
        tokens.push(UrlToken::Text(&text[run_start..]));
    }
    tokens
}

/// Minimal, allocation-light HTML entity escape for text nodes and attribute
/// values. Escapes the five characters that matter for both contexts.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a stored mail body to SAFE HTML for the reader: HTML-ish bodies go
/// through `sanitize_email_html`, everything else through the escaping
/// plaintext renderer. `allow_remote_images` only affects the HTML path.
fn render_email_body(body: &str, allow_remote_images: bool) -> String {
    if body_looks_like_html(body) {
        sanitize_email_html(body, allow_remote_images)
    } else {
        render_plaintext_email(body)
    }
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

/// Step one calendar day, wrapping months and years: (2026, 7, 31) forward →
/// (2026, 8, 1); (2026, 1, 1) prev → (2025, 12, 31). Pure integer math over
/// `days_in_month` so the Day view's ‹/› never pulls chrono. `month` must be
/// 1-12 and `day` a valid day-of-month.
fn step_day(year: i32, month: u32, day: u32, forward: bool) -> (i32, u32, u32) {
    if forward {
        if day < days_in_month(year, month) {
            (year, month, day + 1)
        } else {
            let (ny, nm) = step_month(year, month, true);
            (ny, nm, 1)
        }
    } else if day > 1 {
        (year, month, day - 1)
    } else {
        let (py, pm) = step_month(year, month, false);
        (py, pm, days_in_month(py, pm))
    }
}

/// The Sunday that starts the week containing `(year, month, day)` — i.e. step
/// back by the date's weekday (0 = Sunday). Returned as its own (y, m, d),
/// wrapping across a month/year boundary. Pure (uses `weekday` + `step_day`),
/// so the Week view's column set is testable without a clock.
fn week_start_sunday(year: i32, month: u32, day: u32) -> (i32, u32, u32) {
    let mut cur = (year, month, day);
    for _ in 0..weekday(year, month, day) {
        cur = step_day(cur.0, cur.1, cur.2, false);
    }
    cur
}

/// The seven consecutive days (Sun→Sat) of the week containing `(y, m, d)`,
/// each as its own (year, month, day). Built by stepping forward from the
/// week's Sunday, so it wraps month/year ends correctly.
fn week_days(year: i32, month: u32, day: u32) -> Vec<(i32, u32, u32)> {
    let mut cur = week_start_sunday(year, month, day);
    let mut days = Vec::with_capacity(7);
    days.push(cur);
    for _ in 0..6 {
        cur = step_day(cur.0, cur.1, cur.2, true);
        days.push(cur);
    }
    days
}

/// A short weekday name (0 = Sunday), e.g. "Mon" — used by the Day header and
/// the week/day labels.
fn weekday_abbrev(wd: u32) -> &'static str {
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    WD.get(wd as usize).copied().unwrap_or("")
}

/// A short month name (1 = January), e.g. "Jul" — used by the week-range and
/// day labels (the month grid uses the full `month_label`).
fn month_abbrev(month: u32) -> &'static str {
    const MO: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MO.get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("")
}

/// The contextual label for the Week view: the span of its seven days, e.g.
/// "Jul 7–13, 2026", collapsing a shared month/year and spelling both out when
/// the week straddles a boundary ("Nov 30 – Dec 6, 2026", "Dec 28, 2026 – Jan
/// 3, 2027"). Pure over `week_days`.
fn week_range_label(year: i32, month: u32, day: u32) -> String {
    let days = week_days(year, month, day);
    let (fy, fm, fd) = days[0];
    let (ly, lm, ld) = days[6];
    if fy == ly && fm == lm {
        format!("{} {fd}–{ld}, {fy}", month_abbrev(fm))
    } else if fy == ly {
        format!(
            "{} {fd} – {} {ld}, {fy}",
            month_abbrev(fm),
            month_abbrev(lm)
        )
    } else {
        format!(
            "{} {fd}, {fy} – {} {ld}, {ly}",
            month_abbrev(fm),
            month_abbrev(lm)
        )
    }
}

/// The contextual label for the Day view, e.g. "Monday, Jul 12, 2026".
fn day_label(year: i32, month: u32, day: u32) -> String {
    const FULL: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let wd = FULL
        .get(weekday(year, month, day) as usize)
        .copied()
        .unwrap_or("");
    format!("{wd}, {} {day}, {year}", month_abbrev(month))
}

/// The 0–23 start-hour of a timed event `at`, or None when it carries no time
/// (all-day / untimed) — the Day timeline buckets timed events into hour rows
/// by this. Derived from `event_time` so it accepts exactly the same shapes.
fn event_hour(at: &str) -> Option<u32> {
    let t = event_time(at)?;
    t.get(0..2)?.parse::<u32>().ok().filter(|h| *h <= 23)
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

// ── task due helpers (chrono-free, pure, unit-tested) ─────────────────────────
//
// The Reminders smart views (Today / Scheduled) and the due chip need only:
// which (y,m,d) a task's `due` string falls on, and how that day sits relative
// to today (overdue / today / future / undated). `task_due_day` reuses the
// calendar's `event_day` parser (it already accepts a bare date OR a
// datetime-ish string and validates the calendar), so a task due and an event
// `at` bucket identically. All integer comparison — no clock, no chrono.

/// The (year, month, day) a task `due` string names, or None when it's absent
/// or unparseable (which then reads as "undated", never a wrong day).
fn task_due_day(due: &str) -> Option<(i32, u32, u32)> {
    event_day(due)
}

/// Where a `due` sits relative to `today` (a `YYYY-MM-DD` string). Ordered so
/// overdue sorts before today before future; undated sorts last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DueBucket {
    Overdue,
    Today,
    Future,
    Undated,
}

/// Classify a task `due` against `today` by plain (y,m,d) tuple comparison. An
/// absent/garbage due is `Undated`; a valid past day is `Overdue`, an equal day
/// `Today`, a later day `Future`. `today` is a `YYYY-MM-DD` (from `today_ymd`).
fn due_bucket(due: Option<&str>, today: &str) -> DueBucket {
    let Some(day) = due.and_then(task_due_day) else {
        return DueBucket::Undated;
    };
    let Some(t) = parse_ymd(today) else {
        // Without a valid "today" we can't order it — treat as future so it's
        // never wrongly flagged overdue.
        return DueBucket::Future;
    };
    match day.cmp(&t) {
        std::cmp::Ordering::Less => DueBucket::Overdue,
        std::cmp::Ordering::Equal => DueBucket::Today,
        std::cmp::Ordering::Greater => DueBucket::Future,
    }
}

/// A short, human due label for the chip: "Overdue", "Today", "Tomorrow", or a
/// month-day like "Jul 15" (with the year when it's not `today`'s year). Garbage
/// dues fall back to their trimmed raw text so nothing is silently dropped.
fn due_label(due: &str, today: &str) -> String {
    let Some((y, m, d)) = task_due_day(due) else {
        return due.trim().to_string();
    };
    match due_bucket(Some(due), today) {
        DueBucket::Overdue => "Overdue".to_string(),
        DueBucket::Today => "Today".to_string(),
        DueBucket::Future => {
            // "Tomorrow" when it's exactly the next calendar day.
            if let Some((ty, tm, td)) = parse_ymd(today) {
                let (ny, nm, nd) = step_day(ty, tm, td, true);
                if (y, m, d) == (ny, nm, nd) {
                    return "Tomorrow".to_string();
                }
            }
            short_month_day(y, m, d, today)
        }
        DueBucket::Undated => due.trim().to_string(),
    }
}

/// "Jul 15" (same year as today) or "Jul 15 2027" (a different year).
fn short_month_day(year: i32, month: u32, day: u32, today: &str) -> String {
    const ABBR: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let name = ABBR
        .get((month.saturating_sub(1)) as usize)
        .copied()
        .unwrap_or("");
    let same_year = parse_ymd(today).map(|(ty, ..)| ty == year).unwrap_or(true);
    if same_year {
        format!("{name} {day}")
    } else {
        format!("{name} {day} {year}")
    }
}

/// Priority as a sortable rank (higher = more urgent) so the checklist can put
/// urgent/high tasks above normal/low. Mirrors the list's priority order.
fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::Urgent => 3,
        Priority::High => 2,
        Priority::Normal => 1,
        Priority::Low => 0,
    }
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

// ── Apple-Contacts pure helpers (favorites, groups, avatars, A–Z) ─────────────
//
// All total and clock-free so they unit-test without a store (like age_years /
// the calendar library). Favorites/groups live in the contact's own
// EntityFields — `favorite` (Bool) and `groups` (Text, comma-separated) — so
// these read/format those JSON values with no fold or schema involvement.

/// Whether a contact is favorited: its `favorite` Bool field is exactly `true`.
/// Absent/null/false all read as not-favorite (existing contacts are null).
fn contact_is_favorite(e: &CustomEntity) -> bool {
    matches!(e.fields.get("favorite"), Some(Value::Bool(true)))
}

/// Parse a comma-separated `groups` string into distinct, trimmed group names,
/// in first-seen order, dropping blanks and case-insensitive duplicates. The
/// storage format is a single Text field ("Family, Work") so there is no schema
/// change; this is the one place that format is interpreted.
fn parse_groups(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for part in raw.split(',') {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.to_lowercase()) {
            out.push(name.to_string());
        }
    }
    out
}

/// A contact's groups (its `groups` Text field, parsed).
fn contact_groups(e: &CustomEntity) -> Vec<String> {
    parse_groups(&value_str(e.fields.get("groups")))
}

/// Join group names back into the stored comma-separated form ("Family, Work").
/// Round-trips with `parse_groups` (which re-trims/dedupes on the way in).
fn join_groups(groups: &[String]) -> String {
    groups.join(", ")
}

/// A stable slug for a group name, for the left-rail row id `#contacts-group-{slug}`.
/// Lowercased, non-alphanumerics collapsed to single hyphens, trimmed — enough
/// to be a unique, valid id per distinct name at household scale.
fn group_slug(name: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    s.trim_matches('-').to_string()
}

/// The initials shown in a contact's avatar: first letter of the first word and
/// first letter of the last word, uppercased (a single word yields one letter).
/// A name with no letters (blank / symbols only) yields "#", matching the "#"
/// alphabetical bucket.
fn avatar_initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| !w.is_empty()).collect();
    let first_letter = |w: &str| w.chars().find(|c| c.is_alphanumeric());
    let mut initials = String::new();
    if let Some(f) = words.first().and_then(|w| first_letter(w)) {
        initials.extend(f.to_uppercase());
    }
    if words.len() > 1 {
        if let Some(l) = words.last().and_then(|w| first_letter(w)) {
            initials.extend(l.to_uppercase());
        }
    }
    if initials.is_empty() {
        "#".to_string()
    } else {
        initials
    }
}

/// A deterministic avatar background color for a name, so the same contact keeps
/// the same hue across launches. FNV-1a over the lowercased name indexes a fixed
/// palette tuned for the dark theme (muted, readable against light initials).
fn avatar_color(name: &str) -> &'static str {
    // Warm, muted palette that sits on the {BG} dark theme without a light mode.
    const PALETTE: &[&str] = &[
        "#b5793a", // amber
        "#7a6f3a", // olive gold
        "#8a5a4a", // clay
        "#5f6b4a", // moss
        "#4a6b6b", // teal-slate
        "#6b5a7a", // muted plum
        "#7a5a5a", // dusty rose
        "#5a6b7a", // steel blue
        "#7a6b4a", // bronze
        "#4a5f6b", // deep slate
    ];
    let mut hash: u32 = 2166136261; // FNV-1a offset basis
    for b in name.trim().to_lowercase().bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
}

/// A deterministic accent color for an event, so the same happening keeps the
/// same hue in every calendar view (month chip, week chip, day block). Chosen
/// FOLD-SAFELY without any color column: hash the event's first tag if it has
/// one (so a tag like "work" tints all its events alike), else its title, with
/// the same FNV-1a the contact avatars use. Always returns a palette member.
fn event_color(ev: &EventItem) -> &'static str {
    // Six saturated-but-muted hues that read as event blocks on the {BG} dark
    // theme (distinct from the warmer, dimmer avatar palette).
    const PALETTE: &[&str] = &[
        "#c56b4f", // terracotta
        "#c99a3f", // amber
        "#6f9a52", // leaf
        "#3f9a8a", // teal
        "#5a86c9", // azure
        "#8a6bc9", // violet
        "#c25f8a", // magenta-rose
        "#9a8a52", // brass
    ];
    let seed = ev
        .tags
        .iter()
        .map(|t| t.trim())
        .find(|t| !t.is_empty())
        .unwrap_or(ev.title.trim());
    let mut hash: u32 = 2166136261; // FNV-1a offset basis
    for b in seed.to_lowercase().bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
}

/// The A–Z section bucket a display name sorts under: its first ASCII letter
/// (uppercased), or "#" for names that start with a digit/symbol or are blank.
/// Non-ASCII-alphabetic leading chars also bucket under "#".
fn section_letter(name: &str) -> String {
    match name.trim().chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
        _ => "#".to_string(),
    }
}

/// The case-insensitive sort key for the alphabetical list: a name that has no
/// leading ASCII letter (blank, or starting with a digit/symbol) sinks into the
/// trailing "#" bucket (leading `1`), everything else sorts by lowercased name
/// (leading `0`). Ties break on the raw name for stability.
fn contact_sort_key(name: &str) -> (u8, String) {
    let trimmed = name.trim();
    let bucket = match trimmed.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => 0,
        _ => 1,
    };
    (bucket, trimmed.to_lowercase())
}

/// Collect the distinct group names across all contacts, sorted case-insensitively
/// for the left rail. Each name maps to a stable `group_slug` for its row id.
fn all_group_names(contacts: &[CustomEntity]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for c in contacts {
        for g in contact_groups(c) {
            if seen.insert(g.to_lowercase()) {
                names.push(g);
            }
        }
    }
    names.sort_by_key(|a| a.to_lowercase());
    names
}

/// Does a contact match the free-text search? Case-insensitive substring over
/// the display name plus email/phone/organization/title/nickname — the fields a
/// person would type to find someone. Empty query matches everything.
fn contact_matches_search(e: &CustomEntity, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let mut hay = contact_display(e).to_lowercase();
    for key in ["email", "phone", "organization", "title", "nickname"] {
        let v = value_str(e.fields.get(key));
        if !v.is_empty() {
            hay.push(' ');
            hay.push_str(&v.to_lowercase());
        }
    }
    hay.contains(&q)
}

/// The active left-rail filter: All contacts, just Favorites, or one named group.
#[derive(Clone, PartialEq)]
enum ContactFilter {
    All,
    Favorites,
    Group(String),
}

impl ContactFilter {
    /// Does this contact pass the source-list filter (before the search box)?
    fn accepts(&self, e: &CustomEntity) -> bool {
        match self {
            ContactFilter::All => true,
            ContactFilter::Favorites => contact_is_favorite(e),
            ContactFilter::Group(name) => {
                let lc = name.to_lowercase();
                contact_groups(e).iter().any(|g| g.to_lowercase() == lc)
            }
        }
    }
}

/// The Contacts section, styled as an Apple-Contacts app: a left source list
/// (All / Favorites / group rail + a search box, then the alphabetical contact
/// list with initials avatars and A–Z section headers) beside a right card (the
/// selected contact via the reusable `EntityDetail`, restyled). Selection is a
/// LOCAL signal so the list ↔ card switch happens IN PLACE inside this pane —
/// the outer Section sidebar is untouched, and the global `selected` (which
/// swaps the whole main pane for tasks/events) is deliberately not used here.
/// The `contact` type is seeded idempotently on mount (ensure_contact_type),
/// backfilling `favorite`/`groups`, so the very first use works with no setup.
#[component]
fn ContactsPane(store: ReadOnlySignal<Store>, refresh: Signal<u32>) -> Element {
    let mut new_name = use_signal(String::new);
    let mut err = use_signal(|| Option::<String>::None);
    let mut search = use_signal(String::new);
    let filter = use_signal(|| ContactFilter::All);
    // The card column's selection, local to this pane (the outer `selected`
    // signal would take over the ENTIRE main pane; here the list must stay). It
    // carries a `Selected::Contact` so the reused EntityDetail can clear it on
    // its "← Back" (which returns the card column to the empty state).
    let card_sel = use_signal(|| Option::<Selected>::None);
    // Bumped by favorite/group edits in the card so the left list re-pulls.
    let local_refresh = use_signal(|| 0u32);
    let mut show_new = use_signal(|| false);

    // Seed the type (idempotent; backfills favorite/groups), then list its
    // instances. Re-pulls on the outer `refresh` (journal [contact:] emergence)
    // and the pane-local one (favorite/group edits from the card).
    let contacts = use_resource(move || {
        let store = store();
        async move {
            let _ = refresh();
            let _ = local_refresh();
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
        let mut card_sel = card_sel;
        spawn(async move {
            // Ensure the type first (a fresh store may not have it yet), then
            // create the card and open it in the right column.
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
                    show_new.set(false);
                    refresh += 1;
                    card_sel.set(Some(Selected::Contact(e.id)));
                }
                Err(e) => err.set(Some(entity_err(e))),
            }
        });
    };

    rsx! {
        div {
            id: "contacts-pane",
            style: "display: flex; height: 100%; min-height: 0;",

            // ── left source list ─────────────────────────────────────────────
            div {
                style: "width: 21rem; flex-shrink: 0; height: 100%; box-sizing: border-box; \
                        border-right: 1px solid {EDGE}; background: {PANEL}; display: flex; \
                        flex-direction: column; min-height: 0;",

                // header row: title + new-contact "+"
                div {
                    style: "display: flex; align-items: center; gap: 0.5rem; padding: 1rem 1rem 0.6rem;",
                    div { style: "font-size: 1.25rem; font-weight: 700; flex: 1;", "Contacts" }
                    button {
                        id: "contact-new",
                        style: "background: {EDGE}; color: {GOLD}; border: none; border-radius: 50%; \
                                width: 1.9rem; height: 1.9rem; font-size: 1.15rem; line-height: 1; \
                                cursor: pointer; flex-shrink: 0;",
                        title: "New contact",
                        onclick: move |_| {
                            let now = !show_new();
                            show_new.set(now);
                        },
                        "+"
                    }
                }

                // new-contact inline form (toggled by the +)
                if show_new() {
                    div {
                        style: "padding: 0 1rem 0.7rem;",
                        div {
                            style: "display: flex; gap: 0.4rem;",
                            input {
                                id: "contact-new-name",
                                style: "flex: 1; min-width: 0; box-sizing: border-box; background: {BG}; \
                                        color: {INK}; border: 1px solid {EDGE}; border-radius: 8px; \
                                        padding: 0.5rem 0.6rem; font: inherit; font-size: 0.88rem; outline: none;",
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
                                        padding: 0.5rem 0.9rem; font-weight: 700; font-size: 0.85rem; cursor: pointer;",
                                onclick: move |_| create(),
                                "Add"
                            }
                        }
                        if let Some(e) = err() {
                            div {
                                style: "color: #e07a5f; font-size: 0.8rem; margin-top: 0.4rem;",
                                "{e}"
                            }
                        }
                    }
                }

                // search box
                div {
                    style: "padding: 0 1rem 0.6rem;",
                    input {
                        id: "contacts-search",
                        style: "width: 100%; box-sizing: border-box; background: {BG}; color: {INK}; \
                                border: 1px solid {EDGE}; border-radius: 8px; padding: 0.5rem 0.7rem; \
                                font: inherit; font-size: 0.88rem; outline: none;",
                        r#type: "search",
                        placeholder: "Search",
                        value: "{search}",
                        oninput: move |e| search.set(e.value()),
                    }
                }

                // filter rail + the alphabetical list share the scroll area
                div {
                    style: "flex: 1; min-height: 0; overflow-y: auto; padding: 0 0.5rem 1rem;",
                    match contacts() {
                        None => muted("loading contacts…"),
                        Some(Err(e)) => muted(&format!("contacts unavailable: {e}")),
                        Some(Ok(list)) => {
                            let groups = all_group_names(&list);
                            rsx! {
                                {contact_filter_rail(filter, &groups, &list)}
                                {contact_source_list(&list, filter(), &search(), card_sel)}
                            }
                        }
                    }
                }
            }

            // ── right card column ────────────────────────────────────────────
            div {
                id: "contact-card-col",
                style: "flex: 1; min-width: 0; height: 100%; overflow-y: auto;",
                if let Some(Selected::Contact(id)) = card_sel() {
                    // Key on the id so switching contacts makes a fresh card
                    // instance (fresh resource + cleared group-input state).
                    // `id` is cloned for the prop so the `key: "{id}"` borrow
                    // survives it — the move-then-borrow otherwise borrow-checks
                    // clean in debug but FAILS in release (opt-level temporary
                    // lifetime difference), which is why CI/debug missed it.
                    ContactCard { key: "{id}", store, card_sel, refresh, local_refresh, id: id.clone() }
                } else {
                    {contact_empty_state()}
                }
            }
        }
    }
}

/// The left rail's filter chips: All Contacts, Favorites, then one row per
/// distinct group name (sorted). The active one is highlighted. Selecting a row
/// sets the `filter` signal; the list below re-filters. Plain fn: it takes a
/// slice and signal, no PartialEq props.
fn contact_filter_rail(
    mut filter: Signal<ContactFilter>,
    groups: &[String],
    contacts: &[CustomEntity],
) -> Element {
    let active = filter();
    let fav_count = contacts.iter().filter(|c| contact_is_favorite(c)).count();
    let all_count = contacts.len();
    let row_style = |on: bool| {
        let (bg, fg) = if on {
            (GOLD.to_string(), "#14120e".to_string())
        } else {
            ("transparent".to_string(), INK.to_string())
        };
        format!(
            "display: flex; align-items: center; gap: 0.55rem; width: 100%; text-align: left; \
             background: {bg}; color: {fg}; border: none; border-radius: 8px; \
             padding: 0.45rem 0.6rem; font: inherit; font-size: 0.9rem; cursor: pointer; \
             margin-bottom: 0.15rem;"
        )
    };
    rsx! {
        div {
            style: "margin-bottom: 0.5rem;",
            button {
                id: "contacts-filter-all",
                style: "{row_style(matches!(active, ContactFilter::All))} font-weight: 600;",
                onclick: move |_| filter.set(ContactFilter::All),
                span { style: "width: 1.2rem; text-align: center;", "◎" }
                span { style: "flex: 1;", "All Contacts" }
                span { style: "font-size: 0.78rem; opacity: 0.8;", "{all_count}" }
            }
            button {
                id: "contacts-filter-favorites",
                style: "{row_style(matches!(active, ContactFilter::Favorites))} font-weight: 600;",
                onclick: move |_| filter.set(ContactFilter::Favorites),
                span { style: "width: 1.2rem; text-align: center;", "⭐" }
                span { style: "flex: 1;", "Favorites" }
                span { style: "font-size: 0.78rem; opacity: 0.8;", "{fav_count}" }
            }
            if !groups.is_empty() {
                div {
                    style: "font-size: 0.68rem; font-weight: 700; letter-spacing: 0.06em; \
                            text-transform: uppercase; color: {FAINT}; padding: 0.5rem 0.6rem 0.25rem;",
                    "Groups"
                }
                for name in groups.iter() {
                    {group_rail_row(filter, name, contacts)}
                }
            }
        }
    }
}

/// One group row in the filter rail. Split out so each closure owns its own
/// clone of the group name (the file's per-closure-clone idiom).
fn group_rail_row(
    mut filter: Signal<ContactFilter>,
    name: &str,
    contacts: &[CustomEntity],
) -> Element {
    let slug = group_slug(name);
    let name_owned = name.to_string();
    let on = matches!(filter(), ContactFilter::Group(ref g) if g.eq_ignore_ascii_case(name));
    let count = contacts
        .iter()
        .filter(|c| {
            contact_groups(c)
                .iter()
                .any(|g| g.eq_ignore_ascii_case(name))
        })
        .count();
    let (bg, fg) = if on {
        (GOLD.to_string(), "#14120e".to_string())
    } else {
        ("transparent".to_string(), INK.to_string())
    };
    rsx! {
        button {
            id: "contacts-group-{slug}",
            style: "display: flex; align-items: center; gap: 0.55rem; width: 100%; text-align: left; \
                    background: {bg}; color: {fg}; border: none; border-radius: 8px; \
                    padding: 0.45rem 0.6rem; font: inherit; font-size: 0.9rem; cursor: pointer; \
                    margin-bottom: 0.15rem;",
            onclick: move |_| filter.set(ContactFilter::Group(name_owned.clone())),
            span { style: "width: 1.2rem; text-align: center;", "◈" }
            span { style: "flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;", "{name}" }
            span { style: "font-size: 0.78rem; opacity: 0.8;", "{count}" }
        }
    }
}

/// The alphabetical contact list: apply the source filter AND the search, sort
/// case-insensitively by display name (no-letter names sink under "#"), then
/// render A–Z section headers with a row (avatar + name + ⭐) per contact.
fn contact_source_list(
    contacts: &[CustomEntity],
    filter: ContactFilter,
    search: &str,
    card_sel: Signal<Option<Selected>>,
) -> Element {
    let mut shown: Vec<&CustomEntity> = contacts
        .iter()
        .filter(|c| filter.accepts(c) && contact_matches_search(c, search))
        .collect();
    shown.sort_by(|a, b| {
        contact_sort_key(&contact_display(a)).cmp(&contact_sort_key(&contact_display(b)))
    });

    if shown.is_empty() {
        let msg = match (&filter, search.trim().is_empty()) {
            (_, false) => "No contacts match your search.",
            (ContactFilter::Favorites, true) => {
                "No favorites yet. Open a contact and tap the star to add one."
            }
            (ContactFilter::Group(_), true) => "No contacts in this group yet.",
            (ContactFilter::All, true) => {
                "No contacts yet. Tap + to add one — or write [contact: a name] in a journal entry."
            }
        };
        return muted(msg);
    }

    let mut last_letter = String::new();
    rsx! {
        div {
            id: "contact-list",
            for c in shown.into_iter() {
                {
                    let letter = section_letter(&contact_display(c));
                    let header = if letter != last_letter {
                        last_letter = letter.clone();
                        Some(letter)
                    } else {
                        None
                    };
                    rsx! {
                        if let Some(h) = header {
                            div {
                                class: "contact-section-header",
                                style: "font-size: 0.72rem; font-weight: 700; color: {GOLD}; \
                                        letter-spacing: 0.08em; padding: 0.6rem 0.6rem 0.25rem;",
                                "{h}"
                            }
                        }
                        {contact_row(c, card_sel)}
                    }
                }
            }
        }
    }
}

/// One contact row: circular initials avatar (deterministic color) + name + a
/// small ⭐ if favorited. Clicking selects it into the card column. Plain fn:
/// CustomEntity lacks the PartialEq component props need.
fn contact_row(c: &CustomEntity, mut card_sel: Signal<Option<Selected>>) -> Element {
    let id = c.id.clone();
    let name = contact_display(c);
    let initials = avatar_initials(&name);
    let color = avatar_color(&name);
    let favorite = contact_is_favorite(c);
    let selected_now = matches!(card_sel(), Some(Selected::Contact(ref s)) if s == &c.id);
    let bg = if selected_now { EDGE } else { "transparent" };
    rsx! {
        button {
            id: "contact-row-{c.id}",
            class: "contact-row",
            style: "display: flex; align-items: center; gap: 0.65rem; width: 100%; text-align: left; \
                    background: {bg}; border: none; border-radius: 8px; padding: 0.4rem 0.6rem; \
                    margin-bottom: 0.1rem; color: {INK}; font: inherit; cursor: pointer;",
            onclick: move |_| card_sel.set(Some(Selected::Contact(id.clone()))),
            span {
                style: "display: inline-flex; align-items: center; justify-content: center; \
                        width: 2rem; height: 2rem; border-radius: 50%; background: {color}; \
                        color: #f4eeda; font-size: 0.8rem; font-weight: 700; flex-shrink: 0;",
                "{initials}"
            }
            span {
                style: "flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; \
                        white-space: nowrap; font-size: 0.95rem;",
                "{name}"
            }
            if favorite {
                span { style: "color: {GOLD}; font-size: 0.8rem; flex-shrink: 0;", "⭐" }
            }
        }
    }
}

/// The right column when no contact is selected: a tasteful, centered empty
/// state that reads like Apple Contacts' "No Contact Selected".
fn contact_empty_state() -> Element {
    rsx! {
        div {
            id: "contact-empty",
            style: "height: 100%; display: flex; flex-direction: column; align-items: center; \
                    justify-content: center; text-align: center; color: {FAINT}; padding: 2rem;",
            div {
                style: "display: inline-flex; align-items: center; justify-content: center; \
                        width: 4.5rem; height: 4.5rem; border-radius: 50%; background: {PANEL}; \
                        border: 1px solid {EDGE}; color: {FAINT}; font-size: 2rem; margin-bottom: 1rem;",
                "☺"
            }
            div { style: "font-size: 1.05rem; font-weight: 700; color: {DIM};", "No contact selected" }
            div {
                style: "font-size: 0.88rem; margin-top: 0.4rem; max-width: 22rem; line-height: 1.55;",
                "Pick a contact from the list, or tap + to add one. Each contact is a card — \
                 standard details plus any fields you add — and the journal entries that mention \
                 them gather on the card automatically."
            }
        }
    }
}

/// The right-column contact card: the reusable `EntityDetail` (fields, add-field
/// affordance, journal backlinks) preceded by an Apple-style header — a large
/// avatar, the name, a favorite star toggle, and the groups editor (chips +
/// add). Favorite/group edits persist a single field immediately and bump both
/// the card (`local_refresh`, so header + chips re-read) and the left list.
/// A `#[component]` (its props are all Copy/PartialEq) so the header re-pulls
/// the entity independently of EntityDetail's internal working copy.
#[component]
fn ContactCard(
    store: ReadOnlySignal<Store>,
    card_sel: Signal<Option<Selected>>,
    refresh: Signal<u32>,
    local_refresh: Signal<u32>,
    id: String,
) -> Element {
    let err = use_signal(|| Option::<String>::None);
    let mut new_group = use_signal(String::new);

    // The card header reads the contact directly (name + favorite + groups),
    // re-pulling whenever a header edit bumps local_refresh or a field Save
    // bumps refresh. EntityDetail below owns the field editors separately.
    let id_for_load = id.clone();
    let entity = use_resource(move || {
        let store = store();
        let id = id_for_load.clone();
        async move {
            let _ = refresh();
            let _ = local_refresh();
            store
                .custom_entities_get(&id)
                .await
                .map_err(|e| format!("{e:#}"))
        }
    });

    // Single-field writes (favorite toggle, group add/remove) go through the
    // free `persist_contact_field` helper below: each event closure captures the
    // Copy signals + clones the (cheap) `id`, so no shared non-Copy closure is
    // threaded through the chips. `err`/`refresh`/`local_refresh` are Copy.

    rsx! {
        div {
            id: "contact-card",
            style: "max-width: 640px; margin: 0 auto; padding: 1.6rem 1.4rem 0;",
            match entity() {
                None => muted("loading…"),
                Some(Err(e)) => muted(&format!("couldn't open this contact: {e}")),
                Some(Ok(None)) => muted("this contact no longer exists"),
                Some(Ok(Some(c))) => {
                    let name = contact_display(&c);
                    let initials = avatar_initials(&name);
                    let color = avatar_color(&name);
                    let favorite = contact_is_favorite(&c);
                    let groups = contact_groups(&c);
                    let star = if favorite { "⭐" } else { "☆" };
                    let star_color = if favorite { GOLD } else { FAINT };
                    // Add the typed group name to this contact's `groups`, deduped.
                    let add_group = {
                        let groups = groups.clone();
                        let id = id.clone();
                        move || {
                            let g = new_group().trim().to_string();
                            if g.is_empty() {
                                return;
                            }
                            let mut next = groups.clone();
                            next.push(g);
                            // parse_groups on the joined value dedupes case-insensitively.
                            let joined = join_groups(&parse_groups(&join_groups(&next)));
                            new_group.set(String::new());
                            persist_contact_field(
                                store, id.clone(), "groups", Value::String(joined),
                                refresh, local_refresh, err,
                            );
                        }
                    };
                    rsx! {
                        // deselect back to the empty state (the embedded
                        // EntityDetail hides its own back button)
                        button {
                            id: "contact-card-back",
                            style: "background: none; border: none; color: {GOLD}; font: inherit; \
                                    font-size: 0.85rem; cursor: pointer; padding: 0; margin-bottom: 0.8rem;",
                            onclick: move |_| {
                                let mut card_sel = card_sel;
                                card_sel.set(None);
                            },
                            "← Back"
                        }

                        // avatar + name + favorite star header
                        div {
                            style: "display: flex; flex-direction: column; align-items: center; \
                                    text-align: center; margin-bottom: 1.2rem;",
                            span {
                                style: "display: inline-flex; align-items: center; justify-content: center; \
                                        width: 5rem; height: 5rem; border-radius: 50%; background: {color}; \
                                        color: #f4eeda; font-size: 1.8rem; font-weight: 700; margin-bottom: 0.7rem;",
                                "{initials}"
                            }
                            div {
                                style: "display: flex; align-items: center; gap: 0.5rem;",
                                div { id: "contact-card-name", style: "font-size: 1.5rem; font-weight: 700;", "{name}" }
                                button {
                                    id: "contact-favorite",
                                    style: "background: none; border: none; color: {star_color}; \
                                            font-size: 1.35rem; line-height: 1; cursor: pointer; padding: 0;",
                                    title: if favorite { "Remove from Favorites" } else { "Add to Favorites" },
                                    onclick: {
                                        let id = id.clone();
                                        move |_| persist_contact_field(
                                            store, id.clone(), "favorite", Value::Bool(!favorite),
                                            refresh, local_refresh, err,
                                        )
                                    },
                                    "{star}"
                                }
                            }
                            if let Some(h) = contact_hint(&c) {
                                div { style: "font-size: 0.9rem; color: {DIM}; margin-top: 0.2rem;", "{h}" }
                            }
                        }

                        // groups editor: current groups as removable chips + an add input
                        div {
                            id: "contact-groups",
                            style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                                    padding: 0.8rem 1rem; margin-bottom: 1rem;",
                            div {
                                style: "font-size: 0.72rem; font-weight: 700; letter-spacing: 0.06em; \
                                        text-transform: uppercase; color: {FAINT}; margin-bottom: 0.5rem;",
                                "Groups"
                            }
                            div {
                                style: "display: flex; flex-wrap: wrap; gap: 0.4rem; align-items: center;",
                                for g in groups.iter() {
                                    {group_chip(store, &id, g, &groups, refresh, local_refresh, err)}
                                }
                                if groups.is_empty() {
                                    span { style: "color: {FAINT}; font-size: 0.82rem;", "No groups yet." }
                                }
                            }
                            div {
                                style: "display: flex; gap: 0.4rem; margin-top: 0.6rem;",
                                input {
                                    id: "contact-group-add",
                                    style: "flex: 1; min-width: 0; box-sizing: border-box; background: {BG}; \
                                            color: {INK}; border: 1px solid {EDGE}; border-radius: 8px; \
                                            padding: 0.45rem 0.6rem; font: inherit; font-size: 0.85rem; outline: none;",
                                    placeholder: "Add to a group, e.g. Family",
                                    value: "{new_group}",
                                    oninput: move |e| new_group.set(e.value()),
                                    onkeydown: {
                                        let mut add_group = add_group.clone();
                                        move |e: KeyboardEvent| {
                                            if e.key() == Key::Enter {
                                                add_group();
                                            }
                                        }
                                    },
                                }
                                button {
                                    id: "contact-group-add-submit",
                                    style: "background: {EDGE}; color: {INK}; border: none; border-radius: 8px; \
                                            padding: 0.45rem 0.9rem; font: inherit; font-weight: 700; \
                                            font-size: 0.82rem; cursor: pointer;",
                                    onclick: move |_| {
                                        let mut add_group = add_group.clone();
                                        add_group();
                                    },
                                    "Add"
                                }
                            }
                        }

                        if let Some(e) = err() {
                            div { style: "color: #e07a5f; font-size: 0.82rem; margin-bottom: 0.6rem;", "{e}" }
                        }

                        // the reusable typed-field editor + journal backlinks
                        EntityDetail {
                            store,
                            selected: card_sel,
                            refresh,
                            target: Selected::Contact(c.id.clone()),
                            embedded: true,
                        }
                    }
                }
            }
        }
    }
}

/// One removable group chip. Removing rewrites the `groups` field without this
/// name (via the shared free helper). Plain fn: it takes a slice + Copy signals,
/// no PartialEq props, and each chip owns its own clones (per-closure-clone).
#[allow(clippy::too_many_arguments)]
fn group_chip(
    store: ReadOnlySignal<Store>,
    id: &str,
    name: &str,
    groups: &[String],
    refresh: Signal<u32>,
    local_refresh: Signal<u32>,
    err: Signal<Option<String>>,
) -> Element {
    let name_owned = name.to_string();
    let id_owned = id.to_string();
    let remaining: Vec<String> = groups
        .iter()
        .filter(|g| !g.eq_ignore_ascii_case(name))
        .cloned()
        .collect();
    rsx! {
        span {
            class: "contact-group-chip",
            style: "display: inline-flex; align-items: center; gap: 0.35rem; background: {EDGE}; \
                    color: {INK}; border-radius: 999px; padding: 0.22rem 0.35rem 0.22rem 0.6rem; \
                    font-size: 0.82rem;",
            "{name}"
            button {
                style: "background: none; border: none; color: {DIM}; font-size: 0.9rem; line-height: 1; \
                        cursor: pointer; padding: 0 0.15rem;",
                title: "Remove from {name_owned}",
                onclick: move |_| {
                    persist_contact_field(
                        store,
                        id_owned.clone(),
                        "groups",
                        Value::String(join_groups(&remaining)),
                        refresh,
                        local_refresh,
                        err,
                    )
                },
                "×"
            }
        }
    }
}

/// Persist a single field on a contact, then refresh the card header and the
/// left list. The shared write path for the favorite toggle and group
/// add/remove — instant, independent of EntityDetail's Save button. All signals
/// are Copy and `id` is owned, so callers just clone the (cheap) id per closure.
#[allow(clippy::too_many_arguments)]
fn persist_contact_field(
    store: ReadOnlySignal<Store>,
    id: String,
    slug: &'static str,
    value: Value,
    mut refresh: Signal<u32>,
    mut local_refresh: Signal<u32>,
    mut err: Signal<Option<String>>,
) {
    let store = store();
    spawn(async move {
        let mut fields = serde_json::Map::new();
        fields.insert(slug.to_string(), value);
        match store
            .custom_entities_update(
                &id,
                CustomEntityPatch {
                    title: None,
                    fields: Some(fields),
                    scope: None,
                },
                "system",
                None,
            )
            .await
        {
            Ok(_) => {
                err.set(None);
                local_refresh += 1; // card header + group chips re-read
                refresh += 1; // the left list (⭐, group rail) re-pulls
            }
            Err(e) => err.set(Some(entity_err(e))),
        }
    });
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

// ── tasks pane (Apple-Reminders-style) ────────────────────────────────────────

/// Which list the Reminders view is showing. Smart lists are computed views;
/// a `List(project)` is a user list (a task `project`); `NoList` is the bucket
/// of tasks with no project. `Clone`/`PartialEq` so it drives a signal cheaply.
#[derive(Clone, PartialEq)]
enum TaskListSel {
    Today,
    Scheduled,
    Flagged,
    All,
    Completed,
    List(String),
    NoList,
}

impl TaskListSel {
    /// The header title the main column shows for this list.
    fn title(&self) -> String {
        match self {
            TaskListSel::Today => "Today".into(),
            TaskListSel::Scheduled => "Scheduled".into(),
            TaskListSel::Flagged => "Flagged".into(),
            TaskListSel::All => "All".into(),
            TaskListSel::Completed => "Completed".into(),
            TaskListSel::List(p) => p.clone(),
            TaskListSel::NoList => "No List".into(),
        }
    }
}

/// A task is "open" when it hasn't been completed (any status but Done). The
/// smart lists (Today/Scheduled/Flagged/All) are all over open tasks.
fn task_is_open(t: &Task) -> bool {
    t.status != TaskStatus::Done
}

/// A task's list name (its `project`) if it has a non-empty one.
fn task_list_name(t: &Task) -> Option<String> {
    t.project
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// Does a task belong in the given list view? Smart rules per the spec; a user
/// list matches by project; No List matches tasks without one. `today` is a
/// `YYYY-MM-DD`. Completed lists tasks by Done; every other view filters to open
/// tasks (a user list shows open first then completed — handled in ordering, so
/// membership there includes both).
fn task_in_list(t: &Task, sel: &TaskListSel, today: &str) -> bool {
    match sel {
        // open AND (overdue or due today)
        TaskListSel::Today => {
            task_is_open(t)
                && matches!(
                    due_bucket(t.due.as_deref(), today),
                    DueBucket::Overdue | DueBucket::Today
                )
        }
        // open AND has a real due day
        TaskListSel::Scheduled => {
            task_is_open(t) && t.due.as_deref().and_then(task_due_day).is_some()
        }
        // open AND priority ≥ High
        TaskListSel::Flagged => {
            task_is_open(t) && priority_rank(t.priority) >= priority_rank(Priority::High)
        }
        // open (any)
        TaskListSel::All => task_is_open(t),
        // done
        TaskListSel::Completed => t.status == TaskStatus::Done,
        // this project (open + completed; ordering puts open first)
        TaskListSel::List(p) => task_list_name(t).as_deref() == Some(p.as_str()),
        // no project (open + completed)
        TaskListSel::NoList => task_list_name(t).is_none(),
    }
}

/// The count a list shows in the rail. Smart lists count their membership;
/// Completed counts done tasks; a user list / No List count only OPEN tasks
/// (Reminders shows open counts beside lists).
fn task_list_count(tasks: &[Task], sel: &TaskListSel, today: &str) -> usize {
    tasks
        .iter()
        .filter(|t| {
            task_in_list(t, sel, today)
                && match sel {
                    TaskListSel::List(_) | TaskListSel::NoList => task_is_open(t),
                    _ => true,
                }
        })
        .count()
}

/// Order a list's tasks for display: open first, completed last; within open,
/// by due day ascending (undated last), then priority descending; completed by
/// most-recently-updated. Pure, so it's unit-testable and stable.
fn ordered_tasks(mut tasks: Vec<Task>, today: &str) -> Vec<Task> {
    tasks.sort_by(|a, b| {
        let ao = task_is_open(a);
        let bo = task_is_open(b);
        // Open before completed.
        if ao != bo {
            return bo.cmp(&ao); // open (true) first
        }
        if ao {
            // Both open: due asc (undated last), then priority desc.
            let ad = a.due.as_deref().and_then(task_due_day);
            let bd = b.due.as_deref().and_then(task_due_day);
            match (ad, bd) {
                (Some(x), Some(y)) => {
                    if x != y {
                        return x.cmp(&y);
                    }
                }
                (Some(_), None) => return std::cmp::Ordering::Less,
                (None, Some(_)) => return std::cmp::Ordering::Greater,
                (None, None) => {}
            }
            priority_rank(b.priority)
                .cmp(&priority_rank(a.priority))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        } else {
            // Both completed: most recently updated first.
            let _ = today;
            b.updated_at.cmp(&a.updated_at)
        }
    });
    tasks
}

/// The distinct user lists (non-empty `project` values) present in the task
/// set, sorted case-insensitively for a stable rail. Deterministic.
fn distinct_lists(tasks: &[Task]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for t in tasks {
        if let Some(name) = task_list_name(t) {
            if !names.iter().any(|n| n == &name) {
                names.push(name);
            }
        }
    }
    names.sort_by_key(|a| a.to_lowercase());
    names
}

/// Do any tasks have no list? (drives whether the No List bucket shows).
fn has_no_list_tasks(tasks: &[Task]) -> bool {
    tasks.iter().any(|t| task_list_name(t).is_none())
}

/// The Tasks section — an Apple-Reminders-style app. A left rail of smart lists
/// (Today / Scheduled / Flagged / All / Completed) plus the user's lists (task
/// `project` values) and a No List bucket; a main column with a quick-add row
/// and a tap-to-complete checklist. Row titles still open the shared
/// `EntityDetail` for full editing (unchanged). Client-side filtering over a
/// single `tasks_list` pull keeps it fold-neutral.
#[component]
fn TasksPane(store: ReadOnlySignal<Store>, selected: Signal<Option<Selected>>) -> Element {
    // Re-list whenever a row action or a detail save bumps the tick.
    let tick = use_signal(|| 0u32);
    // Which list is showing. Default = Today (Reminders' home).
    let sel = use_signal(|| TaskListSel::Today);
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
            style: "display: flex; height: 100%; min-height: 0; background: {BG};",
            match tasks() {
                None => rsx! {
                    div { style: "margin: 2rem auto;", {muted("loading tasks…")} }
                },
                Some(Err(e)) => rsx! {
                    div { style: "margin: 2rem auto;", {muted(&format!("tasks unavailable: {e}"))} }
                },
                Some(Ok(list)) => {
                    let today = today_ymd();
                    tasks_reminders_view(store, selected, tick, sel, list, today)
                }
            }
        }
    }
}

/// The two-column Reminders body: the sidebar rail + the main list column. A
/// plain fn (Task lacks PartialEq, so `list` can't ride a memoized prop).
fn tasks_reminders_view(
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    tick: Signal<u32>,
    sel: Signal<TaskListSel>,
    list: Vec<Task>,
    today: String,
) -> Element {
    // The rows for the currently-selected list, ordered for display.
    let current = sel();
    let mut rows: Vec<Task> = list
        .iter()
        .filter(|t| task_in_list(t, &current, &today))
        .cloned()
        .collect();
    rows = ordered_tasks(rows, &today);
    let header_count = rows.iter().filter(|t| task_is_open(t)).count();

    rsx! {
        // ── left rail ────────────────────────────────────────────────────────
        {tasks_sidebar(sel, &list, &today)}

        // ── main column ──────────────────────────────────────────────────────
        div {
            id: "tasks-main",
            style: "flex: 1; min-width: 0; height: 100%; overflow-y: auto; \
                    padding: 1.4rem 1.6rem 3rem;",
            // header: list name + open count
            div {
                style: "display: flex; align-items: baseline; gap: 0.6rem; margin-bottom: 0.2rem;",
                div {
                    id: "tasks-main-title",
                    style: "font-size: 1.6rem; font-weight: 800; color: {GOLD};",
                    "{current.title()}"
                }
                span {
                    id: "tasks-main-count",
                    style: "font-size: 1.1rem; font-weight: 700; color: {FAINT};",
                    "{header_count}"
                }
            }

            // quick-add row
            {tasks_quickadd(store, tick, sel)}

            // checklist
            if rows.is_empty() {
                div {
                    id: "tasks-empty",
                    style: "color: {FAINT}; font-size: 0.9rem; padding: 1.4rem 0.2rem; line-height: 1.6;",
                    {tasks_empty_hint(&current)}
                }
            } else {
                div {
                    id: "tasks-list",
                    style: "margin-top: 1rem;",
                    for t in rows.iter() {
                        {task_item(t, store, selected, tick, &today)}
                    }
                }
            }
        }
    }
}

/// The empty-state hint for a list with no matching tasks.
fn tasks_empty_hint(sel: &TaskListSel) -> &'static str {
    match sel {
        TaskListSel::Today => "Nothing due today. Add a task above, or check Scheduled.",
        TaskListSel::Scheduled => "No dated tasks yet. Give a task a due date to see it here.",
        TaskListSel::Flagged => "No flagged tasks. Flag one with the ! to surface it here.",
        TaskListSel::Completed => "Nothing completed yet.",
        _ => "No tasks in this list yet. Add one above.",
    }
}

/// The left rail: smart lists (with counts) then the user's lists + No List.
/// Plain fn (borrows the task slice). `sel` drives the main column.
fn tasks_sidebar(mut sel: Signal<TaskListSel>, tasks: &[Task], today: &str) -> Element {
    let smart = [
        ("tasks-smart-today", "Today", "◎", TaskListSel::Today),
        (
            "tasks-smart-scheduled",
            "Scheduled",
            "▤",
            TaskListSel::Scheduled,
        ),
        ("tasks-smart-flagged", "Flagged", "⚑", TaskListSel::Flagged),
        ("tasks-smart-all", "All", "≡", TaskListSel::All),
        (
            "tasks-smart-completed",
            "Completed",
            "✓",
            TaskListSel::Completed,
        ),
    ];
    let lists = distinct_lists(tasks);
    let show_no_list = has_no_list_tasks(tasks);
    let current = sel();

    rsx! {
        div {
            id: "tasks-sidebar",
            style: "width: 232px; flex: none; height: 100%; overflow-y: auto; \
                    border-right: 1px solid {EDGE}; background: {PANEL}; \
                    padding: 1.1rem 0.7rem 2rem;",

            // section title
            div {
                style: "font-size: 1.15rem; font-weight: 800; color: {INK}; margin: 0.1rem 0.4rem 0.7rem;",
                "Reminders"
            }

            // smart lists
            for (id, label, icon, kind) in smart.iter() {
                {tasks_rail_row(
                    id, label, icon,
                    task_list_count(tasks, kind, today),
                    current == *kind,
                    { let k = kind.clone(); move || sel.set(k.clone()) },
                )}
            }

            // user lists header (only when there are any)
            if !lists.is_empty() || show_no_list {
                div {
                    style: "font-size: 0.7rem; font-weight: 700; letter-spacing: 0.08em; \
                            text-transform: uppercase; color: {FAINT}; margin: 1rem 0.4rem 0.4rem;",
                    "My Lists"
                }
            }
            for name in lists.iter() {
                {
                    let kind = TaskListSel::List(name.clone());
                    let count = task_list_count(tasks, &kind, today);
                    let id = format!("tasks-list-{name}");
                    tasks_rail_row(
                        &id, name, "▸", count, current == kind,
                        { let k = kind.clone(); move || sel.set(k.clone()) },
                    )
                }
            }
            if show_no_list {
                {tasks_rail_row(
                    "tasks-list-none", "No List", "▸",
                    task_list_count(tasks, &TaskListSel::NoList, today),
                    current == TaskListSel::NoList,
                    move || sel.set(TaskListSel::NoList),
                )}
            }
        }
    }
}

/// One selectable rail row: icon, label, count; highlighted when active.
fn tasks_rail_row(
    id: &str,
    label: &str,
    icon: &str,
    count: usize,
    active: bool,
    on_select: impl FnMut() + 'static,
) -> Element {
    let mut on_select = on_select;
    let (bg, fg) = if active {
        (GOLD, "#14120e")
    } else {
        ("transparent", INK)
    };
    let count_color = if active { "#14120e" } else { FAINT };
    rsx! {
        button {
            id: "{id}",
            style: "display: flex; align-items: center; gap: 0.55rem; width: 100%; \
                    box-sizing: border-box; text-align: left; background: {bg}; color: {fg}; \
                    border: none; border-radius: 8px; padding: 0.42rem 0.55rem; \
                    font: inherit; font-size: 0.9rem; font-weight: 600; cursor: pointer; \
                    margin-bottom: 0.15rem;",
            onclick: move |_| on_select(),
            span { style: "width: 1.1rem; text-align: center; opacity: 0.85;", "{icon}" }
            span { style: "flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;", "{label}" }
            span { style: "font-size: 0.82rem; font-weight: 700; color: {count_color};", "{count}" }
        }
    }
}

/// The quick-add row: a title input, an optional native date, an optional
/// priority, and an Add button. Enter or Add creates the task into the selected
/// user list (or no list under a smart view). Plain fn — no memoized props.
fn tasks_quickadd(
    store: ReadOnlySignal<Store>,
    mut tick: Signal<u32>,
    sel: Signal<TaskListSel>,
) -> Element {
    let mut title = use_signal(String::new);
    let mut due = use_signal(String::new);
    let mut priority = use_signal(|| Priority::Normal.as_str().to_string());
    let mut error = use_signal(|| Option::<String>::None);

    // Add: build a TaskCreate and persist, then clear + re-pull. Empty title is
    // a no-op. `project` = the selected user list (None under a smart view).
    let mut submit = move || {
        let t = title().trim().to_string();
        if t.is_empty() {
            return; // empty title = no-op
        }
        let due_v = due().trim().to_string();
        let due_opt = if due_v.is_empty() { None } else { Some(due_v) };
        let prio = Priority::from_str_lossy(&priority());
        let project = match sel() {
            TaskListSel::List(p) => Some(p),
            _ => None, // smart view / No List → unlisted
        };
        let store = store();
        error.set(None);
        spawn(async move {
            let input = TaskCreate {
                title: t,
                body: String::new(),
                status: TaskStatus::Todo,
                priority: prio,
                due: due_opt,
                project,
                ..Default::default()
            };
            match store.tasks_create(input, "system").await {
                Ok(_) => {
                    title.set(String::new());
                    due.set(String::new());
                    tick += 1;
                }
                Err(e) => error.set(Some(format!("{e:#}"))),
            }
        });
    };

    rsx! {
        div {
            id: "tasks-quickadd",
            style: "display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; \
                    background: {PANEL}; border: 1px solid {EDGE}; border-radius: 10px; \
                    padding: 0.55rem 0.7rem; margin-top: 0.9rem;",
            span { style: "color: {GOLD}; font-size: 1.1rem; line-height: 1;", "＋" }
            input {
                id: "tasks-quickadd-input",
                style: "flex: 1; min-width: 160px; background: transparent; color: {INK}; \
                        border: none; outline: none; font: inherit; font-size: 0.95rem;",
                r#type: "text",
                placeholder: "Add a task",
                value: "{title}",
                oninput: move |e| title.set(e.value()),
                onkeydown: move |e| {
                    if e.key() == Key::Enter {
                        submit();
                    }
                },
            }
            input {
                id: "tasks-quickadd-due",
                style: "background: {BG}; color: {DIM}; border: 1px solid {EDGE}; border-radius: 7px; \
                        padding: 0.3rem 0.4rem; font: inherit; font-size: 0.8rem; cursor: pointer; \
                        color-scheme: dark;",
                r#type: "date",
                value: "{due}",
                oninput: move |e| due.set(e.value()),
            }
            select {
                id: "tasks-quickadd-priority",
                style: "background: {BG}; color: {DIM}; border: 1px solid {EDGE}; border-radius: 7px; \
                        padding: 0.32rem 0.4rem; font: inherit; font-size: 0.8rem; cursor: pointer;",
                value: "{priority}",
                onchange: move |e| priority.set(e.value()),
                for p in PRIORITIES.iter() {
                    option { value: "{p.as_str()}", "{priority_label(*p)}" }
                }
            }
            button {
                id: "tasks-quickadd-submit",
                style: "background: {GOLD}; color: #14120e; border: none; border-radius: 7px; \
                        padding: 0.4rem 0.9rem; font: inherit; font-weight: 700; font-size: 0.85rem; \
                        cursor: pointer;",
                onclick: move |_| submit(),
                "Add"
            }
            if let Some(e) = error() {
                div {
                    id: "tasks-quickadd-error",
                    style: "flex-basis: 100%; color: #e07a5f; font-size: 0.8rem; margin-top: 0.2rem;",
                    "{e}"
                }
            }
        }
    }
}

/// A short human label for a priority in the quick-add dropdown.
fn priority_label(p: Priority) -> &'static str {
    match p {
        Priority::Low => "Low",
        Priority::Normal => "Normal",
        Priority::High => "High",
        Priority::Urgent => "Urgent",
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

/// A red-ish palette const for overdue accents (matches the app's error red).
const OVERDUE: &str = "#e07a5f";

/// One checklist row: a tap-to-complete circle, the title (opens the detail
/// view), a due chip, a priority flag, and a status chip for Doing/Blocked.
/// Plain fn — Task has no PartialEq, and each control clones its own id before
/// moving it into a closure (never borrowing `t` past a move — the release
/// borrow-check trap).
fn task_item(
    t: &Task,
    store: ReadOnlySignal<Store>,
    mut selected: Signal<Option<Selected>>,
    mut tick: Signal<u32>,
    today: &str,
) -> Element {
    let done = t.status == TaskStatus::Done;
    let due = t.due.clone().filter(|d| !d.trim().is_empty());
    let bucket = due_bucket(t.due.as_deref(), today);
    let due_text = due.as_deref().map(|d| due_label(d, today));
    let due_color = match bucket {
        DueBucket::Overdue => OVERDUE,
        DueBucket::Today => GOLD,
        _ => DIM,
    };
    let current = t.status;
    let priority = t.priority;
    let show_flag = priority_rank(priority) >= priority_rank(Priority::High);
    let flag_glyph = if priority == Priority::Urgent {
        "‼"
    } else {
        "!"
    };
    let show_status_chip = matches!(current, TaskStatus::Doing | TaskStatus::Blocked);
    let assignee = t.assignees.first().cloned();

    // Circle: toggle Done ⇄ Todo.
    let id_circle = t.id.clone();
    let toggle_done = move |_: MouseEvent| {
        let id = id_circle.clone();
        let store = store();
        let next = if done {
            TaskStatus::Todo
        } else {
            TaskStatus::Done
        };
        spawn(async move {
            let patch = TaskPatch {
                status: Some(next),
                ..Default::default()
            };
            let _ = store.tasks_update(&id, patch, "system").await;
            tick += 1;
        });
    };

    // Title: open the shared EntityDetail (full edit) — do NOT fork it.
    let id_title = t.id.clone();
    let open_detail = move |_: MouseEvent| {
        selected.set(Some(Selected::Task(id_title.clone())));
    };

    // Flag: cycle Normal → High → Urgent → Normal.
    let id_flag = t.id.clone();
    let cycle_flag = move |_: MouseEvent| {
        let id = id_flag.clone();
        let store = store();
        let next = match priority {
            Priority::Normal => Priority::High,
            Priority::High => Priority::Urgent,
            Priority::Urgent => Priority::Normal,
            Priority::Low => Priority::High,
        };
        spawn(async move {
            let patch = TaskPatch {
                priority: Some(next),
                ..Default::default()
            };
            let _ = store.tasks_update(&id, patch, "system").await;
            tick += 1;
        });
    };

    let title_style = if done {
        format!("color: {DIM}; text-decoration: line-through;")
    } else {
        format!("color: {INK};")
    };
    // Precomputed (rsx format strings and attributes can't hold an `if`).
    let circle_color = if done { GOLD } else { FAINT };
    let circle_glyph = if done { "⦿" } else { "○" };
    let circle_title = if done {
        "Mark as not done"
    } else {
        "Mark as done"
    };

    rsx! {
        div {
            id: "task-item-{t.id}",
            style: "display: flex; align-items: flex-start; gap: 0.7rem; \
                    padding: 0.6rem 0.3rem; border-bottom: 1px solid {EDGE};",

            // tap-to-complete circle
            button {
                id: "task-check-{t.id}",
                style: "flex: none; background: none; border: none; cursor: pointer; padding: 0; \
                        font-size: 1.15rem; line-height: 1.3; color: {circle_color};",
                title: "{circle_title}",
                onclick: toggle_done,
                "{circle_glyph}"
            }

            // title + meta
            div {
                style: "flex: 1; min-width: 0;",
                button {
                    id: "task-title-{t.id}",
                    style: "display: block; width: 100%; text-align: left; background: none; \
                            border: none; font: inherit; font-size: 0.95rem; font-weight: 600; \
                            cursor: pointer; padding: 0; {title_style}",
                    onclick: open_detail,
                    "{t.title}"
                }
                // chips row: due, status, assignee
                if due_text.is_some() || show_status_chip || assignee.is_some() {
                    div {
                        style: "display: flex; flex-wrap: wrap; align-items: center; gap: 0.4rem; margin-top: 0.3rem;",
                        if let Some(txt) = due_text.clone() {
                            span {
                                id: "task-due-{t.id}",
                                style: "font-size: 0.74rem; font-weight: 600; color: {due_color}; \
                                        border: 1px solid {due_color}; border-radius: 999px; padding: 0.05rem 0.5rem;",
                                "{txt}"
                            }
                        }
                        if show_status_chip {
                            {task_status_chip(t.id.clone(), current, store, tick)}
                        }
                        if let Some(a) = assignee {
                            span { style: "font-size: 0.74rem; color: {FAINT};", "· {a}" }
                        }
                    }
                }
            }

            // priority flag (High/Urgent)
            if show_flag {
                button {
                    id: "task-flag-{t.id}",
                    style: "flex: none; background: none; border: none; cursor: pointer; padding: 0.1rem 0.2rem; \
                            font-size: 0.95rem; font-weight: 800; color: {OVERDUE};",
                    title: "Cycle priority (High → Urgent → Normal)",
                    onclick: cycle_flag,
                    "{flag_glyph}"
                }
            } else {
                // A faint outline flag to RAISE priority from Normal/Low.
                button {
                    id: "task-flag-{t.id}",
                    style: "flex: none; background: none; border: none; cursor: pointer; padding: 0.1rem 0.2rem; \
                            font-size: 0.95rem; color: {FAINT}; opacity: 0.5;",
                    title: "Flag (raise priority)",
                    onclick: cycle_flag,
                    "⚐"
                }
            }
        }
    }
}

/// The small status chip shown when a task is Doing or Blocked (states
/// Reminders lacks but hive keeps). It's a select so a click sets any status —
/// nothing is lost. Plain fn; clones its id into the closure.
fn task_status_chip(
    id: String,
    current: TaskStatus,
    store: ReadOnlySignal<Store>,
    mut tick: Signal<u32>,
) -> Element {
    let color = match current {
        TaskStatus::Blocked => OVERDUE,
        _ => GOLD, // Doing
    };
    rsx! {
        select {
            id: "task-status-{id}",
            style: "font: inherit; font-size: 0.72rem; font-weight: 700; color: {color}; \
                    background: {BG}; border: 1px solid {color}; border-radius: 999px; \
                    padding: 0.05rem 0.4rem; cursor: pointer;",
            value: "{current.as_str()}",
            onchange: move |e| {
                let want = e.value();
                let id = id.clone();
                let store = store();
                spawn(async move {
                    if let Some(status) = TaskStatus::parse(&want) {
                        let patch = TaskPatch { status: Some(status), ..Default::default() };
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
///
/// `embedded` = true when this renders INSIDE the Apple-Contacts card (which
/// supplies its own avatar/name header + Back), so the view drops its own back
/// button and name/kind chip to avoid a doubled header; the field editor,
/// add-field affordance, and backlinks are unchanged. The full-pane task/event
/// takeover passes false.
#[component]
fn EntityDetail(
    store: ReadOnlySignal<Store>,
    selected: Signal<Option<Selected>>,
    refresh: Signal<u32>,
    target: Selected,
    embedded: bool,
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

    // Embedded in the contact card: no outer max-width/padding (the card owns
    // the frame) and no own back/name header. Standalone: the full detail frame.
    let outer_style = if embedded {
        "margin: 0;".to_string()
    } else {
        "max-width: 760px; margin: 0 auto; padding: 1.4rem 1.2rem 3rem;".to_string()
    };

    rsx! {
        div {
            id: "entity-detail",
            style: "{outer_style}",

            // back to the list (standalone takeover only; the card supplies its own)
            if !embedded {
                button {
                    id: "detail-back",
                    style: "background: none; border: none; color: {GOLD}; font: inherit; \
                            font-size: 0.85rem; cursor: pointer; padding: 0; margin-bottom: 0.9rem;",
                    onclick: move |_| selected.set(None),
                    "← Back"
                }
            }

            match data() {
                None => muted("loading…"),
                Some(Err(e)) => muted(&format!("couldn't open this: {e}")),
                Some(Ok(d)) => rsx! {
                    // header: display name + kind chip (standalone only — the
                    // contact card renders its own avatar + name header).
                    if !embedded {
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
                    }

                    // fields. For a contact, `favorite`/`groups` are surfaced
                    // as the star toggle + group chips in the card header
                    // (ContactCard), so they are hidden from the raw editor here.
                    div {
                        style: "background: {PANEL}; border: 1px solid {EDGE}; border-radius: 12px; \
                                padding: 1rem 1.1rem; margin: 1rem 0;",
                        for spec in d.specs.iter().filter(|f| {
                            !(f.archived
                                || is_contact
                                    && matches!(f.slug.as_str(), "favorite" | "groups"))
                        }) {
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
            // its own field row only if present. Persist the working field
            // values as a merge patch (present keys set, absent keys left as-is
            // by the fold). `favorite`/`groups` are OWNED by the card header's
            // star + chips (which write them instantly via their own path), so
            // they are stripped here — Save must never clobber them with a
            // possibly-stale working-copy value.
            let mut fields = values.clone();
            fields.remove("favorite");
            fields.remove("groups");
            let patch = CustomEntityPatch {
                title: None,
                fields: Some(fields),
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
            // The detail form edits `due` (date field): a value sets it, a
            // blank clears it (Some(None) → SQL NULL). `project` is owned by the
            // Reminders rail (moving lists happens there), so detail Save leaves
            // it untouched (None = keep).
            let due_raw = get("due");
            let due = if due_raw.trim().is_empty() {
                Some(None)
            } else {
                Some(Some(due_raw.trim().to_string()))
            };
            let patch = TaskPatch {
                title: Some(get("title")),
                body: Some(get("body")),
                status: TaskStatus::parse(&get("status")),
                priority: Some(Priority::from_str_lossy(&get("priority"))),
                assignees: Some(assignees),
                tags: None,
                project: None,
                due,
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
    // Independent of the settings Save: the Accounts card's add/toggle/resync/
    // delete each write immediately and bump THIS to re-pull the account list.
    let accounts_refresh = use_signal(|| 0u32);

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

            // ── Accounts ──
            // The same mail-account add form + connected list as the Mail gear,
            // reached here too. Its ops are immediate — no dependence on Save.
            div {
                id: "settings-accounts",
                style: settings_card_style(),
                div { style: "font-weight: 700; font-size: 1.02rem;", "Accounts" }
                div {
                    style: "color: {DIM}; font-size: 0.86rem; line-height: 1.55; margin-top: 0.25rem;",
                    "Mailboxes connected to this hive. Each belongs to an identity and syncs on its own."
                }
                MailAccountsPanel { store, refresh: accounts_refresh }
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

/// The small outlined pill the reader's Reply / Reply-all buttons share.
fn reply_button_style() -> String {
    format!(
        "background: none; border: 1px solid {EDGE}; color: {INK}; border-radius: 8px; \
         padding: 0.35rem 0.8rem; font: inherit; font-size: 0.82rem; font-weight: 600; \
         cursor: pointer;"
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
    use super::{
        body_looks_like_html, render_email_body, render_markdown, render_plaintext_email,
        sanitize_email_html,
    };

    /// THE SECURITY CONTRACT. Mail bodies are untrusted, attacker-controlled
    /// markup rendered in the app's WebKit — this hostile corpus proves the
    /// sanitizer neutralizes every classic vector, and that the render makes no
    /// network request by default.
    #[test]
    fn sanitize_email_html_neutralizes_hostile_corpus() {
        let hostile = "\
            <script>alert('xss')</script>\
            <style>body{background:url('https://evil.example/leak.png')}</style>\
            <img src=\"https://evil.example/pixel.gif\" width=\"1\" height=\"1\">\
            <img src=\"x\" onerror=\"alert(document.cookie)\">\
            <a href=\"javascript:alert(1)\">tap</a>\
            <a href=\"data:text/html,<script>alert(1)</script>\">data</a>\
            <iframe src=\"https://evil.example/frame\"></iframe>\
            <object data=\"https://evil.example/o\"></object>\
            <embed src=\"https://evil.example/e\">\
            <form action=\"https://evil.example/steal\"><input name=\"pw\"></form>\
            <div style=\"background:url('https://evil.example/css.png')\">hi</div>\
            <svg onload=\"alert(1)\"></svg>\
            <p onclick=\"steal()\">click me</p>";
        let out = sanitize_email_html(hostile, false);

        // Script tag + payload gone (contents removed, not just unwrapped).
        assert!(!out.contains("<script"), "script tag survived: {out}");
        assert!(!out.contains("alert('xss')"), "script body survived: {out}");
        // Style element + its CSS url() gone (no <style>, no exfil fetch).
        assert!(!out.contains("<style"), "style element survived: {out}");
        // Framing / plugin / form / input elements all dropped.
        for tag in ["<iframe", "<object", "<embed", "<form", "<input", "<svg"] {
            assert!(!out.contains(tag), "{tag} survived: {out}");
        }
        // No remote image fetch at all by default (tracking-pixel defense).
        assert!(
            !out.contains("<img"),
            "img survived while remote OFF: {out}"
        );
        assert!(
            !out.contains("evil.example"),
            "a remote URL leaked into the output: {out}"
        );
        // No event handlers anywhere.
        for handler in ["onerror", "onclick", "onload"] {
            assert!(!out.contains(handler), "{handler} survived: {out}");
        }
        // No dangerous URL schemes in any attribute.
        assert!(!out.contains("javascript:"), "javascript: survived: {out}");
        assert!(!out.contains("data:text/html"), "data: URI survived: {out}");
        // No inline style attribute (CSS url() / expression() vector) survives.
        assert!(!out.contains("style="), "inline style survived: {out}");
    }

    /// A benign, formatted email SURVIVES sanitization: structure, emphasis, and
    /// a safe link are kept, and the safe link is made inert (external).
    #[test]
    fn sanitize_email_html_keeps_benign_formatting() {
        let benign = "<p>Hi <strong>Nate</strong>,</p><ul><li>one</li><li>two</li></ul>\
                      <p>See <a href=\"https://example.com/report\">the report</a>.</p>";
        let out = sanitize_email_html(benign, false);
        assert!(out.contains("<strong>"), "emphasis dropped: {out}");
        assert!(out.contains("<li>"), "list dropped: {out}");
        assert!(
            out.contains("https://example.com/report"),
            "safe link dropped: {out}"
        );
        assert!(out.contains("noopener"), "safe link not made inert: {out}");
        assert!(
            out.contains("target=\"_blank\"") || out.contains("target=_blank"),
            "link not forced external: {out}"
        );
    }

    /// The remote-image opt-in: a tracking pixel is blocked by default and only
    /// an http(s) `<img src>` returns once the user explicitly allows it —
    /// `srcset` and inline data: images never come back.
    #[test]
    fn sanitize_email_html_remote_image_optin() {
        let with_img =
            "<img src=\"https://cdn.example/logo.png\" srcset=\"https://cdn.example/2x.png 2x\">\
                        <img src=\"data:image/png;base64,AAAA\">";
        // Default OFF: nothing loads.
        let off = sanitize_email_html(with_img, false);
        assert!(!off.contains("<img"), "image shown while remote OFF: {off}");
        assert!(!off.contains("cdn.example"), "remote url leaked OFF: {off}");
        // Opt-in ON: the http(s) image returns; srcset + data: image do not.
        let on = sanitize_email_html(with_img, true);
        assert!(
            on.contains("https://cdn.example/logo.png"),
            "opted-in image missing: {on}"
        );
        assert!(!on.contains("srcset"), "srcset came back ON: {on}");
        assert!(!on.contains("data:image"), "data: image came back ON: {on}");
    }

    /// Plaintext bodies are HTML-escaped (so tag-shaped text is inert) and bare
    /// URLs are linkified into inert external links — with no auto-loading.
    #[test]
    fn plaintext_email_escapes_and_linkifies() {
        let body = "Watch out for <script>alert(1)</script> and see https://example.com/x, thanks.";
        let out = render_plaintext_email(body);
        // The tag-shaped text is escaped, never live markup.
        assert!(!out.contains("<script"), "plaintext tag went live: {out}");
        assert!(out.contains("&lt;script&gt;"), "not escaped: {out}");
        // The bare URL is linkified and inert; trailing comma is not swallowed.
        assert!(
            out.contains("href=\"https://example.com/x\""),
            "url not linkified: {out}"
        );
        assert!(out.contains("noopener"), "linkified url not inert: {out}");
        assert!(out.contains("</a>,"), "trailing comma swallowed: {out}");
    }

    /// The HTML sniff + dispatcher: tag-bearing bodies route through the
    /// sanitizer, plain ones through the escaping renderer.
    #[test]
    fn body_dispatch_routes_html_vs_plaintext() {
        assert!(body_looks_like_html("<p>hello</p>"));
        assert!(!body_looks_like_html("just some plain text, no tags"));
        // A plaintext body with a stray '<' is treated as text and escaped.
        let out = render_email_body("2 < 3 and 4 > 1", false);
        assert!(out.contains("2 &lt; 3"), "plaintext not escaped: {out}");
        // An HTML body with a hostile handler is sanitized.
        let out = render_email_body("<img src=x onerror=alert(1)>", false);
        assert!(!out.contains("onerror"), "handler survived dispatch: {out}");
        assert!(!out.contains("<img"), "remote img survived dispatch: {out}");
    }

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

    // ── task due helpers + Reminders smart-view rules ──

    /// A tiny task builder for the pure-fn tests (Task has no Default).
    fn mk_task(
        id: &str,
        status: hive_shared::TaskStatus,
        priority: hive_shared::Priority,
        project: Option<&str>,
        due: Option<&str>,
    ) -> hive_shared::Task {
        hive_shared::Task {
            id: id.into(),
            title: id.into(),
            body: String::new(),
            status,
            priority,
            tags: Vec::new(),
            assignees: Vec::new(),
            project: project.map(str::to_string),
            phase: None,
            due: due.map(str::to_string),
            origin_entry_id: None,
            anchor_text: None,
            created_at: String::new(),
            updated_at: id.into(),
        }
    }

    /// task_due_day accepts the same shapes event_day does (bare date and
    /// datetime-ish) and rejects garbage, so garbage reads as undated.
    #[test]
    fn task_due_day_parses_dates_and_datetimes() {
        use super::task_due_day;
        assert_eq!(task_due_day("2026-07-15"), Some((2026, 7, 15)));
        assert_eq!(task_due_day("2026-08-01T09:30:00.000Z"), Some((2026, 8, 1)));
        assert_eq!(task_due_day("  2026-12-31 23:59  "), Some((2026, 12, 31)));
        // Garbage / vague → None (lands undated, never a wrong day).
        assert_eq!(task_due_day(""), None);
        assert_eq!(task_due_day("next Tuesday"), None);
        assert_eq!(task_due_day("2026-13-01"), None);
        assert_eq!(task_due_day("2026-02-30"), None);
    }

    /// due_bucket classifies Overdue / Today / Future / Undated by tuple
    /// comparison, and is deterministic (no clock).
    #[test]
    fn due_bucket_classifies_relative_to_today() {
        use super::{due_bucket, DueBucket};
        let today = "2026-07-15";
        assert_eq!(due_bucket(Some("2026-07-14"), today), DueBucket::Overdue);
        assert_eq!(due_bucket(Some("2026-07-15"), today), DueBucket::Today);
        assert_eq!(due_bucket(Some("2026-07-16"), today), DueBucket::Future);
        // A datetime today still buckets as Today (day-granular).
        assert_eq!(
            due_bucket(Some("2026-07-15T23:00:00.000Z"), today),
            DueBucket::Today
        );
        // Absent or garbage → Undated.
        assert_eq!(due_bucket(None, today), DueBucket::Undated);
        assert_eq!(due_bucket(Some("whenever"), today), DueBucket::Undated);
        // Determinism: same inputs, same output.
        assert_eq!(
            due_bucket(Some("2026-07-14"), today),
            due_bucket(Some("2026-07-14"), today)
        );
    }

    /// due_label reads as Overdue / Today / Tomorrow / "Mon D" (with a year when
    /// it differs), and falls back to raw text for an unparseable due.
    #[test]
    fn due_label_reads_relative_then_short_date() {
        use super::due_label;
        let today = "2026-07-15";
        assert_eq!(due_label("2026-07-10", today), "Overdue");
        assert_eq!(due_label("2026-07-15", today), "Today");
        assert_eq!(due_label("2026-07-16", today), "Tomorrow");
        assert_eq!(due_label("2026-07-20", today), "Jul 20");
        // A different year carries the year.
        assert_eq!(due_label("2027-01-03", today), "Jan 3 2027");
        // Unparseable due → its trimmed raw text (never dropped).
        assert_eq!(due_label("  someday  ", today), "someday");
    }

    /// The smart-view membership rules: Today = open & (overdue|today);
    /// Scheduled = open & dated; Flagged = open & ≥High; All = open;
    /// Completed = done; a user list = its project; No List = projectless.
    #[test]
    fn task_in_list_smart_and_user_rules() {
        use super::{task_in_list, TaskListSel};
        use hive_shared::{Priority, TaskStatus};
        let today = "2026-07-15";

        let overdue = mk_task(
            "a",
            TaskStatus::Todo,
            Priority::Normal,
            None,
            Some("2026-07-01"),
        );
        let due_today = mk_task(
            "b",
            TaskStatus::Todo,
            Priority::High,
            Some("Work"),
            Some("2026-07-15"),
        );
        let future = mk_task(
            "c",
            TaskStatus::Todo,
            Priority::Urgent,
            Some("Work"),
            Some("2026-08-01"),
        );
        let undated = mk_task("d", TaskStatus::Doing, Priority::Normal, None, None);
        let done = mk_task(
            "e",
            TaskStatus::Done,
            Priority::Normal,
            Some("Work"),
            Some("2026-07-01"),
        );

        // Today: overdue + due-today open tasks; not future/undated/done.
        assert!(task_in_list(&overdue, &TaskListSel::Today, today));
        assert!(task_in_list(&due_today, &TaskListSel::Today, today));
        assert!(!task_in_list(&future, &TaskListSel::Today, today));
        assert!(!task_in_list(&undated, &TaskListSel::Today, today));
        assert!(!task_in_list(&done, &TaskListSel::Today, today));

        // Scheduled: any open dated task; not the undated one, not done.
        assert!(task_in_list(&overdue, &TaskListSel::Scheduled, today));
        assert!(task_in_list(&future, &TaskListSel::Scheduled, today));
        assert!(!task_in_list(&undated, &TaskListSel::Scheduled, today));
        assert!(!task_in_list(&done, &TaskListSel::Scheduled, today));

        // Flagged: open & priority ≥ High.
        assert!(task_in_list(&due_today, &TaskListSel::Flagged, today)); // High
        assert!(task_in_list(&future, &TaskListSel::Flagged, today)); // Urgent
        assert!(!task_in_list(&overdue, &TaskListSel::Flagged, today)); // Normal

        // All: every open task; never a done one.
        assert!(task_in_list(&overdue, &TaskListSel::All, today));
        assert!(task_in_list(&undated, &TaskListSel::All, today));
        assert!(!task_in_list(&done, &TaskListSel::All, today));

        // Completed: only the done task.
        assert!(task_in_list(&done, &TaskListSel::Completed, today));
        assert!(!task_in_list(&overdue, &TaskListSel::Completed, today));

        // User list "Work": tasks with that project (open + completed).
        let work = TaskListSel::List("Work".into());
        assert!(task_in_list(&due_today, &work, today));
        assert!(task_in_list(&done, &work, today));
        assert!(!task_in_list(&overdue, &work, today)); // no project

        // No List: only projectless tasks.
        assert!(task_in_list(&overdue, &TaskListSel::NoList, today));
        assert!(task_in_list(&undated, &TaskListSel::NoList, today));
        assert!(!task_in_list(&due_today, &TaskListSel::NoList, today));
    }

    /// ordered_tasks: open before completed; within open, due asc (undated
    /// last) then priority desc; completed by most-recently-updated.
    #[test]
    fn ordered_tasks_open_first_then_due_then_priority() {
        use super::ordered_tasks;
        use hive_shared::{Priority, TaskStatus};
        let today = "2026-07-15";

        let a_undated_urgent = mk_task("a", TaskStatus::Todo, Priority::Urgent, None, None);
        let b_soon = mk_task(
            "b",
            TaskStatus::Todo,
            Priority::Low,
            None,
            Some("2026-07-16"),
        );
        let c_later = mk_task(
            "c",
            TaskStatus::Todo,
            Priority::High,
            None,
            Some("2026-07-20"),
        );
        let d_done = mk_task(
            "d",
            TaskStatus::Done,
            Priority::High,
            None,
            Some("2026-07-01"),
        );

        let ordered = ordered_tasks(
            vec![
                d_done.clone(),
                a_undated_urgent.clone(),
                c_later.clone(),
                b_soon.clone(),
            ],
            today,
        );
        let ids: Vec<&str> = ordered.iter().map(|t| t.id.as_str()).collect();
        // Dated open tasks come first in due order, then the undated open
        // (urgent) task, then the completed one last.
        assert_eq!(ids, vec!["b", "c", "a", "d"]);
    }

    /// distinct_lists dedupes non-empty projects (case-insensitive sort) and
    /// has_no_list_tasks detects projectless tasks.
    #[test]
    fn distinct_lists_and_no_list_detection() {
        use super::{distinct_lists, has_no_list_tasks};
        use hive_shared::{Priority, TaskStatus};
        let tasks = vec![
            mk_task("a", TaskStatus::Todo, Priority::Normal, Some("Zeta"), None),
            mk_task("b", TaskStatus::Todo, Priority::Normal, Some("alpha"), None),
            mk_task("c", TaskStatus::Todo, Priority::Normal, Some("Zeta"), None),
            mk_task("d", TaskStatus::Todo, Priority::Normal, Some("   "), None), // blank → No List
            mk_task("e", TaskStatus::Todo, Priority::Normal, None, None),
        ];
        assert_eq!(distinct_lists(&tasks), vec!["alpha", "Zeta"]);
        assert!(has_no_list_tasks(&tasks), "blank + none count as No List");

        let all_listed = vec![mk_task(
            "x",
            TaskStatus::Todo,
            Priority::Normal,
            Some("A"),
            None,
        )];
        assert!(!has_no_list_tasks(&all_listed));
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

    // ── Apple-Calendar day/week/day-view helpers ──

    /// step_day advances/retreats one calendar day, wrapping month and year
    /// ends over the right month lengths (incl. leap February).
    #[test]
    fn step_day_wraps_months_and_years() {
        use super::step_day;
        // Ordinary mid-month steps.
        assert_eq!(step_day(2026, 7, 12, true), (2026, 7, 13));
        assert_eq!(step_day(2026, 7, 12, false), (2026, 7, 11));
        // Month rollover forward (July has 31 days) and back.
        assert_eq!(step_day(2026, 7, 31, true), (2026, 8, 1));
        assert_eq!(step_day(2026, 8, 1, false), (2026, 7, 31));
        // Year rollover both ways.
        assert_eq!(step_day(2026, 12, 31, true), (2027, 1, 1));
        assert_eq!(step_day(2026, 1, 1, false), (2025, 12, 31));
        // February lengths: leap 2024 has 29, non-leap 2026 has 28.
        assert_eq!(step_day(2024, 2, 28, true), (2024, 2, 29));
        assert_eq!(step_day(2024, 2, 29, true), (2024, 3, 1));
        assert_eq!(step_day(2026, 2, 28, true), (2026, 3, 1));
        assert_eq!(step_day(2026, 3, 1, false), (2026, 2, 28));
    }

    /// The week's Sunday is the date stepped back by its weekday; a Sunday is
    /// its own week start; the seven days run Sun→Sat and wrap month/year ends.
    #[test]
    fn week_sunday_and_days_span_the_week() {
        use super::{week_days, week_start_sunday, weekday};
        // 2026-07-12 is a Sunday (weekday 0), so it starts its own week.
        assert_eq!(weekday(2026, 7, 12), 0);
        assert_eq!(week_start_sunday(2026, 7, 12), (2026, 7, 12));
        // 2026-07-15 is a Wednesday; its week's Sunday is the 12th.
        assert_eq!(week_start_sunday(2026, 7, 15), (2026, 7, 12));
        let days = week_days(2026, 7, 15);
        assert_eq!(days.len(), 7);
        assert_eq!(days[0], (2026, 7, 12), "first column is Sunday");
        assert_eq!(days[6], (2026, 7, 18), "last column is Saturday");
        // A week straddling a month boundary wraps correctly. 2026-08-01 is a
        // Saturday, so its week's Sunday is 2026-07-26.
        assert_eq!(weekday(2026, 8, 1), 6);
        let cross = week_days(2026, 8, 1);
        assert_eq!(cross[0], (2026, 7, 26));
        assert_eq!(cross[6], (2026, 8, 1));
    }

    /// The contextual labels: week range collapses a shared month/year and
    /// spells both out across a boundary; the day label names the weekday.
    #[test]
    fn week_and_day_labels_read_naturally() {
        use super::{day_label, week_range_label};
        // Same month/year: "Jul 12–18, 2026" (week of Sunday the 12th).
        assert_eq!(week_range_label(2026, 7, 15), "Jul 12–18, 2026");
        // Crossing a month within a year: week of Sunday 2026-07-26 → Aug 1.
        assert_eq!(week_range_label(2026, 8, 1), "Jul 26 – Aug 1, 2026");
        // Crossing a year end: week of Sunday 2025-12-28 → Jan 3 2026.
        assert_eq!(week_range_label(2025, 12, 31), "Dec 28, 2025 – Jan 3, 2026");
        // Day label spells the full weekday. 2026-07-12 is a Sunday.
        assert_eq!(day_label(2026, 7, 12), "Sunday, Jul 12, 2026");
    }

    /// event_hour buckets a timed event by its start hour and yields None for an
    /// untimed (all-day) event, matching the Day timeline's partition.
    #[test]
    fn event_hour_buckets_by_start_hour() {
        use super::event_hour;
        assert_eq!(event_hour("2026-07-12T09:30:00.000Z"), Some(9));
        assert_eq!(event_hour("2026-07-12 00:15"), Some(0));
        assert_eq!(event_hour("2026-07-12T23:59"), Some(23));
        assert_eq!(event_hour("2026-07-12"), None, "bare date is untimed");
        assert_eq!(event_hour("next week"), None);
    }

    /// event_color is deterministic (same event → same hue everywhere), keys off
    /// the first tag when present else the title, and always returns a palette
    /// member.
    #[test]
    fn event_color_is_deterministic_and_in_palette() {
        use super::event_color;
        use hive_shared::EventItem;

        let ev = |title: &str, tags: &[&str]| EventItem {
            id: "x".into(),
            title: title.into(),
            body: String::new(),
            at: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            assignees: Vec::new(),
            origin_entry_id: None,
            anchor_text: None,
            created_at: String::new(),
        };

        // Same inputs → same color.
        assert_eq!(
            event_color(&ev("Dentist", &[])),
            event_color(&ev("Dentist", &[]))
        );
        // Always a 7-char hex palette entry.
        let c = event_color(&ev("anything", &[]));
        assert!(c.starts_with('#') && c.len() == 7);
        // Tag drives the color when present: two differently-titled events that
        // share their first tag get the SAME color (so a "work" tag tints alike),
        // and it matches coloring by that tag's text as the title.
        assert_eq!(
            event_color(&ev("Standup", &["work"])),
            event_color(&ev("Review", &["work"]))
        );
        assert_eq!(
            event_color(&ev("Standup", &["work"])),
            event_color(&ev("work", &[]))
        );
        // A blank first tag falls through to the title.
        assert_eq!(
            event_color(&ev("Solo", &["  "])),
            event_color(&ev("Solo", &[]))
        );
    }

    // ── Apple-Contacts helpers (favorites, groups, avatars, A–Z) ──

    /// Group parsing splits on commas, trims, drops blanks, and dedupes
    /// case-insensitively in first-seen order; join round-trips the result.
    #[test]
    fn parse_and_join_groups_normalize() {
        use super::{join_groups, parse_groups};
        assert_eq!(
            parse_groups("Family, Work"),
            vec!["Family".to_string(), "Work".to_string()]
        );
        // Blanks, extra whitespace, and trailing commas are cleaned.
        assert_eq!(
            parse_groups("  Family ,, ,Work,  "),
            vec!["Family".to_string(), "Work".to_string()]
        );
        // Case-insensitive dedupe keeps the first spelling.
        assert_eq!(
            parse_groups("Family, family, FAMILY"),
            vec!["Family".to_string()]
        );
        assert!(parse_groups("").is_empty());
        assert!(parse_groups("   ,  , ").is_empty());
        // Round-trip through the stored form.
        let g = vec!["Family".to_string(), "Work".to_string()];
        assert_eq!(join_groups(&g), "Family, Work");
        assert_eq!(parse_groups(&join_groups(&g)), g);
    }

    /// Group slugs (left-rail row ids) are lowercased, hyphen-collapsed, trimmed.
    #[test]
    fn group_slug_is_stable_id() {
        use super::group_slug;
        assert_eq!(group_slug("Family"), "family");
        assert_eq!(group_slug("Close Friends"), "close-friends");
        assert_eq!(group_slug("  Work / Team!!  "), "work-team");
        assert_eq!(group_slug("A&B  C"), "a-b-c");
    }

    /// Avatar initials: first + last word letters, uppercased; single word →
    /// one letter; a letterless name → "#" (matching the "#" bucket).
    #[test]
    fn avatar_initials_first_and_last() {
        use super::avatar_initials;
        assert_eq!(avatar_initials("Jane Doe"), "JD");
        assert_eq!(avatar_initials("jane van doe"), "JD"); // first + last only
        assert_eq!(avatar_initials("Cher"), "C");
        assert_eq!(avatar_initials("  ada   lovelace "), "AL");
        assert_eq!(avatar_initials(""), "#");
        assert_eq!(avatar_initials("(unnamed contact)"), "UC"); // '(' skipped
        assert_eq!(avatar_initials("!!!"), "#");
    }

    /// Avatar color is deterministic (same name → same hue) and always a
    /// palette member.
    #[test]
    fn avatar_color_is_deterministic() {
        use super::avatar_color;
        assert_eq!(avatar_color("Jane Doe"), avatar_color("Jane Doe"));
        // Case-insensitive: the same person keeps their color if titled differently.
        assert_eq!(avatar_color("Jane Doe"), avatar_color("jane doe"));
        // Always starts with a hex marker (a real palette entry).
        assert!(avatar_color("anyone").starts_with('#'));
        assert_eq!(avatar_color("anyone").len(), 7);
    }

    /// A–Z bucketing: leading ASCII letter uppercased, else "#"; and the sort
    /// key sinks letterless names below alphabetical ones.
    #[test]
    fn section_and_sort_bucket_correctly() {
        use super::{contact_sort_key, section_letter};
        assert_eq!(section_letter("Ada"), "A");
        assert_eq!(section_letter("zeb"), "Z");
        assert_eq!(section_letter("42 Jump St"), "#");
        assert_eq!(section_letter("  "), "#");
        assert_eq!(section_letter("(unnamed contact)"), "#");

        // Sort: case-insensitive; letterless (bucket 1) sorts after letters.
        let mut names = vec!["zoe", "Ada", "9lives", "bob", "(x)"];
        names.sort_by_key(|a| contact_sort_key(a));
        assert_eq!(names, vec!["Ada", "bob", "zoe", "(x)", "9lives"]);
    }

    /// The favorite predicate reads the Bool field strictly; the group filter
    /// and search compose over a contact's fields.
    #[test]
    fn favorite_filter_and_search_read_fields() {
        use super::{contact_matches_search, ContactFilter};
        use hive_shared::CustomEntity;
        use serde_json::{Map, Value};

        let make = |title: &str, fields: Vec<(&str, Value)>| {
            let mut m = Map::new();
            for (k, v) in fields {
                m.insert(k.to_string(), v);
            }
            CustomEntity {
                id: "c1".into(),
                type_id: "t".into(),
                type_slug: "contact".into(),
                title: title.into(),
                fields: m,
                user_scope: None,
                origin_entry_id: None,
                created_by: "system".into(),
                created_at: String::new(),
                updated_at: String::new(),
            }
        };

        let fav = make("Jane Doe", vec![("favorite", Value::Bool(true))]);
        let not_fav = make("Bob", vec![("favorite", Value::Bool(false))]);
        let null_fav = make("Carol", vec![]);
        assert!(ContactFilter::Favorites.accepts(&fav));
        assert!(!ContactFilter::Favorites.accepts(&not_fav));
        assert!(
            !ContactFilter::Favorites.accepts(&null_fav),
            "null is not favorite"
        );
        assert!(ContactFilter::All.accepts(&null_fav));

        let grouped = make(
            "Jane Doe",
            vec![("groups", Value::String("Family, Work".into()))],
        );
        assert!(
            ContactFilter::Group("family".into()).accepts(&grouped),
            "case-insensitive group"
        );
        assert!(!ContactFilter::Group("School".into()).accepts(&grouped));

        // Search hits name and org, case-insensitively; empty query matches all.
        let hit = make(
            "Jane Doe",
            vec![("organization", Value::String("Acme".into()))],
        );
        assert!(contact_matches_search(&hit, ""));
        assert!(contact_matches_search(&hit, "jane"));
        assert!(contact_matches_search(&hit, "acme"));
        assert!(!contact_matches_search(&hit, "zzz"));
    }

    // ── compose helpers ─────────────────────────────────────────────────────

    /// Comma/semicolon splitting, `Name <email>` parsing, unquoting, and
    /// case-insensitive de-duplication.
    #[test]
    fn parse_recipients_splits_and_dedups() {
        use super::parse_recipients;
        let got = parse_recipients(
            "  Alice <alice@ex.test> , bob@ex.test; \"Carol B\" <carol@ex.test>, ALICE@ex.test ,  ",
        );
        assert_eq!(got.len(), 3, "Alice de-duplicated, empties dropped");
        assert_eq!(got[0].email, "alice@ex.test");
        assert_eq!(got[0].name.as_deref(), Some("Alice"));
        assert_eq!(got[1].email, "bob@ex.test");
        assert_eq!(got[1].name, None);
        assert_eq!(got[2].name.as_deref(), Some("Carol B"));
        assert!(parse_recipients("   ").is_empty());
    }

    /// `Re:` is prefixed once and never doubled.
    #[test]
    fn reply_subject_no_double_re() {
        use super::reply_subject;
        assert_eq!(reply_subject("Hello"), "Re: Hello");
        assert_eq!(reply_subject("Re: Hello"), "Re: Hello");
        assert_eq!(reply_subject("RE: Hello"), "RE: Hello");
        assert_eq!(reply_subject("re: hello"), "re: hello");
        assert_eq!(reply_subject("  spaced  "), "Re: spaced");
        assert_eq!(reply_subject(""), "Re:");
    }

    /// The quoted body carries an attribution line and `> `-prefixes each line;
    /// an empty original quotes to nothing (no dangling header).
    #[test]
    fn quote_reply_body_attributes_and_prefixes() {
        use super::quote_reply_body;
        let q = quote_reply_body("2026-07-10", "Maggie <m@ex.test>", "hi there\n\nsee you");
        assert_eq!(
            q,
            "On 2026-07-10, Maggie <m@ex.test> wrote:\n> hi there\n>\n> see you"
        );
        assert_eq!(quote_reply_body("2026-07-10", "x", "   "), "");
    }

    /// Reply picks Reply-To over From; reply-all unions sender + To into To and
    /// keeps Cc, always dropping the replying account's own address. References
    /// append the parent Message-ID once.
    #[test]
    fn reply_recipients_and_references() {
        use super::{
            reply_all_recipients, reply_references, reply_to_recipients, EmailAddr, MailReplyMeta,
        };
        let addr = |e: &str| EmailAddr {
            email: e.into(),
            name: None,
        };
        let meta = MailReplyMeta {
            account_id: "acct".into(),
            account_address: "me@ex.test".into(),
            message_id_hdr: Some("<parent@ex.test>".into()),
            references: vec!["<root@ex.test>".into()],
            from: vec![addr("sender@ex.test")],
            reply_to: vec![addr("list@ex.test")],
            to: vec![addr("me@ex.test"), addr("other@ex.test")],
            cc: vec![addr("cc1@ex.test"), addr("me@ex.test")],
            subject: "Bees".into(),
            received_at: "2026-07-10".into(),
        };

        // Reply → the Reply-To party only.
        let reply = reply_to_recipients(&meta);
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].email, "list@ex.test");

        // Reply-all → To = reply-to + original To minus self; Cc = original Cc
        // minus self and minus anyone already in To.
        let (to, cc) = reply_all_recipients(&meta);
        let to_emails: Vec<_> = to.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(to_emails, vec!["list@ex.test", "other@ex.test"]);
        let cc_emails: Vec<_> = cc.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(cc_emails, vec!["cc1@ex.test"]);

        // References = existing chain + the parent id, appended once.
        assert_eq!(
            reply_references(&meta),
            vec!["<root@ex.test>".to_string(), "<parent@ex.test>".to_string()]
        );
    }
}
