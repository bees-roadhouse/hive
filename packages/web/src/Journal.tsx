// Journal.tsx — WYSIWYG markdown editor (TipTap) + day-by-day feed.
//
// Authoring model (PRESERVED from the textarea version):
//   • Body is stored + submitted as MARKDOWN. The TipTap tiptap-markdown
//     extension serialises the ProseMirror doc → markdown string on submit.
//   • Select-to-tag: capture editor.state.selection text → pending anchor chip.
//     At submit, recompute {start,end} by scanning the serialised markdown with
//     body().indexOf(p.text, cursor) — identical to the original post() logic.
//   • Autocomplete (@/#/+/!/[): keydown-driven dropdown, same trigger logic.
//     On select, inserts literal `[kind: Label]` text at cursor position.
//   • Raw-markdown toggle: "rich | source" seg. Source mode = textarea bound to
//     the same markdown string; switching back calls editor.commands.setContent
//     to re-parse from markdown.
import {
  createEffect,
  createMemo,
  createSignal,
  For,
  on,
  onCleanup,
  onMount,
  Show,
  type Component,
} from "solid-js";
import type {
  AnchorKind,
  AutocompleteItem,
  JournalEntryView,
  JournalWriter,
  NewAnchor,
  Priority,
  ResolvedAnchor,
} from "@hive/shared";
import { PRIORITIES } from "@hive/shared";
import { Editor } from "@tiptap/core";
import StarterKit from "@tiptap/starter-kit";
import Link from "@tiptap/extension-link";
import { Markdown as MarkdownExt } from "tiptap-markdown";
import { api, getActor } from "./api.ts";
import { liveRev } from "./live.ts";
import { composeReq, consumeComposeRequest } from "./ui.ts";
import { ANCHOR_GLYPH, relTime } from "./lib.tsx";
import { KIND } from "./kinds.ts";
import { Icon } from "./icons.tsx";
import { JournalBody } from "./markdown.tsx";
import { EntityCard } from "./Boards.tsx";

interface Pending {
  text: string;
  kind: AnchorKind;
  title: string;
  priority: Priority;
}

const PAGE = 20;

// Namespace-filter sentinel: the active scope is either a user slug, this token
// for the global/continuous (un-owned) stream, or null for "all namespaces".
const GLOBAL_SCOPE = "global";

// Autocomplete trigger scheme:
//   @  → person
//   #  → topic
//   +  → project
//   !  → task (open)
//   [  → all kinds (person, topic, project, phase, task)
const TRIGGERS: Record<string, string[]> = {
  "@": ["person"],
  "#": ["topic"],
  "+": ["project"],
  "!": ["task"],
  "[": ["person", "topic", "project", "phase", "task"],
};

/** "Today" / "Yesterday" / "Tuesday, June 2, 2026" for a day header. */
function dayLabel(iso: string): string {
  const d = new Date(iso);
  const key = (x: Date) => `${x.getFullYear()}-${x.getMonth()}-${x.getDate()}`;
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (key(d) === key(today)) return "Today";
  if (key(d) === key(yesterday)) return "Yesterday";
  return d.toLocaleDateString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: "numeric",
  });
}

// Group a flat entry list into calendar-day buckets, newest first.
function groupByDay(
  entries: JournalEntryView[],
): { day: string; label: string; items: JournalEntryView[] }[] {
  const out: { day: string; label: string; items: JournalEntryView[] }[] = [];
  const idx = new Map<string, number>();
  for (const e of entries) {
    const day = e.created_at.slice(0, 10);
    let i = idx.get(day);
    if (i === undefined) {
      i = out.length;
      idx.set(day, i);
      out.push({ day, label: dayLabel(e.created_at), items: [] });
    }
    out[i].items.push(e);
  }
  return out;
}

// Get the currently selected plain text from a TipTap editor.
function editorSelectionText(editor: Editor): string {
  const { from, to } = editor.state.selection;
  if (from === to) return "";
  return editor.state.doc.textBetween(from, to, " ").trim();
}

// Find autocomplete trigger context walking left from caret in a string.
function findTriggerContext(
  text: string,
  caret: number,
): { trigger: string; query: string; triggerPos: number } | null {
  const limit = Math.max(0, caret - 64);
  for (let i = caret - 1; i >= limit; i--) {
    const ch = text[i];
    if (ch in TRIGGERS) {
      const before = i === 0 ? "" : text[i - 1];
      if (before && /\w/.test(before)) continue;
      return { trigger: ch, query: text.slice(i + 1, caret), triggerPos: i };
    }
    if (/\s/.test(ch) && ch !== " ") break;
  }
  return null;
}

export const Journal: Component = () => {
  // ---- editor mode ----
  // "rich"   → TipTap WYSIWYG div
  // "source" → raw textarea (markdown source)
  const [editorMode, setEditorMode] = createSignal<"rich" | "source">("rich");

  // Canonical markdown string — source of truth shared between modes.
  // Rich mode reads/writes it via TipTap; source mode reads/writes it directly.
  const [markdownBody, setMarkdownBody] = createSignal("");

  // tiptap-markdown escapes `[`/`]` (markdown link syntax), turning our entity
  // tokens into `\[person: …\]`. Restore them so the parser + feed recognise them.
  const cleanTokens = (md: string) =>
    md.replace(
      /\\\[(person|topic|project|phase|task):\s*([^\]\\]+)\\\]/gi,
      (_m, k: string, l: string) => `[${k}: ${l.trim()}]`,
    );

  // ---- pending anchors ----
  const [pending, setPending] = createSignal<Pending[]>([]);

  // selectedText: what the user has selected (drives the tag-as buttons).
  // In rich mode we read from the editor; in source mode from the textarea.
  const [selectedText, setSelectedText] = createSignal("");

  // ---- anchor drawer ----
  const [open, setOpen] = createSignal<ResolvedAnchor | null>(null);

  // ---- overlay ----
  const [overlayOpen, setOverlayOpen] = createSignal(false);

  // ---- autocomplete ----
  const [acItems, setAcItems] = createSignal<AutocompleteItem[]>([]);
  const [acTrigger, setAcTrigger] = createSignal<string | null>(null);
  const [acQuery, setAcQuery] = createSignal("");
  const [acActive, setAcActive] = createSignal(0);
  let acTimer: ReturnType<typeof setTimeout> | undefined;

  // ---- feed state: day-by-day reveal ----
  const [buffer, setBuffer] = createSignal<JournalEntryView[]>([]);
  const [fetching, setFetching] = createSignal(false);
  const [fetchDone, setFetchDone] = createSignal(false);
  const [visibleDays, setVisibleDays] = createSignal(1);

  // ---- namespace (memory-scope) filter ----
  // null = all namespaces; GLOBAL_SCOPE = continuous/un-owned; else a user slug.
  const [activeScope, setActiveScope] = createSignal<string | null>(null);
  // slug → friendly name, from the writers the viewer already sees.
  const [writers, setWriters] = createSignal<JournalWriter[]>([]);
  const scopeName = (slug: string): string =>
    writers().find((w) => w.slug === slug)?.name ?? slug;
  // Friendly label for an entry's namespace chip.
  const chipLabel = (scope: string | null | undefined): string =>
    scope ? scopeName(scope) : "Continuous";
  // AI-authored entries render in the serif voice face (see .entry-ai) so who
  // is speaking reads at a glance. Unknown authors default to the human face.
  const isAi = (author: string): boolean =>
    writers().find((w) => w.slug === author)?.kind === "ai";
  // The token a chip filters by (its entry's scope, or GLOBAL_SCOPE when null).
  const chipScope = (scope: string | null | undefined): string => scope ?? GLOBAL_SCOPE;
  const activeScopeLabel = createMemo(() => {
    const s = activeScope();
    if (s === null) return "";
    return s === GLOBAL_SCOPE ? "Continuous" : scopeName(s);
  });

  const allDayGroups = createMemo(() => groupByDay(buffer()));
  const days = createMemo(() => allDayGroups().slice(0, visibleDays()));
  const hasBufferedDays = createMemo(() => allDayGroups().length > visibleDays());
  const allDone = createMemo(() => fetchDone() && !hasBufferedDays());

  // ---- right rail: anchored entities for entries currently in view ----
  // Entry ids whose source <article> is intersecting the viewport (tracked by an
  // IntersectionObserver in onMount). The rail derives its contents from these.
  const [visibleEntryIds, setVisibleEntryIds] = createSignal<Set<string>>(new Set());
  // The entry the reader is currently centred on — its rail items get a highlight.
  const [activeEntryId, setActiveEntryId] = createSignal<string | null>(null);

  type RailItem = { anchor: ResolvedAnchor; entryId: string; title: string };

  // Anchors from in-view entries, deduped by entity (ref_id) and grouped by kind.
  // Falls back to all loaded entries before the observer has reported anything.
  const railGroups = createMemo(() => {
    const visible = visibleEntryIds();
    const entries = buffer();
    const inView = visible.size > 0 ? entries.filter((e) => visible.has(e.id)) : entries;
    const groups: Record<AnchorKind, RailItem[]> = { task: [], decision: [], event: [] };
    const seen = new Set<string>();
    for (const e of inView) {
      for (const a of e.anchors) {
        const key = a.ref_id || a.id;
        if (seen.has(key)) continue;
        seen.add(key);
        const title = a.entity?.title?.trim() || a.text;
        groups[a.kind].push({ anchor: a, entryId: e.id, title });
      }
    }
    return groups;
  });

  const railTotal = createMemo(() => {
    const g = railGroups();
    return g.task.length + g.decision.length + g.event.length;
  });

  // Click a rail item → open the same anchor drawer and scroll its source entry
  // into view (so the rail doubles as a table of contents).
  const openFromRail = (item: RailItem) => {
    setOpen(item.anchor);
    const el = document.getElementById(`entry-${item.entryId}`);
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "center" });
      el.classList.add("entry-flash");
      setTimeout(() => el.classList.remove("entry-flash"), 1200);
    }
  };

  const fetchUntilNewDay = async (): Promise<boolean> => {
    if (fetchDone()) return false;
    const startDayCount = allDayGroups().length;
    setFetching(true);
    try {
      while (true) {
        const batch = await api.journal(PAGE, buffer().length, activeScope());
        if (batch.length === 0) { setFetchDone(true); return false; }
        setBuffer((prev) => [...prev, ...batch]);
        if (batch.length < PAGE) setFetchDone(true);
        if (allDayGroups().length > startDayCount) return true;
        if (fetchDone()) return false;
      }
    } finally {
      setFetching(false);
    }
  };

  const revealNextDay = async () => {
    // reload() may still be in flight (the sentinel intersects immediately on
    // mount); starting a second fetch at the same offset would append the
    // same page twice. The sentinel re-fires on scroll, so skipping is safe.
    if (fetching()) return;
    if (hasBufferedDays()) { setVisibleDays((n) => n + 1); return; }
    const got = await fetchUntilNewDay();
    if (got) setVisibleDays((n) => n + 1);
  };

  const reload = async () => {
    setFetchDone(false);
    setBuffer([]);
    setVisibleDays(1);
    setFetching(true);
    try {
      const batch = await api.journal(PAGE, 0, activeScope());
      setBuffer(batch);
      if (batch.length < PAGE) setFetchDone(true);
    } finally {
      setFetching(false);
    }
  };

  // Apply (or clear) the namespace filter, then reload the feed from the top.
  const setScope = (scope: string | null) => {
    if (activeScope() === scope) return;
    setActiveScope(scope);
    void reload();
  };

  createEffect(on(liveRev, () => { void reload(); }, { defer: true }));

  let sentinel!: HTMLDivElement;
  let feedEl!: HTMLDivElement;

  onMount(() => {
    void reload();
    // Resolve namespace slugs → friendly names for the chips (best-effort).
    void api.journalWriters(getActor()).then(setWriters).catch(() => {});

    const scrollObs = new IntersectionObserver(
      (ents) => { if (ents.some((x) => x.isIntersecting)) void revealNextDay(); },
      { rootMargin: "300px" },
    );
    scrollObs.observe(sentinel);

    // Track which entries are on screen so the right rail can reflect them.
    // rootMargin trims the band to the middle of the viewport so the rail
    // follows what the reader is actually looking at, not edge slivers.
    const railObs = new IntersectionObserver(
      (ents) => {
        let mostVisible: { id: string; ratio: number } | null = null;
        setVisibleEntryIds((prev) => {
          const next = new Set(prev);
          for (const ent of ents) {
            const id = (ent.target as HTMLElement).dataset.entryId;
            if (!id) continue;
            if (ent.isIntersecting) next.add(id);
            else next.delete(id);
          }
          return next;
        });
        for (const ent of ents) {
          const id = (ent.target as HTMLElement).dataset.entryId;
          if (id && ent.isIntersecting && (!mostVisible || ent.intersectionRatio > mostVisible.ratio)) {
            mostVisible = { id, ratio: ent.intersectionRatio };
          }
        }
        if (mostVisible) setActiveEntryId(mostVisible.id);
      },
      { rootMargin: "-15% 0px -45% 0px", threshold: [0, 0.25, 0.5, 1] },
    );
    const observeRail = () => {
      feedEl?.querySelectorAll<HTMLElement>(".entry[data-entry-id]").forEach((el) => railObs.observe(el));
    };
    observeRail();
    const railMo = new MutationObserver(observeRail);
    railMo.observe(feedEl, { childList: true, subtree: true });
    onCleanup(() => { railObs.disconnect(); railMo.disconnect(); });

    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    let fadeObs: IntersectionObserver | undefined;
    if (!reduced) {
      fadeObs = new IntersectionObserver(
        (ents) => {
          for (const ent of ents) {
            if (ent.isIntersecting) {
              (ent.target as HTMLElement).classList.add("entry-visible");
              fadeObs!.unobserve(ent.target);
            }
          }
        },
        { threshold: 0.05 },
      );
      const observeEntries = () => {
        feedEl?.querySelectorAll(".entry:not(.entry-visible)").forEach((el) => {
          fadeObs!.observe(el);
        });
      };
      observeEntries();
      const mo = new MutationObserver(observeEntries);
      mo.observe(feedEl, { childList: true, subtree: true });
      onCleanup(() => mo.disconnect());
    }

    onCleanup(() => {
      scrollObs.disconnect();
      fadeObs?.disconnect();
    });
  });

  // ---- TipTap editor instance ----
  let editorDiv!: HTMLDivElement;
  let editor: Editor | undefined;

  // Initialise TipTap when the overlay opens (the div is mounted).
  // Destroy on overlay close (onCleanup fires when Show unmounts).
  const initEditor = () => {
    editor = new Editor({
      element: editorDiv,
      extensions: [
        StarterKit.configure({
          // tiptap-markdown handles serialisation; disable the history plugin's
          // schema additions that clash with tiptap-markdown's list handling.
          // (Keep defaults for everything else.)
        }),
        Link.configure({ openOnClick: false, HTMLAttributes: { rel: "noopener noreferrer" } }),
        MarkdownExt.configure({
          html: false,          // Never accept raw HTML from the user.
          tightLists: true,     // Keep tight lists (no extra <p> inside <li>).
          transformPastedText: true, // Parse markdown on paste.
          transformCopiedText: false,
        }),
      ],
      content: markdownBody() || "",
      editorProps: {
        attributes: {
          class: "ProseMirror wysiwyg-editor",
          "aria-label": "Journal entry editor",
        },
      },
      onUpdate({ editor: ed }) {
        // Keep markdownBody in sync so submit always has fresh source.
        const md = (ed.storage.markdown as { getMarkdown(): string }).getMarkdown();
        setMarkdownBody(cleanTokens(md));

        // Drive autocomplete on content change (same as textarea's onInput).
        const { from } = ed.state.selection;
        const textBefore = ed.state.doc.textBetween(0, from, "\n");
        const ctx = findTriggerContext(textBefore, textBefore.length);
        if (!ctx) { dismissAc(); return; }
        // Don't re-fire if query unchanged.
        if (ctx.query === acQuery() && ctx.trigger === acTrigger()) return;
        setAcTrigger(ctx.trigger);
        setAcQuery(ctx.query);
        setAcActive(0);
        clearTimeout(acTimer);
        acTimer = setTimeout(async () => {
          try {
            const kinds = TRIGGERS[ctx.trigger] ?? ["person"];
            const items = await api.autocomplete(ctx.query, kinds);
            setAcItems(items);
            setAcActive(0);
          } catch { setAcItems([]); }
        }, 120);
      },
      onSelectionUpdate({ editor: ed }) {
        // Track selected text for the select-to-tag feature.
        setSelectedText(editorSelectionText(ed));
      },
    });
  };

  const destroyEditor = () => {
    editor?.destroy();
    editor = undefined;
  };

  // ---- raw textarea ref (source mode only) ----
  let sourceTA!: HTMLTextAreaElement;
  const trackSourceSel = () => {
    if (!sourceTA) return;
    const t = markdownBody().slice(sourceTA.selectionStart, sourceTA.selectionEnd).trim();
    setSelectedText(t);
  };

  // ---- mode switching ----
  const switchToSource = () => {
    // Flush editor markdown to signal before swapping mode.
    if (editor) {
      const md = (editor.storage.markdown as { getMarkdown(): string }).getMarkdown();
      setMarkdownBody(cleanTokens(md));
    }
    setEditorMode("source");
    queueMicrotask(() => { if (sourceTA) sourceTA.focus(); });
  };

  const switchToRich = () => {
    setEditorMode("rich");
    // Re-parse current markdown into the editor after it mounts.
    // The editor div remounts inside Show, so we wait a tick.
    queueMicrotask(() => {
      if (editor) {
        editor.commands.setContent(markdownBody(), false);
        editor.commands.focus("end");
      }
    });
  };

  // ---- overlay open/close ----
  const openOverlay = () => {
    setOverlayOpen(true);
    setEditorMode("rich");
    // Editor is mounted inside Show; give DOM a tick.
    queueMicrotask(() => { editor?.commands.focus("end"); });
  };

  const closeOverlay = () => {
    setOverlayOpen(false);
    dismissAc();
  };

  // The command palette's "New entry" action lands here (see ui.ts). The
  // deferred effect covers a palette fired while the journal is mounted; the
  // mount-time consume covers "New entry" from another route, where the bump
  // happens before this component (and the listener) exists.
  createEffect(on(composeReq, () => { if (consumeComposeRequest()) openOverlay(); }, { defer: true }));
  onMount(() => { if (consumeComposeRequest()) openOverlay(); });

  // ---- autocomplete ----
  const dismissAc = () => {
    setAcItems([]);
    setAcTrigger(null);
    setAcQuery("");
    setAcActive(0);
    clearTimeout(acTimer);
  };

  // Insert `[kind: Label]` literal token at the current cursor position.
  // Works in both rich mode (TipTap insertContent) and source mode (textarea).
  const selectAcItem = (item: AutocompleteItem) => {
    const token = `[${item.kind}: ${item.label}]`;
    if (editorMode() === "rich" && editor) {
      const { from } = editor.state.selection;
      const textBefore = editor.state.doc.textBetween(0, from, "\n");
      const ctx = findTriggerContext(textBefore, textBefore.length);
      if (ctx) {
        // Delete from triggerPos to cursor, then insert token.
        const deleteFrom = from - (textBefore.length - ctx.triggerPos);
        editor
          .chain()
          .focus()
          .deleteRange({ from: deleteFrom, to: from })
          .insertContent(token)
          .run();
      } else {
        editor.chain().focus().insertContent(token).run();
      }
    } else if (editorMode() === "source" && sourceTA) {
      const val = markdownBody();
      const caret = sourceTA.selectionStart;
      const ctx = findTriggerContext(val, caret);
      if (ctx) {
        const before = val.slice(0, ctx.triggerPos);
        const after = val.slice(caret);
        const next = `${before}${token}${after}`;
        setMarkdownBody(next);
        queueMicrotask(() => {
          const pos = before.length + token.length;
          sourceTA.selectionStart = sourceTA.selectionEnd = pos;
          sourceTA.focus();
        });
      } else {
        const before = val.slice(0, caret);
        const after = val.slice(caret);
        const next = `${before}${token}${after}`;
        setMarkdownBody(next);
      }
    }
    dismissAc();
  };

  // keydown for source-mode textarea (autocomplete nav + Esc).
  const onSourceKeyDown = (e: KeyboardEvent) => {
    if (acItems().length) {
      if (e.key === "ArrowDown") { e.preventDefault(); setAcActive((i) => Math.min(i + 1, acItems().length - 1)); return; }
      if (e.key === "ArrowUp")   { e.preventDefault(); setAcActive((i) => Math.max(i - 1, 0)); return; }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const item = acItems()[acActive()];
        if (item) selectAcItem(item);
        return;
      }
      if (e.key === "Escape") { dismissAc(); return; }
    }
    if (e.key === "Escape") { e.stopPropagation(); closeOverlay(); }
  };

  // keydown for rich mode (autocomplete nav inside the editor).
  // We hook this via a DOM keydown listener on the editor element.
  const onRichKeyDown = (e: KeyboardEvent) => {
    if (acItems().length) {
      if (e.key === "ArrowDown") { e.preventDefault(); setAcActive((i) => Math.min(i + 1, acItems().length - 1)); return; }
      if (e.key === "ArrowUp")   { e.preventDefault(); setAcActive((i) => Math.max(i - 1, 0)); return; }
      if (e.key === "Enter" || e.key === "Tab") {
        // Only intercept if dropdown is showing — let normal Enter/Tab through otherwise.
        e.preventDefault();
        const item = acItems()[acActive()];
        if (item) selectAcItem(item);
        return;
      }
      if (e.key === "Escape") { dismissAc(); return; }
    }
    if (e.key === "Escape") { e.stopPropagation(); closeOverlay(); }
  };

  // Also fire autocomplete on input in source mode.
  const onSourceInput = (e: InputEvent & { currentTarget: HTMLTextAreaElement }) => {
    const val = e.currentTarget.value;
    setMarkdownBody(val);
    trackSourceSel();
    const caret = e.currentTarget.selectionStart;
    const ctx = findTriggerContext(val, caret);
    if (!ctx) { dismissAc(); return; }
    setAcTrigger(ctx.trigger);
    setAcQuery(ctx.query);
    setAcActive(0);
    clearTimeout(acTimer);
    acTimer = setTimeout(async () => {
      try {
        const kinds = TRIGGERS[ctx.trigger] ?? ["person"];
        const items = await api.autocomplete(ctx.query, kinds);
        setAcItems(items);
        setAcActive(0);
      } catch { setAcItems([]); }
    }, 120);
  };

  // ---- select-to-tag ----
  const mark = (kind: AnchorKind) => {
    const text = selectedText();
    if (!text) return;
    setPending([
      ...pending(),
      { text, kind, title: text.split(/[.\n]/)[0].slice(0, 80), priority: "normal" },
    ]);
  };
  const removePending = (i: number) => setPending(pending().filter((_, j) => j !== i));

  // ---- toolbar commands (rich mode only) ----
  const toolBold   = () => editor?.chain().focus().toggleBold().run();
  const toolItalic = () => editor?.chain().focus().toggleItalic().run();
  const toolCode   = () => editor?.chain().focus().toggleCode().run();
  const toolHeading = () => editor?.chain().focus().toggleHeading({ level: 2 }).run();
  const toolBullet  = () => editor?.chain().focus().toggleBulletList().run();
  // Checkbox: insert markdown literal since TaskList extension is not loaded.
  const toolCheckbox = () => editor?.chain().focus().insertContent("- [ ] ").run();
  const toolQuote  = () => editor?.chain().focus().toggleBlockquote().run();
  const toolLink   = () => {
    const url = window.prompt("URL:");
    if (url) editor?.chain().focus().setLink({ href: url }).run();
  };

  // ---- submit ----
  const post = async () => {
    const md = cleanTokens(
      editorMode() === "rich"
        ? (editor?.storage.markdown as { getMarkdown(): string } | undefined)?.getMarkdown() ?? markdownBody()
        : markdownBody(),
    );
    if (!md.trim()) return;

    // Recompute anchor offsets from the serialised markdown at submit time.
    // Identical logic to the original post() — never desync on edits.
    let cursor = 0;
    const anchors: NewAnchor[] = [];
    for (const p of pending()) {
      const start = md.indexOf(p.text, cursor);
      if (start === -1) continue;
      const end = start + p.text.length;
      cursor = end;
      anchors.push({
        start,
        end,
        kind: p.kind,
        fields: { title: p.title, ...(p.kind === "task" ? { priority: p.priority } : {}) },
      });
    }

    await api.append({ author: getActor(), body: md, anchors });

    // Reset state.
    setMarkdownBody("");
    setPending([]);
    setSelectedText("");
    dismissAc();
    if (editor) {
      editor.commands.setContent("", false);
    }
    setOverlayOpen(false);
    await reload();
  };

  // ---- render ----
  return (
    <section class="journal">
      {/* ---- active namespace filter (only when one is set) ---- */}
      <Show when={activeScope() !== null}>
        <div class="journal-header">
          <div class="ns-filter-bar">
            <span class="dim sm">namespace</span>
            <span class="ns-chip ns-chip-active">◆ {activeScopeLabel()}</span>
            <button class="ns-clear" onClick={() => setScope(null)} title="Show all namespaces">
              ✕ all namespaces
            </button>
          </div>
        </div>
      </Show>

      {/* ---- feed + anchored-entities rail ---- */}
      <div class="journal-layout">
      <div ref={feedEl} class="feed">
        <For each={days()}>
          {(d) => (
            <section class="day">
              <header class="day-head">
                <Icon name="hex" class="comb" />
                <h3>{d.label}</h3>
                <span class="dim sm">
                  {d.items.length} {d.items.length > 1 ? "entries" : "entry"}
                </span>
              </header>
              <For each={d.items}>
                {(e) => (
                  <article
                    class="entry entry-fade"
                    classList={{ "entry-ai": isAi(e.author) }}
                    id={`entry-${e.id}`}
                    data-entry-id={e.id}
                  >
                    <header>
                      <span class="actor-chip">{e.author}</span>
                      <button
                        type="button"
                        class="ns-chip"
                        classList={{ "ns-chip-global": !e.user_scope }}
                        title={`Filter to ${chipLabel(e.user_scope)}`}
                        onClick={() => setScope(chipScope(e.user_scope))}
                      >
                        ◆ {chipLabel(e.user_scope)}
                      </button>
                      <time>{relTime(e.created_at)}</time>
                      <Show when={e.anchors.length}>
                        <span class="dim">
                          · {e.anchors.length} anchor{e.anchors.length > 1 ? "s" : ""}
                        </span>
                      </Show>
                    </header>
                    <JournalBody body={e.body} anchors={e.anchors} refs={e.refs} onAnchor={setOpen} />
                  </article>
                )}
              </For>
            </section>
          )}
        </For>

        <div ref={sentinel} class="sentinel">
          <Show when={fetching()}>
            <span class="dim sm">gathering…</span>
          </Show>
          <Show when={allDone() && buffer().length > 0}>
            <span class="dim sm">— the first cell —</span>
          </Show>
          <Show when={allDone() && buffer().length === 0}>
            <span class="dim sm">no entries yet — write the first one.</span>
          </Show>
        </div>
      </div>

      {/* ---- right rail: entities anchored in the entries currently in view ---- */}
      <aside class="journal-rail">
        <div class="rail-head">
          <span class="dim sm">in view</span>
          <Show when={railTotal() > 0}>
            <span class="badge">{railTotal()}</span>
          </Show>
        </div>
        <Show
          when={railTotal() > 0}
          fallback={<p class="dim sm rail-empty">No anchored tasks, decisions, or events in the entries on screen. Tag text as you write to surface them here.</p>}
        >
          <For each={["task", "decision", "event"] as AnchorKind[]}>
            {(kind) => (
              <Show when={railGroups()[kind].length > 0}>
                <div class="rail-group">
                  <h4 class="rail-group-head">
                    <span class={`rail-glyph rail-glyph-${kind}`}>{ANCHOR_GLYPH[kind]}</span>
                    {kind === "task" ? "Tasks" : kind === "decision" ? "Decisions" : "Events"}
                    <span class="dim sm">{railGroups()[kind].length}</span>
                  </h4>
                  <ul class="rail-list">
                    <For each={railGroups()[kind]}>
                      {(item) => (
                        <li
                          class={`rail-item rail-item-${kind}`}
                          classList={{ "rail-item-active": item.entryId === activeEntryId() }}
                          onClick={() => openFromRail(item)}
                          title={item.title}
                        >
                          {item.title}
                        </li>
                      )}
                    </For>
                  </ul>
                </div>
              </Show>
            )}
          </For>
        </Show>
      </aside>
      </div>

      {/* ---- write bar: the always-there composer entry point, pinned to the
              bottom of the viewport like a chat input. Clicking anywhere on it
              opens the full overlay composer (rich editor, anchors, tags). ---- */}
      <div class="write-bar">
        <div
          class="write-bar-inner"
          role="button"
          tabindex="0"
          aria-label="New entry"
          onClick={openOverlay}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              openOverlay();
            }
          }}
        >
          <span class="write-hint">Write to the hive…</span>
          <span class="write-glyph" title="@name mentions people">@</span>
          <span class="write-glyph" title="#topic +project !task tags">#</span>
          <span class="write-send" aria-hidden="true">↑</span>
        </div>
      </div>

      {/* ---- new-entry overlay ---- */}
      <Show when={overlayOpen()}>
        <div class="overlay-backdrop" onClick={closeOverlay}>
          <div class="overlay-panel" onClick={(ev) => ev.stopPropagation()}>
            {/* header row */}
            <div class="overlay-head">
              <span class="overlay-title">new entry</span>

              {/* rich | source mode toggle */}
              <div class="seg wysiwyg-mode-seg">
                <button
                  classList={{ active: editorMode() === "rich" }}
                  onClick={switchToRich}
                  title="WYSIWYG editor"
                >
                  rich
                </button>
                <button
                  classList={{ active: editorMode() === "source" }}
                  onClick={switchToSource}
                  title="Raw markdown source"
                >
                  {"</>"} source
                </button>
              </div>

              <button class="x" onClick={closeOverlay} title="Close (Esc)">✕</button>
            </div>

            {/* hint */}
            <div class="composer-hint overlay-hint">
              Write in <strong>markdown</strong>. Use{" "}
              <code class="trigger-hint">@name</code> for people,{" "}
              <code class="trigger-hint">#topic</code>,{" "}
              <code class="trigger-hint">+project</code>,{" "}
              <code class="trigger-hint">!task</code>, or{" "}
              <code class="trigger-hint">[kind: …]</code> for any entity.
              Select text to tag as a task, decision, or event.
            </div>

            {/* ---- rich mode: TipTap WYSIWYG ---- */}
            <Show when={editorMode() === "rich"}>
              {/* toolbar */}
              <div class="md-toolbar">
                <button title="bold"         onClick={toolBold}>   <b>B</b>     </button>
                <button title="italic"        onClick={toolItalic}> <i>I</i>     </button>
                <button title="inline code"   onClick={toolCode}>   {"</>"}     </button>
                <button title="heading"       onClick={toolHeading}>H            </button>
                <button title="bullet list"   onClick={toolBullet}> •            </button>
                <button title="checkbox task" onClick={toolCheckbox}><Icon name="tasks" size={14} /></button>
                <button title="blockquote"    onClick={toolQuote}><Icon name="quote" size={14} /></button>
                <button title="link"          onClick={toolLink}><Icon name="link" size={14} /></button>
              </div>

              {/* TipTap mount point + autocomplete dropdown */}
              <div class="composer-editor-wrap wysiwyg-wrap">
                <div
                  ref={(el) => {
                    editorDiv = el;
                    // onMount fires after ref assignment; initEditor uses editorDiv.
                    queueMicrotask(() => {
                      if (!editor) {
                        initEditor();
                        // Attach keydown handler for AC nav and Esc.
                        editorDiv?.addEventListener("keydown", onRichKeyDown);
                        onCleanup(() => {
                          editorDiv?.removeEventListener("keydown", onRichKeyDown);
                          destroyEditor();
                        });
                        // Set initial content if returning from source mode.
                        if (markdownBody()) {
                          editor?.commands.setContent(markdownBody(), false);
                        }
                        editor?.commands.focus("end");
                      }
                    });
                  }}
                  class="wysiwyg-host"
                  aria-multiline="true"
                />
                <Show when={acItems().length > 0}>
                  <ul class="ac-dropdown" role="listbox">
                    <For each={acItems()}>
                      {(item, i) => (
                        <li
                          class="ac-item"
                          classList={{ "ac-item-active": i() === acActive() }}
                          role="option"
                          aria-selected={i() === acActive()}
                          onMouseDown={(e) => { e.preventDefault(); selectAcItem(item); }}
                          onMouseEnter={() => setAcActive(i())}
                        >
                          <span class="ac-kind-glyph">{KIND[item.kind]?.glyph ?? "·"}</span>
                          <span class="ac-label">{item.label}</span>
                          <span class="ac-kind dim">{item.kind}</span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </div>
            </Show>

            {/* ---- source mode: raw textarea ---- */}
            <Show when={editorMode() === "source"}>
              <div class="composer-editor-wrap">
                <textarea
                  ref={sourceTA}
                  class="composer-ta overlay-ta"
                  placeholder="Raw markdown source — switch to rich to see it rendered."
                  value={markdownBody()}
                  onInput={onSourceInput}
                  onSelect={trackSourceSel}
                  onMouseUp={trackSourceSel}
                  onKeyUp={trackSourceSel}
                  onKeyDown={onSourceKeyDown}
                />
                <Show when={acItems().length > 0}>
                  <ul class="ac-dropdown" role="listbox">
                    <For each={acItems()}>
                      {(item, i) => (
                        <li
                          class="ac-item"
                          classList={{ "ac-item-active": i() === acActive() }}
                          role="option"
                          aria-selected={i() === acActive()}
                          onMouseDown={(e) => { e.preventDefault(); selectAcItem(item); }}
                          onMouseEnter={() => setAcActive(i())}
                        >
                          <span class="ac-kind-glyph">{KIND[item.kind]?.glyph ?? "·"}</span>
                          <span class="ac-label">{item.label}</span>
                          <span class="ac-kind dim">{item.kind}</span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </div>
            </Show>

            {/* bottom bar: select-to-tag + submit */}
            <div class="composer-bar">
              <div class="mark-group">
                <span class="dim">
                  {selectedText() ? `tag "${selectedText().slice(0, 28)}…" as` : "select text to tag"}
                </span>
                <For each={["task", "decision", "event"] as AnchorKind[]}>
                  {(k) => (
                    <button disabled={!selectedText()} onClick={() => mark(k)}>
                      {ANCHOR_GLYPH[k]} {k}
                    </button>
                  )}
                </For>
              </div>
              <button class="primary" disabled={!markdownBody().trim()} onClick={post}>
                write entry
              </button>
            </div>

            {/* pending anchors */}
            <Show when={pending().length}>
              <div class="pending">
                <For each={pending()}>
                  {(p, i) => (
                    <div class={`chip chip-${p.kind}`}>
                      <span>{ANCHOR_GLYPH[p.kind]}</span>
                      <input value={p.title} onInput={(e) => (p.title = e.currentTarget.value)} />
                      <Show when={p.kind === "task"}>
                        <select onChange={(e) => (p.priority = e.currentTarget.value as Priority)}>
                          <For each={PRIORITIES}>
                            {(pr) => (
                              <option value={pr} selected={pr === "normal"}>
                                {pr}
                              </option>
                            )}
                          </For>
                        </select>
                      </Show>
                      <button class="x" onClick={() => removePending(i())}>
                        ✕
                      </button>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </div>
        </div>
      </Show>

      {/* ---- anchor drawer ---- */}
      <Show when={open()}>
        {(a) => (
          <div class="drawer-backdrop" onClick={() => setOpen(null)}>
            <div class="drawer" onClick={(ev) => ev.stopPropagation()}>
              <div class="drawer-head">
                <span class="badge">{a().kind}</span>
                <button class="x" onClick={() => setOpen(null)}>
                  ✕
                </button>
              </div>
              <blockquote>"{a().text}"</blockquote>
              <EntityCard kind={a().kind} entity={a().entity} />
            </div>
          </div>
        )}
      </Show>
    </section>
  );
};
