use leptos::prelude::*;
use leptos_meta::{MetaTags, Stylesheet, Title, provide_meta_context};
use leptos_router::StaticSegment;
use leptos_router::components::{Route, Router, Routes};

use crate::pages::home::HomePage;
use crate::pages::journal::JournalPage;
use crate::pages::notes::NotesPage;
use crate::pages::tasks::TasksPage;
use crate::pages::wire::WirePage;

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
        </nav>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

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
