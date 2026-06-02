import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTORS } from "@hive/shared";
import { api, getActor, setActor } from "./api.ts";
import { Journal } from "./Journal.tsx";
import { Inbox } from "./Inbox.tsx";
import { Dashboard } from "./Dashboard.tsx";
import { Settings } from "./Settings.tsx";
import { Decisions, Events, SearchPane, Tasks, Wire } from "./Boards.tsx";

const TABS = [
  { id: "journal", icon: "📓" },
  { id: "inbox", icon: "📥" },
  { id: "dashboard", icon: "📊" },
  { id: "tasks", icon: "◻" },
  { id: "decisions", icon: "◆" },
  { id: "events", icon: "◷" },
  { id: "search", icon: "⌕" },
  { id: "wire", icon: "⚡" },
  { id: "settings", icon: "⚙" },
] as const;
type Tab = (typeof TABS)[number]["id"];

export const App: Component = () => {
  const [tab, setTab] = createSignal<Tab>("journal");
  const [actor, setActorState] = createSignal(getActor());

  // Live unread count for the current actor; refetches when the actor changes.
  const [unread] = createResource(actor, async (a) => (await api.inbox(a, true)).length);

  const onActor = (a: string) => {
    setActor(a);
    setActorState(a);
  };

  return (
    <div class="app">
      <aside class="sidebar">
        <div class="brand">
          <span class="logo">🐝</span>
          <span class="brand-name">hive</span>
        </div>

        <nav>
          <For each={TABS}>
            {(t) => (
              <button classList={{ active: tab() === t.id }} onClick={() => setTab(t.id)}>
                <span class="nav-icon">{t.icon}</span>
                <span class="nav-label">{t.id}</span>
                <Show when={t.id === "inbox" && (unread() ?? 0) > 0}>
                  <span class="nav-badge">{unread()}</span>
                </Show>
              </button>
            )}
          </For>
        </nav>

        <div class="sidebar-foot">
          <label class="actor">
            <span class="dim">acting as</span>
            <select value={actor()} onChange={(e) => onActor(e.currentTarget.value)}>
              <For each={ACTORS}>
                {(a) => (
                  <option value={a.name}>
                    {a.name} ({a.kind})
                  </option>
                )}
              </For>
            </select>
          </label>
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
        <Show when={tab() === "search"}><SearchPane /></Show>
        <Show when={tab() === "wire"}><Wire /></Show>
        <Show when={tab() === "settings"}><Settings /></Show>
      </main>
    </div>
  );
};
