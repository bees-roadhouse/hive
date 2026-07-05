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
  /* Section header the command sits under when the list is unfiltered.
     The free-text search fallback carries none. */
  section?: string;
  keywords?: string;
  adminOnly?: boolean;
  run: (navigate: (to: string) => void) => void;
}

// Declaration order is display order; sections must be contiguous because
// headers are emitted whenever the section name changes while rendering.
const COMMANDS: Cmd[] = [
  {
    id: "new-entry",
    label: "New entry",
    hint: "write to the hive",
    icon: "journal",
    section: "Actions",
    keywords: "write compose journal",
    run: (nav) => {
      nav("/journal");
      requestCompose();
    },
  },
  { id: "journal", label: "Today", hint: "journal feed", icon: "journal", section: "Go to", keywords: "journal home", run: (nav) => nav("/journal") },
  { id: "inbox", label: "Inbox", hint: "mentions + assignments", icon: "inbox", section: "Go to", run: (nav) => nav("/inbox") },
  { id: "search", label: "Search", hint: "keyword + semantic", icon: "search", section: "Go to", run: (nav) => nav("/search") },
  { id: "workspaces", label: "Workspaces", hint: "hosted Claude Code sessions", icon: "workspaces", section: "Go to", keywords: "claude code terminal", run: (nav) => nav("/workspaces") },
  { id: "tasks", label: "Tasks", hint: "board", icon: "tasks", section: "Boards", run: (nav) => nav("/tasks") },
  { id: "decisions", label: "Decisions", hint: "log", icon: "decisions", section: "Boards", run: (nav) => nav("/decisions") },
  { id: "events", label: "Events", hint: "log", icon: "events", section: "Boards", run: (nav) => nav("/events") },
  { id: "people", label: "People", hint: "humans + AIs", icon: "people", section: "Boards", run: (nav) => nav("/people") },
  { id: "topics", label: "Topics", hint: "tag index", icon: "topics", section: "Boards", run: (nav) => nav("/topics") },
  { id: "projects", label: "Projects", hint: "with phases", icon: "projects", section: "Boards", run: (nav) => nav("/projects") },
  { id: "wire", label: "Wire", hint: "activity + feeds", icon: "wire", section: "System", keywords: "news rss activity", run: (nav) => nav("/wire") },
  { id: "graph", label: "Graph", hint: "knowledge graph", icon: "graph", section: "System", run: (nav) => nav("/graph") },
  { id: "dashboard", label: "Dashboard", hint: "instance stats", icon: "dashboard", section: "System", run: (nav) => nav("/dashboard") },
  { id: "settings", label: "Settings", hint: "preferences", icon: "settings", section: "Manage", run: (nav) => nav("/settings") },
  { id: "account", label: "Account", hint: "users + tokens", icon: "account", section: "Manage", adminOnly: true, run: (nav) => nav("/account") },
  { id: "admin", label: "Admin", hint: "sources, import, actors", icon: "admin", section: "Manage", adminOnly: true, run: (nav) => nav("/admin") },
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

  // What the list renders: section headers interleaved while browsing, a flat
  // match list while filtering (headers between two hits are just noise).
  // items() stays the keyboard-order truth; idx points back into it.
  type Row = { sec: string } | { cmd: Cmd; idx: number };
  const rows = createMemo((): Row[] => {
    const list = items();
    if (query().trim()) return list.map((cmd, idx) => ({ cmd, idx }));
    const out: Row[] = [];
    let sec = "";
    list.forEach((cmd, idx) => {
      if (cmd.section && cmd.section !== sec) {
        sec = cmd.section;
        out.push({ sec });
      }
      out.push({ cmd, idx });
    });
    return out;
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
            <For each={rows()}>
              {(row) =>
                "sec" in row ? (
                  <li class="cmdk-sec" role="presentation">{row.sec}</li>
                ) : (
                  <li
                    class="cmdk-item"
                    classList={{ "cmdk-item-active": row.idx === active() }}
                    role="option"
                    aria-selected={row.idx === active()}
                    onMouseEnter={() => setActive(row.idx)}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      runItem(row.cmd);
                    }}
                  >
                    <span class="cmdk-icon"><Icon name={row.cmd.icon} size={16} /></span>
                    <span class="cmdk-label">{row.cmd.label}</span>
                    <span class="cmdk-item-hint dim">{row.cmd.hint}</span>
                  </li>
                )
              }
            </For>
          </ul>
        </div>
      </div>
    </Show>
  );
};
