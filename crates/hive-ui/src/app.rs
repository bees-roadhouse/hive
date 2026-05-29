use leptos::prelude::*;
#[cfg(feature = "ssr")]
use leptos_meta::MetaTags;
use leptos_meta::{Stylesheet, Title, provide_meta_context};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::{StaticSegment, path};

use crate::api::SessionId;
use crate::pages::compose::ComposePage;
use crate::pages::entry::EntryPage;
use crate::pages::events::{EventDetailPage, EventsPage};
use crate::pages::home::HomePage;
use crate::pages::journal::JournalPage;
use crate::pages::note_detail::NoteDetailPage;
use crate::pages::notes::NotesPage;
use crate::pages::people::{AiDetailPage, AiListPage, PeopleListPage, PersonDetailPage};
use crate::pages::search::SearchPage;
use crate::pages::side_panel::{PanelToggle, SidePanel};
use crate::pages::task_detail::TaskDetailPage;
use crate::pages::tasks::TasksPage;
use crate::pages::wire::WirePage;

/// Read the `hive_ui_session` cookie from the SSR request parts (provided into
/// context by leptos_axum) and surface it as `SessionId`. Synchronous: the
/// parts are already in context by the time `App` renders. Returns an empty
/// `SessionId` when there's no request context (unit tests) or no cookie.
///
/// SSR-only: the request parts only exist on the server side. On the
/// hydrate side the session id was already used when the server rendered
/// the page and we don't need it again (the SSR-resolved Resource values
/// are baked into the hydration data).
#[cfg(feature = "ssr")]
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
        .find(|(k, _)| k.trim() == crate::auth::SESSION_COOKIE)
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty());
    SessionId(sid)
}

#[cfg(not(feature = "ssr"))]
fn session_from_request() -> SessionId {
    SessionId::default()
}

#[cfg(feature = "ssr")]
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
    // Minimal top bar: brand on the left, auth tucked into the corner. The
    // journal feed (home) handles its own chip/filter chrome — the global
    // nav stays out of the writing's way. Tasks/notes/wire keep their routes
    // (/tasks, /notes, /wire) for direct URL access; the side-panel toggle
    // will surface them per-entry in a follow-up.
    view! {
        <nav class="hive-topbar">
            <a class="hive-brand" href="/" rel="external">"hive"</a>
            <a class="hive-new" href="/journal/new" rel="external">"+ new"</a>
            <a class="hive-nav-link" href="/tasks" rel="external">"tasks"</a>
            <a class="hive-nav-link" href="/notes" rel="external">"notes"</a>
            <a class="hive-nav-link" href="/wire" rel="external">"wire"</a>
            <form class="hive-search" method="get" action="/journal/search">
                <input type="search" name="q" placeholder="search journal" aria-label="search journal" />
            </form>
            <span class="hive-spacer"></span>
            <a class="hive-auth" href="/login" rel="external">"log in"</a>
            <a class="hive-auth" href="/logout" rel="external">"log out"</a>
            <PanelToggle/>
        </nav>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    // Make the session id (from the cookie) available to every hive-api fetch
    // for the duration of this SSR render (Phase 3, §3.1).
    provide_context(session_from_request());

    // Link the source stylesheet directly. `main.rs` always serves
    // `/style/main.css`; `/pkg/hive-ui.css` only exists after `cargo leptos
    // build` with `LEPTOS_SITE_ROOT` pointed at the site output. Running the
    // bin standalone (or with a missing site dir) 404'd the pkg path and
    // left the page unstyled white.
    view! {
        <Stylesheet id="hive-ui-css" href="/style/main.css"/>
        <Title text="hive-canvas"/>

        <Router>
            <Nav/>
            <main>
                <Routes fallback=|| view! { <p>"not found"</p> }>
                    <Route path=StaticSegment("") view=HomePage/>
                    <Route path=path!("/journal/search") view=SearchPage/>
                    <Route path=path!("/journal/new") view=ComposePage/>
                    <Route path=StaticSegment("journal") view=JournalPage/>
                    <Route path=path!("/journal/:id") view=EntryPage/>
                    <Route path=StaticSegment("tasks") view=TasksPage/>
                    <Route path=path!("/tasks/:slug") view=TaskDetailPage/>
                    <Route path=StaticSegment("notes") view=NotesPage/>
                    <Route path=path!("/notes/:slug") view=NoteDetailPage/>
                    <Route path=StaticSegment("events") view=EventsPage/>
                    <Route path=path!("/events/:slug") view=EventDetailPage/>
                    <Route path=StaticSegment("people") view=PeopleListPage/>
                    <Route path=path!("/people/:slug") view=PersonDetailPage/>
                    <Route path=StaticSegment("ai") view=AiListPage/>
                    <Route path=path!("/ai/:slug") view=AiDetailPage/>
                    <Route path=StaticSegment("wire") view=WirePage/>
                </Routes>
            </main>
            <SidePanel/>
        </Router>
    }
}
