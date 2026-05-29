//! `/journal/new` ... the compose page (GET form + entity picker).
//!
//! Two-sided story:
//!
//! - **GET /journal/new** (this Leptos page) renders the form fields and the
//!   interactive entity picker. The page is part of the Leptos route table,
//!   so it goes through the same SSR + hydrate pipeline as the rest of the
//!   app. The picker's reactivity (signal + Resource + dropdown) lives in
//!   the hydrated WASM side.
//!
//! - **POST /journal/new** stays as a hand-rolled axum handler in `main.rs`.
//!   The form posts to itself; the server validates + forwards to hive-api
//!   + redirects home. Same code as before ... only the GET form moved.
//!
//! The picker replaces what `static/compose-picker.js` used to do: watch the
//! body textarea for `#task|#note|#event|#journal|#person|#ai`, fire a
//! typeahead fetch to `/api/recent?type=...&q=...`, and on selection
//! substitute the trigger with the canonical `[[type:uuid|title]]` anchor.
//!
//! CSS classes mirror the JS version (`.picker-dropdown`, `.picker-row`,
//! `.picker-row.active`, `.picker-title`, `.picker-meta`) so the styles in
//! `style/main.css` keep working without touching the stylesheet.

use leptos::prelude::*;
#[cfg(feature = "hydrate")]
use leptos::task::spawn_local;
use leptos_router::hooks::use_query_map;
use serde::{Deserialize, Serialize};

/// Trigger types the picker recognizes (matches the legacy JS TYPES list).
#[cfg(feature = "hydrate")]
const TYPES: &[&str] = &["task", "note", "event", "journal", "person", "ai"];

/// Writers offered in the dropdown. Order matters ... pia first.
const COMPOSE_WRITERS: &[&str] = &["pia", "apis", "cera", "nate", "maggie"];
const COMPOSE_DEFAULT_WRITER: &str = "nate";

/// One row in the picker dropdown (matches the JSON `/api/recent` returns).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PickerRow {
    id: String,
    title: String,
    #[serde(default)]
    meta: String,
    #[serde(default)]
    created_at: String,
}

/// The current trigger context: which `#type` opened the dropdown, where in
/// the body the trigger lives, and the query suffix after the type.
#[derive(Debug, Clone)]
struct Trigger {
    /// One of the entries in `TYPES`.
    ty: &'static str,
    /// Byte index of the `#` in the body.
    trigger_idx: usize,
    /// Byte index just past the end of the user's typed query (the cursor).
    query_end: usize,
    /// The query suffix the user typed after the type (e.g. `#task fix` -> `fix`).
    /// Empty when they've only typed `#task`. Only read on the hydrate side.
    #[allow(dead_code)]
    query: String,
}

/// Today's date as `YYYY-MM-DD`, used to default the `date` field. On
/// the server we use `chrono::Local`. On wasm we read the browser clock
/// via `js_sys::Date` ... mirrors what the user expects.
#[cfg(feature = "ssr")]
fn today_iso() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn today_iso() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:04}-{:02}-{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date()
    )
}

#[cfg(not(any(feature = "ssr", feature = "hydrate")))]
fn today_iso() -> String {
    String::new()
}

#[component]
pub fn ComposePage() -> impl IntoView {
    // ?error=... is the round-tripped failure message from the POST handler.
    let query = use_query_map();
    let error = query.with_untracked(|q| q.get("error")).unwrap_or_default();

    let today = today_iso();

    let writer_options = COMPOSE_WRITERS
        .iter()
        .map(|w| {
            let label = w.to_string();
            let value = w.to_string();
            let selected = *w == COMPOSE_DEFAULT_WRITER;
            view! { <option value=value selected=selected>{label}</option> }
        })
        .collect_view();

    // Body textarea state. Driven from the textarea's `prop:value` so the
    // signal is authoritative across SSR + hydrate. Selection cursor lives
    // in the DOM ... the picker reads it back via `selectionStart` when
    // running on wasm.
    let body = RwSignal::new(String::new());
    let trigger: RwSignal<Option<Trigger>> = RwSignal::new(None);
    let rows = RwSignal::new(Vec::<PickerRow>::new());
    let active_idx = RwSignal::new(0usize);

    // The textarea node-ref lets the hydrated picker read the cursor and
    // rewrite the value on selection.
    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();

    let error_block = if error.is_empty() {
        ().into_any()
    } else {
        view! { <p class="error">{error}</p> }.into_any()
    };

    view! {
        <main class="compose-page">
            <h1>"new entry"</h1>
            {error_block}
            <form method="post" action="/journal/new" class="compose-form">
                <label>"writer "
                    <select name="ai" required>{writer_options}</select>
                </label>
                <label>"date "
                    <input name="date" type="date" value=today/>
                </label>
                <label>"title "
                    <input name="title" type="text" required/>
                </label>
                <label>"body "
                    <textarea
                        name="body"
                        rows="14"
                        required
                        node_ref=textarea_ref
                        prop:value=move || body.get()
                        on:input=move |ev| {
                            let v = event_target_value(&ev);
                            body.set(v);
                            update_trigger(&textarea_ref, &body, &trigger, &active_idx);
                        }
                        on:keyup=move |ev| {
                            // Caret moves via arrow keys don't trigger `input` ... resync here.
                            let key = ev.key();
                            if key == "ArrowLeft" || key == "ArrowRight"
                                || key == "Home" || key == "End"
                            {
                                update_trigger(&textarea_ref, &body, &trigger, &active_idx);
                            }
                        }
                        on:click=move |_| {
                            update_trigger(&textarea_ref, &body, &trigger, &active_idx);
                        }
                        on:keydown=move |ev| {
                            handle_keydown(
                                ev,
                                &textarea_ref,
                                &body,
                                &trigger,
                                &rows,
                                &active_idx,
                            );
                        }
                    ></textarea>
                </label>
                <label>"tags "
                    <input name="tags" type="text" placeholder="comma-separated, e.g. immich,traefik"/>
                </label>
                <div class="compose-actions">
                    <a class="compose-cancel" href="/">"cancel"</a>
                    <button type="submit">"save"</button>
                </div>
            </form>
            <PickerDropdown trigger rows active_idx textarea_ref body/>
        </main>
    }
}

/// Detect a `#type` trigger ending at the cursor, fetch matching rows,
/// and update the reactive state. No-op on SSR (no DOM cursor to read).
fn update_trigger(
    textarea_ref: &NodeRef<leptos::html::Textarea>,
    body: &RwSignal<String>,
    trigger: &RwSignal<Option<Trigger>>,
    active_idx: &RwSignal<usize>,
) {
    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::JsCast;
        let Some(el) = textarea_ref.get() else { return };
        let ta: &web_sys::HtmlTextAreaElement = el.unchecked_ref();
        let pos = ta.selection_start().ok().flatten().unwrap_or(0) as usize;
        let end = ta.selection_end().ok().flatten().unwrap_or(0) as usize;
        if pos != end {
            trigger.set(None);
            return;
        }
        let value = body.get_untracked();
        match detect_trigger(&value, pos) {
            Some(t) => {
                let already = trigger.with_untracked(|cur| {
                    cur.as_ref()
                        .map(|c| c.trigger_idx == t.trigger_idx && c.ty == t.ty)
                        .unwrap_or(false)
                });
                if !already {
                    active_idx.set(0);
                }
                let ty = t.ty;
                let q = t.query.clone();
                trigger.set(Some(t));
                spawn_local(async move {
                    if let Some(fetched) = fetch_recent(ty, &q).await {
                        // Only apply if the trigger still matches.
                        // Caller's responsibility ... we do a single-shot update.
                        APPLY_ROWS.with(|cb| {
                            if let Some(cb) = cb.borrow().as_ref() {
                                cb(fetched);
                            }
                        });
                    }
                });
            }
            None => {
                trigger.set(None);
            }
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (textarea_ref, body, trigger, active_idx);
    }
}

#[cfg(feature = "hydrate")]
type ApplyRowsCb = Box<dyn Fn(Vec<PickerRow>)>;

#[cfg(feature = "hydrate")]
thread_local! {
    /// Workaround for `spawn_local`'s 'static requirement: the picker
    /// registers a row-applier callback once when the dropdown mounts;
    /// `update_trigger` fires the fetch and calls the registered cb on
    /// completion. Single-shot, last-writer-wins.
    static APPLY_ROWS: std::cell::RefCell<Option<ApplyRowsCb>>
        = const { std::cell::RefCell::new(None) };
}

/// Walk back from `cursor` to the most-recent `#`, bail on whitespace.
/// Then check that the `#` is at start-of-text or preceded by whitespace,
/// and that the word after it begins with a known type. Returns the
/// active trigger when one's found.
#[cfg(feature = "hydrate")]
fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let cursor = cursor.min(text.len());
    if cursor == 0 {
        return None;
    }
    let bytes = text.as_bytes();
    // Walk back to the `#`.
    let mut i = cursor;
    while i > 0 {
        let b = bytes[i - 1];
        if b == b'#' {
            i -= 1;
            break;
        }
        if b.is_ascii_whitespace() {
            return None;
        }
        i -= 1;
    }
    if i >= cursor {
        return None;
    }
    if bytes[i] != b'#' {
        return None;
    }
    // `#` must be at start or preceded by whitespace.
    if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        return None;
    }
    let word = &text[i + 1..cursor];
    // Find the longest known type that's a prefix.
    let mut matched: Option<&'static str> = None;
    for ty in TYPES {
        if word == *ty || (word.len() > ty.len() && word.starts_with(ty)) {
            matched = Some(ty);
            break;
        }
    }
    let ty = matched?;
    let query = word[ty.len()..].to_string();
    Some(Trigger {
        ty,
        trigger_idx: i,
        query_end: cursor,
        query,
    })
}

/// Hit `/api/recent?type=...&q=...` and parse the JSON array.
#[cfg(feature = "hydrate")]
async fn fetch_recent(ty: &str, q: &str) -> Option<Vec<PickerRow>> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Request, RequestInit, Response};

    let url = format!("/api/recent?type={}&q={}", urlencode(ty), urlencode(q));
    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_credentials(web_sys::RequestCredentials::SameOrigin);
    let req = Request::new_with_str_and_init(&url, &opts).ok()?;
    let win = web_sys::window()?;
    let resp_val = JsFuture::from(win.fetch_with_request(&req)).await.ok()?;
    let resp: Response = resp_val.dyn_into().ok()?;
    if !resp.ok() {
        return None;
    }
    let text_promise = resp.text().ok()?;
    let text_val = JsFuture::from(text_promise).await.ok()?;
    let text = text_val.as_string()?;
    serde_json::from_str::<Vec<PickerRow>>(&text).ok()
}

#[cfg(feature = "hydrate")]
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Keyboard handler on the textarea: arrow keys move selection, Enter/Tab
/// selects, Escape closes.
fn handle_keydown(
    ev: leptos::ev::KeyboardEvent,
    textarea_ref: &NodeRef<leptos::html::Textarea>,
    body: &RwSignal<String>,
    trigger: &RwSignal<Option<Trigger>>,
    rows: &RwSignal<Vec<PickerRow>>,
    active_idx: &RwSignal<usize>,
) {
    let is_open = trigger.with_untracked(|t| t.is_some());
    if !is_open {
        return;
    }
    let key = ev.key();
    let len = rows.with_untracked(|r| r.len());
    match key.as_str() {
        "ArrowDown" => {
            ev.prevent_default();
            if len > 0 {
                active_idx.update(|i| *i = (*i + 1) % len);
            }
        }
        "ArrowUp" => {
            ev.prevent_default();
            if len > 0 {
                active_idx.update(|i| *i = (*i + len - 1) % len);
            }
        }
        "Enter" | "Tab" if len > 0 => {
            ev.prevent_default();
            let idx = active_idx.get_untracked();
            select_row(idx, textarea_ref, body, trigger, rows);
        }
        "Escape" => {
            ev.prevent_default();
            trigger.set(None);
        }
        _ => {}
    }
}

/// Replace the trigger range in `body` with `[[type:id|title]]`, refocus
/// the textarea, and close the dropdown.
fn select_row(
    idx: usize,
    textarea_ref: &NodeRef<leptos::html::Textarea>,
    body: &RwSignal<String>,
    trigger: &RwSignal<Option<Trigger>>,
    rows: &RwSignal<Vec<PickerRow>>,
) {
    let row = rows.with_untracked(|r| r.get(idx).cloned());
    let Some(row) = row else { return };
    let Some(t) = trigger.get_untracked() else {
        return;
    };
    let mut value = body.get_untracked();
    if t.trigger_idx > value.len() || t.query_end > value.len() {
        trigger.set(None);
        return;
    }
    let anchor = format!("[[{}:{}|{}]]", t.ty, row.id, row.title);
    value.replace_range(t.trigger_idx..t.query_end, &anchor);
    body.set(value.clone());
    trigger.set(None);

    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::JsCast;
        if let Some(el) = textarea_ref.get() {
            let ta: &web_sys::HtmlTextAreaElement = el.unchecked_ref();
            ta.set_value(&value);
            let new_pos = (t.trigger_idx + anchor.len()) as u32;
            let _ = ta.set_selection_start(Some(new_pos));
            let _ = ta.set_selection_end(Some(new_pos));
            let _ = ta.focus();
        }
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = textarea_ref;
    }
}

/// The dropdown itself ... renders only when a trigger is active.
#[component]
fn PickerDropdown(
    trigger: RwSignal<Option<Trigger>>,
    rows: RwSignal<Vec<PickerRow>>,
    active_idx: RwSignal<usize>,
    textarea_ref: NodeRef<leptos::html::Textarea>,
    body: RwSignal<String>,
) -> impl IntoView {
    // Register the row-applier callback so `fetch_recent` results flow back
    // into the dropdown state. (Mount-once; the cell is a thread-local.)
    #[cfg(feature = "hydrate")]
    {
        let rows_cb = rows;
        APPLY_ROWS.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move |r| {
                rows_cb.set(r);
            }));
        });
    }

    move || {
        if trigger.with(|t| t.is_none()) {
            return ().into_any();
        }
        let current_rows = rows.get();
        if current_rows.is_empty() {
            return view! {
                <div class="picker-dropdown" role="listbox">
                    <div class="picker-row picker-empty">"no matches"</div>
                </div>
            }
            .into_any();
        }
        let active = active_idx.get();
        view! {
            <div class="picker-dropdown" role="listbox">
                {current_rows.into_iter().enumerate().map(|(i, r)| {
                    let title = r.title.clone();
                    let meta = r.meta.clone();
                    let title_text = if title.is_empty() {
                        "(untitled)".to_string()
                    } else {
                        title.clone()
                    };
                    let mut cls = String::from("picker-row");
                    if i == active {
                        cls.push_str(" active");
                    }
                    view! {
                        <div
                            class=cls
                            role="option"
                            on:mousedown=move |ev| {
                                ev.prevent_default();
                                select_row(i, &textarea_ref, &body, &trigger, &rows);
                            }
                            on:mouseenter=move |_| active_idx.set(i)
                        >
                            <div class="picker-title">{title_text}</div>
                            {(!meta.is_empty()).then(|| view! {
                                <div class="picker-meta">{meta}</div>
                            })}
                        </div>
                    }
                }).collect_view()}
            </div>
        }
        .into_any()
    }
}
