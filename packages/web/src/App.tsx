import { createResource, createSignal, ErrorBoundary, For, Show, Suspense, type Component } from "solid-js";
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

// The signed-in workspace (rendered only after auth resolves).
const Workspace: Component<{ instanceName: string | null; onLogout: () => void }> = (props) => {
  const [tab, setTab] = createSignal<Tab>("journal");
  const user = getCurrentUser();
  const isAdmin = user?.role === "admin";
  const actor = () => getActor();
  connectLive(); // open the SSE stream now that we're authenticated

  const [unread] = createResource(
    () => ({ actor: actor(), _r: liveRev() }),
    (k) => api.inbox(k.actor, true).then((items) => items.length),
  );

  // The account tab (user + token admin) is admin-only.
  const visibleTabs = TABS.filter((t) => t.id !== "account" || isAdmin);

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
              <button classList={{ active: tab() === t.id }} onClick={() => setTab(t.id)}>
                <span class="nav-icon"><Icon name={t.id === "account" ? "settings" : t.id} /></span>
                <span class="nav-label">{t.id}</span>
                <Show when={t.id === "inbox" && (unread() ?? 0) > 0}>
                  <span class="nav-badge">{unread()}</span>
                </Show>
              </button>
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
        <h2 class="page-title">{tab()}</h2>
        <Show when={tab() === "journal"}><Journal /></Show>
        <Show when={tab() === "inbox"}><Inbox /></Show>
        <Show when={tab() === "dashboard"}><Dashboard /></Show>
        <Show when={tab() === "tasks"}><Tasks /></Show>
        <Show when={tab() === "decisions"}><Decisions /></Show>
        <Show when={tab() === "events"}><Events /></Show>
        <Show when={tab() === "people"}><PeopleView /></Show>
        <Show when={tab() === "topics"}><TopicsView /></Show>
        <Show when={tab() === "projects"}><ProjectsView /></Show>
        <Show when={tab() === "graph"}><Graph /></Show>
        <Show when={tab() === "search"}><SearchPane /></Show>
        <Show when={tab() === "wire"}><Wire /></Show>
        <Show when={tab() === "admin"}><Admin /></Show>
        <Show when={tab() === "account" && isAdmin}><Account /></Show>
        <Show when={tab() === "settings"}><Settings /></Show>
      </main>
    </div>
  );
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
            <Workspace instanceName={boot()?.status.instanceName ?? null} onLogout={onLogout} />
          </Show>
        </Show>
      </Suspense>
    </ErrorBoundary>
  );
};
