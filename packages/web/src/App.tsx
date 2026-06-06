import { createResource, ErrorBoundary, For, Show, Suspense, type Component, type JSX } from "solid-js";
import { Navigate, Route, Router, A, useLocation } from "@solidjs/router";
import { api, getActor, getCurrentUser, setCurrentUser } from "./api.ts";
import { connectLive, liveRev } from "./live.ts";
import { Journal } from "./Journal.tsx";
import { Inbox } from "./Inbox.tsx";
import { Dashboard } from "./Dashboard.tsx";
import { Settings } from "./Settings.tsx";
import { Admin } from "./Admin.tsx";
import { Account } from "./Account.tsx";
import { Graph } from "./Graph.tsx";
import { Onboarding } from "./Onboarding.tsx";
import { Login } from "./Login.tsx";
import { OAuthConsent } from "./OAuthConsent.tsx";
import { Icon } from "./icons.tsx";
import { Decisions, Events, PeopleView, ProjectsView, SearchPane, Tasks, TopicsView, Wire } from "./Boards.tsx";

const TABS = [
  { id: "journal" },
  { id: "inbox" },
  { id: "dashboard" },
  { id: "tasks" },
  { id: "decisions" },
  { id: "events" },
  { id: "people" },
  { id: "topics" },
  { id: "projects" },
  { id: "graph" },
  { id: "search" },
  { id: "wire" },
  { id: "admin" },
  { id: "account" },
  { id: "settings" },
] as const;
type Tab = (typeof TABS)[number]["id"];

// Each tab maps 1:1 to a route path (/journal, /inbox, …) so the URL reflects
// the current page and deep-links, refresh, and back/forward all work.
const PAGES: Record<Tab, Component> = {
  journal: Journal,
  inbox: Inbox,
  dashboard: Dashboard,
  tasks: Tasks,
  decisions: Decisions,
  events: Events,
  people: PeopleView,
  topics: TopicsView,
  projects: ProjectsView,
  graph: Graph,
  search: SearchPane,
  wire: Wire,
  admin: Admin,
  account: Account,
  settings: Settings,
};

// The signed-in shell: fixed sidebar + the routed page. Rendered as the Router's
// root layout so every route shares one chrome and a route transition can wrap
// the swapping content.
const Workspace = (props: {
  instanceName: string | null;
  onLogout: () => void;
}): ((rp: { children?: JSX.Element }) => JSX.Element) => {
  return (routeProps) => {
    const user = getCurrentUser();
    const isAdmin = user?.role === "admin";
    const actor = () => getActor();
    const location = useLocation();
    connectLive(); // open the SSE stream now that we're authenticated

    const [unread] = createResource(
      () => ({ actor: actor(), _r: liveRev() }),
      (k) => api.inbox(k.actor, true).then((items) => items.length),
    );

    // The account tab (user + token admin) is admin-only.
    const visibleTabs = TABS.filter((t) => t.id !== "account" || isAdmin);
    // Title comes from the leading path segment so it stays in sync with the URL.
    const pageTitle = () => location.pathname.replace(/^\//, "").split("/")[0] || "journal";

    return (
      <div class="app">
        <aside class="sidebar">
          <div class="brand">
            <span class="logo">🐝</span>
            <span class="brand-name">{props.instanceName ?? "hive"}</span>
          </div>

          <nav>
            <For each={visibleTabs}>
              {(t) => (
                <A href={`/${t.id}`} activeClass="active" end>
                  <span class="nav-icon"><Icon name={t.id} /></span>
                  <span class="nav-label">{t.id}</span>
                  <Show when={t.id === "inbox" && (unread() ?? 0) > 0}>
                    <span class="nav-badge">{unread()}</span>
                  </Show>
                </A>
              )}
            </For>
          </nav>

          <div class="sidebar-foot">
            <div class="signed-in">
              <span class="dim">signed in as</span>
              <strong>{user?.name ?? actor()}</strong>
              <span class="dim">{user?.role}</span>
            </div>
            <button class="logout" onClick={props.onLogout}>Sign out</button>
            <div class="foot-note dim">
              journal-first · MCP-first <code>POST /mcp</code>
            </div>
          </div>
        </aside>

        <main>
          <h2 class="page-title">{pageTitle()}</h2>
          {/* keyed on the leading path segment so each page remounts and re-runs
              the entrance animation when the route changes */}
          <Show when={pageTitle()} keyed>
            {(_seg) => <div class="route-view">{routeProps.children}</div>}
          </Show>
        </main>
      </div>
    );
  };
};

// Splash shown while the boot probe runs (and across its retries).
const Splash: Component<{ text: string }> = (props) => (
  <div class="auth-screen">
    <div class="auth-card">
      <div class="auth-brand"><span class="logo">🐝</span></div>
      <p class="dim">{props.text}</p>
    </div>
  </div>
);

export const App: Component = () => {
  // Boot: resolve onboarding state + current session before rendering anything.
  // Each request is timeout-bounded (see api.req); we retry a few times so a
  // just-restarted / cold hive-api recovers on its own instead of leaving the UI
  // stuck on a splash. If it still can't be reached, the ErrorBoundary below
  // surfaces a Retry button rather than hanging forever.
  const [boot, { refetch }] = createResource(async () => {
    let lastErr: unknown;
    for (let attempt = 0; attempt < 5; attempt++) {
      try {
        const status = await api.onboardingStatus();
        const me = status.completed ? await api.me() : null;
        if (me?.user) setCurrentUser(me.user);
        return { status, signedIn: !!me?.user };
      } catch (e) {
        lastErr = e;
        await new Promise((r) => setTimeout(r, 1500));
      }
    }
    throw lastErr;
  });

  const reload = () => refetch();
  const onLogout = async () => {
    try {
      await api.logout();
    } finally {
      setCurrentUser(null);
      refetch();
    }
  };

  return (
    <ErrorBoundary
      fallback={(_err, reset) => (
        <div class="auth-screen">
          <div class="auth-card">
            <div class="auth-brand"><span class="logo">🐝</span></div>
            <h1>Can't reach hive</h1>
            <p class="dim">The server didn't respond — it may be starting up. Give it a moment, then retry.</p>
            <button class="logout" onClick={() => { reset(); refetch(); }}>Retry</button>
          </div>
        </div>
      )}
    >
      <Suspense fallback={<Splash text="Connecting to hive…" />}>
        <Show when={boot()?.status.completed} fallback={<Onboarding onDone={reload} />}>
          <Show
            when={boot()?.signedIn}
            fallback={<Login instanceName={boot()?.status.instanceName ?? null} onLogin={reload} />}
          >
            {/* OAuth consent is a standalone full-page screen (no workspace
                chrome), so it short-circuits BEFORE the router that owns the
                authenticated app shell. window.location (not useLocation) since
                this sits outside the Router context. */}
            <Show
              when={window.location.pathname !== "/consent"}
              fallback={<OAuthConsent />}
            >
              <Router root={Workspace({ instanceName: boot()?.status.instanceName ?? null, onLogout })}>
                <Route path="/" component={() => <Navigate href="/journal" />} />
                <For each={TABS}>
                  {(t) => (
                    <Route
                      path={`/${t.id}`}
                      component={
                        t.id === "account" && getCurrentUser()?.role !== "admin"
                          ? () => <Navigate href="/journal" />
                          : PAGES[t.id]
                      }
                    />
                  )}
                </For>
                <Route path="*" component={() => <Navigate href="/journal" />} />
              </Router>
            </Show>
          </Show>
        </Show>
      </Suspense>
    </ErrorBoundary>
  );
};
