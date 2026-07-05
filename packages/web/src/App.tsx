import { createResource, ErrorBoundary, For, onCleanup, onMount, Show, Suspense, type Component, type JSX } from "solid-js";
import { Navigate, Route, Router, A, useLocation } from "@solidjs/router";
import { api, getActor, getCurrentUser, setCurrentUser } from "./api.ts";
import { connectLive, liveRev } from "./live.ts";
import { setPaletteOpen } from "./ui.ts";
import { CommandPalette } from "./CommandPalette.tsx";
import { Journal } from "./Journal.tsx";
import { Inbox } from "./Inbox.tsx";
import { Dashboard } from "./Dashboard.tsx";
import { Settings } from "./Settings.tsx";
import { Admin } from "./Admin.tsx";
import { Account } from "./Account.tsx";
import { Graph } from "./Graph.tsx";
import { Workspaces } from "./Workspaces.tsx";
import { Onboarding } from "./Onboarding.tsx";
import { Login } from "./Login.tsx";
import { OAuthConsent } from "./OAuthConsent.tsx";
import { Icon } from "./icons.tsx";
import { Decisions, Events, PeopleView, ProjectsView, SearchPane, Tasks, TopicsView, Wire } from "./Boards.tsx";

// Every tab stays a registered route (deep links, refresh, back/forward all
// keep working) — but only PRIMARY earns a sidebar slot. Everything else is
// reached through the ⌘K command palette, which keeps the shell calm.
const TABS = [
  { id: "journal" },
  { id: "inbox" },
  { id: "dashboard" },
  { id: "workspaces" },
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

const PAGES: Record<Tab, Component> = {
  journal: Journal,
  inbox: Inbox,
  dashboard: Dashboard,
  workspaces: Workspaces,
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

// The four destinations that stay in the sidebar. The journal is the primary
// surface ("Today"); the rest are the daily loops.
const PRIMARY: { id: Tab; label: string; icon: string }[] = [
  { id: "journal", label: "Today", icon: "journal" },
  { id: "inbox", label: "Inbox", icon: "inbox" },
  { id: "search", label: "Search", icon: "search" },
  { id: "workspaces", label: "Workspaces", icon: "workspaces" },
];

// Initials for the footer avatar chip ("Nate Smith" → "NS").
const initials = (name: string): string =>
  name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0]!.toUpperCase())
    .join("") || "?";

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

    // ⌘K / Ctrl+K opens the palette from anywhere in the shell.
    onMount(() => {
      const onKey = (e: KeyboardEvent) => {
        if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
          e.preventDefault();
          setPaletteOpen((open) => !open);
        }
      };
      window.addEventListener("keydown", onKey);
      onCleanup(() => window.removeEventListener("keydown", onKey));
    });

    // Title comes from the leading path segment so it stays in sync with the URL.
    const pageTitle = () => location.pathname.replace(/^\//, "").split("/")[0] || "journal";

    return (
      <div class="app">
        <aside class="sidebar">
          <div class="brand">
            <span class="brand-logo"><Icon name="hex" size={22} /></span>
            <span class="brand-name">{props.instanceName ?? "hive"}</span>
          </div>

          <nav>
            <For each={PRIMARY}>
              {(t) => (
                <A href={`/${t.id}`} activeClass="active" end>
                  <span class="nav-icon"><Icon name={t.icon} /></span>
                  <span class="nav-label">{t.label}</span>
                  <Show when={t.id === "inbox" && (unread() ?? 0) > 0}>
                    <span class="nav-badge">{unread()}</span>
                  </Show>
                </A>
              )}
            </For>
            <button class="cmdk-hint" onClick={() => setPaletteOpen(true)} title="Command palette (⌘K)">
              <span class="nav-icon"><Icon name="more" /></span>
              <span class="nav-label">More</span>
              <kbd>⌘K</kbd>
            </button>
          </nav>

          <div class="sidebar-foot">
            <div class="foot-user">
              <span class="avatar">{initials(user?.name ?? actor())}</span>
              <span class="foot-name">{user?.name ?? actor()}</span>
              <A href="/settings" class="foot-icon" title="Settings" aria-label="Settings">
                <Icon name="settings" size={16} />
              </A>
              <Show when={isAdmin}>
                <A href="/admin" class="foot-icon" title="Admin" aria-label="Admin">
                  <Icon name="admin" size={16} />
                </A>
              </Show>
            </div>
            <button class="logout" onClick={props.onLogout}>Sign out</button>
          </div>
        </aside>

        <main>
          {/* The journal carries its own day headers; a "journal" title above
              them would just be noise. */}
          <Show when={pageTitle() !== "journal"}>
            <h2 class="page-title">{pageTitle()}</h2>
          </Show>
          {/* keyed on the leading path segment so each page remounts and re-runs
              the entrance animation when the route changes */}
          <Show when={pageTitle()} keyed>
            {(_seg) => <div class="route-view">{routeProps.children}</div>}
          </Show>
        </main>

        <CommandPalette />
      </div>
    );
  };
};

// Splash shown while the boot probe runs (and across its retries).
const Splash: Component<{ text: string }> = (props) => (
  <div class="auth-screen">
    <div class="auth-card">
      <div class="auth-brand"><span class="brand-logo"><Icon name="hex" size={28} /></span></div>
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
            <div class="auth-brand"><span class="brand-logo"><Icon name="hex" size={28} /></span></div>
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
