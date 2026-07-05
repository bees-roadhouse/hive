// CommandPalette.tsx — the ⌘K surface that replaced the 12 demoted nav tabs.
//
// Every route stays registered in App.tsx (deep links and back/forward keep
// working); this is how you reach the ones that no longer earn a sidebar slot.
// Items are plain navigations plus a couple of actions (new entry, search
// passthrough). Filtering is a substring match over label + keywords — no
// fuzzy scoring, the list is small.

import { createEffect, createMemo, createSignal, For, on, Show, type Component } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { getCurrentUser } from "./api.ts";
import { Icon } from "./icons.tsx";
import { paletteOpen, requestCompose, setPaletteOpen } from "./ui.ts";

interface Cmd {
  id: string;
  label: string;
  hint: string;
  icon: string;
  keywords?: string;
  adminOnly?: boolean;
  run: (navigate: (to: string) => void) => void;
}

const COMMANDS: Cmd[] = [
  {
    id: "new-entry",
    label: "New entry",
    hint: "write to the hive",
    icon: "journal",
    keywords: "write compose journal",
    run: (nav) => {
      nav("/journal");
      requestCompose();
    },
  },
  { id: "journal", label: "Today", hint: "journal feed", icon: "journal", keywords: "journal home", run: (nav) => nav("/journal") },
  { id: "inbox", label: "Inbox", hint: "mentions + assignments", icon: "inbox", run: (nav) => nav("/inbox") },
  { id: "search", label: "Search", hint: "keyword + semantic", icon: "search", run: (nav) => nav("/search") },
  { id: "workspaces", label: "Workspaces", hint: "hosted Claude Code sessions", icon: "workspaces", keywords: "claude code terminal", run: (nav) => nav("/workspaces") },
  { id: "tasks", label: "Tasks", hint: "board", icon: "tasks", run: (nav) => nav("/tasks") },
  { id: "decisions", label: "Decisions", hint: "log", icon: "decisions", run: (nav) => nav("/decisions") },
  { id: "events", label: "Events", hint: "log", icon: "events", run: (nav) => nav("/events") },
  { id: "people", label: "People", hint: "humans + AIs", icon: "people", run: (nav) => nav("/people") },
  { id: "topics", label: "Topics", hint: "tag index", icon: "topics", run: (nav) => nav("/topics") },
  { id: "projects", label: "Projects", hint: "with phases", icon: "projects", run: (nav) => nav("/projects") },
  { id: "graph", label: "Graph", hint: "knowledge graph", icon: "graph", run: (nav) => nav("/graph") },
  { id: "wire", label: "Wire", hint: "activity + feeds", icon: "wire", keywords: "news rss activity", run: (nav) => nav("/wire") },
  { id: "dashboard", label: "Dashboard", hint: "instance stats", icon: "dashboard", run: (nav) => nav("/dashboard") },
  { id: "account", label: "Account", hint: "users + tokens", icon: "account", adminOnly: true, run: (nav) => nav("/account") },
  { id: "admin", label: "Admin", hint: "sources, import, actors", icon: "admin", adminOnly: true, run: (nav) => nav("/admin") },
  { id: "settings", label: "Settings", hint: "preferences", icon: "settings", run: (nav) => nav("/settings") },
];

export const CommandPalette: Component = () => {
  const navigate = useNavigate();
  const [query, setQuery] = createSignal("");
  const [active, setActive] = createSignal(0);
  let inputEl: HTMLInputElement | undefined;

  const close = () => setPaletteOpen(false);

  // Reset + focus each time the palette opens.
  createEffect(
    on(paletteOpen, (open) => {
      if (open) {
        setQuery("");
        setActive(0);
        queueMicrotask(() => inputEl?.focus());
      }
    }),
  );

  const isAdmin = () => getCurrentUser()?.role === "admin";

  const matches = createMemo(() => {
    const q = query().trim().toLowerCase();
    return COMMANDS.filter((c) => {
      if (c.adminOnly && !isAdmin()) return false;
      if (!q) return true;
      return `${c.label} ${c.hint} ${c.keywords ?? ""}`.toLowerCase().includes(q);
    });
  });

  // Free-text fallback: whatever was typed can always become a search.
  const searchCmd = createMemo((): Cmd | null => {
    const q = query().trim();
    if (!q) return null;
    return {
      id: "search-for",
      label: `Search "${q}"`,
      hint: "keyword + semantic",
      icon: "search",
      run: (nav) => nav(`/search?q=${encodeURIComponent(q)}`),
    };
  });

  const items = createMemo(() => {
    const s = searchCmd();
    return s ? [...matches(), s] : matches();
  });

  const runItem = (c: Cmd) => {
    close();
    c.run(navigate);
  };

  const onKeyDown = (e: KeyboardEvent) => {
    const list = items();
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => Math.min(i + 1, list.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = list[active()];
      if (item) runItem(item);
    } else if (e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };

  return (
    <Show when={paletteOpen()}>
      <div class="cmdk-backdrop" onClick={close}>
        <div class="cmdk" role="dialog" aria-label="Command palette" onClick={(ev) => ev.stopPropagation()}>
          <input
            ref={inputEl}
            class="cmdk-input"
            placeholder="Go to, or search…"
            value={query()}
            onInput={(e) => {
              setQuery(e.currentTarget.value);
              setActive(0);
            }}
            onKeyDown={onKeyDown}
            aria-label="Command palette input"
          />
          <ul class="cmdk-list" role="listbox">
            <For each={items()}>
              {(c, i) => (
                <li
                  class="cmdk-item"
                  classList={{ "cmdk-item-active": i() === active() }}
                  role="option"
                  aria-selected={i() === active()}
                  onMouseEnter={() => setActive(i())}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    runItem(c);
                  }}
                >
                  <span class="cmdk-icon"><Icon name={c.icon} size={16} /></span>
                  <span class="cmdk-label">{c.label}</span>
                  <span class="cmdk-item-hint dim">{c.hint}</span>
                </li>
              )}
            </For>
          </ul>
        </div>
      </div>
    </Show>
  );
};
