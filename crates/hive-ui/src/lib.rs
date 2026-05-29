//! hive-ui ... leptos 0.7 isomorphic web canvas for the hive shared-state DB.
//!
//! This crate is dual-target:
//!
//! - With `--features ssr` it builds a Rust library + bin (`src/main.rs`) that
//!   serves a leptos-rendered axum app. Used by `cargo run -p hive-ui` and by
//!   `cargo leptos serve` for the server side of the isomorphic stack.
//!
//! - With `--features hydrate --target wasm32-unknown-unknown` it builds the
//!   WASM bundle that ships to the browser. cargo-leptos runs this pass and
//!   drops the resulting `hive-ui_bg.wasm` + `hive-ui.js` into `pkg/`. The
//!   `hydrate()` function below is the entry point the browser calls to mount
//!   `App` onto the SSR-rendered DOM.
//!
//! The same `app` module renders on both sides ... so anything reactive
//! (`on:click`, `signal`, `Resource`) Just Works in the browser once hydration
//! lands.

pub mod app;
pub mod markdown;
pub mod pages;

// `api` is dual-target: types + `SessionId` ship on both sides; the actual
// `fetch_*` functions only have working bodies on `ssr` (the wasm/hydrate
// side gets one-line stubs that return an error). `auth` is reqwest +
// process-local Mutex store ... server-only, full stop.
pub mod api;
#[cfg(feature = "ssr")]
pub mod auth;

/// Browser-side entry point. cargo-leptos wires the generated
/// `hive-ui.js` to call this from the loaded WASM bundle; that in turn
/// calls `leptos::mount::hydrate_body` against the existing SSR-rendered
/// DOM, lighting up every `on:click`, signal, and `Resource` in the tree.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(crate::app::App);
}
