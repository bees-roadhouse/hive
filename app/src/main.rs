// hive — the desktop shell (DIRECTION.md D17).
//
// This is the Phase 2 scaffold, shipped early so the flatpak pipeline and
// WebKitGTK-on-Bazzite path are proven before the engine lands. hive-core
// (the append-only store) wires in at the PR 1.6 cutover; until then the
// window reports build status instead of data.

use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::{use_wry_event_handler, Config, WindowBuilder};
use dioxus::prelude::*;

fn main() {
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_menu(None).with_window(
                WindowBuilder::new()
                    .with_title("hive")
                    .with_inner_size(LogicalSize::new(760.0, 520.0)),
            ),
        )
        .launch(app);
}

fn app() -> Element {
    // The titlebar close button must actually quit: dioxus 0.6's default close
    // handling doesn't exit this single-window app reliably, so handle the
    // CloseRequested window event explicitly.
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

    let version = env!("CARGO_PKG_VERSION");
    rsx! {
        div {
            style: "position: fixed; inset: 0; display: flex; flex-direction: column; \
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
                "Nothing is loading — this build is only the app shell. The storage "
                "engine is being built in the repo and arrives as app updates: journal "
                "and search first, then mail, calendar, and contacts. This screen is "
                "all this version does."
            }
        }
    }
}
