// hive — the desktop shell (DIRECTION.md D17).
//
// This is the Phase 2 scaffold, shipped early so the flatpak pipeline and
// WebKitGTK-on-Bazzite path are proven before the engine lands. hive-core
// (the append-only store) wires in at the PR 1.6 cutover; until then the
// window reports build status instead of data.

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_window(
                WindowBuilder::new()
                    .with_title("hive")
                    .with_inner_size(LogicalSize::new(760.0, 520.0)),
            ),
        )
        .launch(app);
}

fn app() -> Element {
    let version = env!("CARGO_PKG_VERSION");
    rsx! {
        div {
            style: "min-height: 100vh; margin: 0; display: flex; flex-direction: column; \
                    align-items: center; justify-content: center; gap: 0.6rem; \
                    background: #14120e; color: #e8e2d4; \
                    font-family: system-ui, sans-serif;",
            div { style: "font-size: 3.2rem; line-height: 1; color: #e2a921;", "⬡" }
            div { style: "font-size: 1.9rem; font-weight: 700; letter-spacing: 0.04em;", "hive" }
            div { style: "font-size: 0.95rem; color: #9a927e;", "v{version} — personal, local-first, peer to peer" }
            div {
                style: "margin-top: 1.4rem; font-size: 0.9rem; color: #6f684f; \
                        border: 1px solid #2c2818; border-radius: 8px; padding: 0.9rem 1.2rem; \
                        max-width: 30rem; line-height: 1.6;",
                "The engine is being assembled (Phase 1: append-only op-log, encrypted "
                "blockstore, SQLite index). Journal, search, mail, calendar, and contacts "
                "wire into this window at the storage cutover."
            }
        }
    }
}
