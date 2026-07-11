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
use hive_shared::{JournalEntryView, NewJournalEntry, SearchHit};

/// Config key naming the journal's author (the onboarding identity step
/// writes it; the Shell reads it at mount). Dotted, matching the config
/// table's conventions (instance.name, search.kind_weights).
const IDENTITY_OWNER_KEY: &str = "identity.owner";

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
                style: "color: {FAINT}; font-size: 0.8rem; margin-top: 0.5rem;",
                "Embeddings backfill in the background once the journal opens."
            }
        }
    }
}

#[component]
fn Shell(store: ReadOnlySignal<Store>) -> Element {
    let mut draft = use_signal(String::new);
    let mut query = use_signal(String::new);
    let mut status = use_signal(|| Option::<String>::None);
    let mut committed = use_signal(|| 0u32);

    // Journal authorship: the identity chosen at onboarding (identity.owner
    // config), read once at mount; author_name() covers stores that predate
    // the record and any read failure.
    let author = use_resource(move || {
        let store = store();
        async move {
            match store.config_get(IDENTITY_OWNER_KEY).await {
                Ok(Some(owner)) if !owner.trim().is_empty() => owner,
                _ => author_name(),
            }
        }
    });

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
        let byline = author().unwrap_or_else(author_name);
        spawn(async move {
            let input = NewJournalEntry {
                author: Some(byline),
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
