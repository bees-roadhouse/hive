# hive-ui

Leptos 0.7 isomorphic web canvas for the hive shared-state DB.

The crate is dual-target:

- **SSR + axum bin** under `--features ssr` (the default for `cargo run`).
- **Browser WASM bundle** under `--features hydrate --target wasm32-unknown-unknown` ... cargo-leptos's lib pass.

The same `App` component renders on both sides. After hydration, every
`on:click`, signal, and `Resource` reactivity lights up in the browser.

## Toolchain

Pinned to **rust 1.95.0** via `rust-toolchain.toml` at the workspace root.

You need the wasm32 target:

```powershell
rustup target add wasm32-unknown-unknown
```

And `cargo-leptos` ... the build tool that drives both passes:

```powershell
# Windows: openssl-sys's vendored OpenSSL needs perl + a C compiler.
# Strawberry Perl ships both; install once via winget:
winget install StrawberryPerl.StrawberryPerl

# Then with C:\Strawberry\perl\bin and C:\Strawberry\c\bin on PATH:
cargo install cargo-leptos --locked
```

The MSYS perl that ships with Git for Windows does NOT work ... it's
missing `Locale::Maketext::Simple` which openssl-src's build script needs.

## Building

`cargo-leptos` orchestrates the two passes:

```powershell
# One-shot build (server + wasm + bundled CSS, dropped under target/site/).
cargo leptos build --release

# Dev server with hot-reload: starts the axum bin on 127.0.0.1:8091 and the
# leptos reload channel on :3001.
cargo leptos serve
```

To build the two passes individually (useful for CI / clippy):

```powershell
$env:CARGO_TARGET_DIR = "C:\Users\nate\.cargo-target\hive-ui"
cargo build  -p hive-ui --features ssr --no-default-features
cargo build  -p hive-ui --features hydrate --no-default-features --target wasm32-unknown-unknown
cargo clippy -p hive-ui --features ssr     --no-default-features -- -D warnings
cargo clippy -p hive-ui --features hydrate --no-default-features --target wasm32-unknown-unknown --lib -- -D warnings
```

## Why a redirected CARGO_TARGET_DIR

The hive workspace lives on SeaDrive, which sometimes corrupts cargo's
fingerprint writes under the default `target/`. Redirect to a local-disk
path before any cargo command:

```powershell
$env:CARGO_TARGET_DIR = "C:\Users\nate\.cargo-target\hive-ui"
```

cargo-leptos respects `CARGO_TARGET_DIR` for the build artifacts, but
`site-root` in `[package.metadata.leptos]` is relative to the manifest
dir and still resolves under the redirected target tree.

## Site output

cargo-leptos drops:

- `target/site/pkg/hive-ui.js`           ... the wasm loader stub
- `target/site/pkg/hive-ui_bg.wasm`      ... the hydration bundle
- `target/site/pkg/hive-ui.css`          ... the compiled stylesheet (from `style/main.css`)
- `target/site/static/...`               ... assets copied from `static/`

`main.rs` serves `/pkg/...` and `/static/...` from `target/site/` so the
browser fetches the bundle off the same origin as the SSR HTML.

## Crate layout

- `src/lib.rs`         ... the library (`App`, pages, api types) + `hydrate()` entry
- `src/main.rs`        ... axum bin: SSR + auth handlers + POST /journal/new + /api/recent + /who/:slug
- `src/app.rs`         ... `App` component + Routes + shell (ssr-only)
- `src/api.rs`         ... hive-api client (`fetch_*` fns are ssr-only; wasm stubs error)
- `src/auth.rs`        ... OAuth password+PKCE flow, server-side session store (ssr-only)
- `src/markdown.rs`    ... shared markdown + mention renderer (both targets)
- `src/pages/`         ... one module per route (HomePage, ComposePage, EntryPage, ...)
- `style/main.css`     ... the single stylesheet, compiled into `pkg/hive-ui.css`
- `static/`            ... static assets (favicon-style stuff; the compose-picker JS
                          has been retired in favor of the hydrated `pages::compose`)
