use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::StaticSegment;
use leptos_router::components::{Route, Router, Routes};

use crate::api::SessionId;
use crate::auth::SESSION_COOKIE;
use crate::pages::home::HomePage;
use crate::pages::journal::JournalPage;
use crate::pages::notes::NotesPage;
use crate::pages::tasks::TasksPage;
use crate::pages::wire::WirePage;

/// Read the `hive_ui_session` cookie from the SSR request parts (provided into
/// context by leptos_axum) and surface it as `SessionId`. Synchronous: the
/// parts are already in context by the time `App` renders. Returns an empty
/// `SessionId` when there's no request context (unit tests) or no cookie.
fn session_from_request() -> SessionId {
    let Some(parts) = use_context::<http::request::Parts>() else {
        return SessionId::default();
    };
    let cookie_header = parts
        .headers
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let sid = cookie_header
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == SESSION_COOKIE)
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty());
    SessionId(sid)
}

pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <AutoReload options=options.clone() />
                <HydrationScripts options/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
fn Nav() -> impl IntoView {
    view! {
        <nav class="hive-nav">
            <a href="/">"links"</a>
            <a href="/journal">"journal"</a>
            <a href="/tasks">"tasks"</a>
            <a href="/notes">"notes"</a>
            <a href="/wire">"wire"</a>
            // /login + /logout are plain axum routes (the OAuth flow runs
            // server-side), not leptos-router views — hence full-page links.
            <a class="hive-nav-auth" href="/login" rel="external">"login"</a>
            <a class="hive-nav-auth" href="/logout" rel="external">"logout"</a>
        </nav>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    // Make the session id (from the cookie) available to every hive-api fetch
    // for the duration of this SSR render (Phase 3, §3.1).
    provide_context(session_from_request());

    view! {
        <Stylesheet id="hive-ui-css" href="/style/main.css"/>
        <Title text="hive-canvas"/>

        <Router>
            <Nav/>
            <main>
                <Routes fallback=|| view! { <p>"not found"</p> }>
                    <Route path=StaticSegment("") view=HomePage/>
                    <Route path=StaticSegment("journal") view=JournalPage/>
                    <Route path=StaticSegment("tasks") view=TasksPage/>
                    <Route path=StaticSegment("notes") view=NotesPage/>
                    <Route path=StaticSegment("wire") view=WirePage/>
                </Routes>
            </main>
        </Router>
    }
}
