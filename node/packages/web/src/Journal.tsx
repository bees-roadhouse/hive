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
import { ANCHOR_GLYPH, relTime } from "./lib.tsx";
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

const KIND_GLYPH: Record<string, string> = {
  person: "👤",
  topic: "#",
  project: "◈",
  phase: "◷",
  task: "◻",
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

  const allDayGroups = createMemo(() => groupByDay(buffer()));
  const days = createMemo(() => allDayGroups().slice(0, visibleDays()));
  const hasBufferedDays = createMemo(() => allDayGroups().length > visibleDays());
  const allDone = createMemo(() => fetchDone() && !hasBufferedDays());

  const fetchUntilNewDay = async (): Promise<boolean> => {
    if (fetchDone()) return false;
    const startDayCount = allDayGroups().length;
    setFetching(true);
    try {
      while (true) {
        const batch = await api.journal(PAGE, buffer().length);
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
      const batch = await api.journal(PAGE, 0);
      setBuffer(batch);
      if (batch.length < PAGE) setFetchDone(true);
    } finally {
      setFetching(false);
    }
  };

  createEffect(on(liveRev, () => { void reload(); }, { defer: true }));

  let sentinel!: HTMLDivElement;
  let feedEl!: HTMLDivElement;

  onMount(() => {
    void reload();

    const scrollObs = new IntersectionObserver(
      (ents) => { if (ents.some((x) => x.isIntersecting)) void revealNextDay(); },
      { rootMargin: "300px" },
    );
    scrollObs.observe(sentinel);

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
        setMarkdownBody(md);

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
      setMarkdownBody(md);
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
    const md = editorMode() === "rich"
      ? (editor?.storage.markdown as { getMarkdown(): string } | undefined)?.getMarkdown() ?? markdownBody()
      : markdownBody();
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
      {/* ---- journal header + "new entry" button ---- */}
      <div class="journal-header">
        <button class="primary journal-new-btn" onClick={openOverlay}>
          + new entry
        </button>
      </div>

      {/* ---- day-by-day feed ---- */}
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
                  <article class="entry entry-fade">
                    <header>
                      <span class="actor-chip">{e.author}</span>
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

      {/* ---- floating action button ---- */}
      <button class="journal-fab" onClick={openOverlay} title="New entry" aria-label="New entry">
        ✦
      </button>

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
                <button title="checkbox task" onClick={toolCheckbox}>☑           </button>
                <button title="blockquote"    onClick={toolQuote}>  ❝            </button>
                <button title="link"          onClick={toolLink}>   🔗           </button>
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
                          <span class="ac-kind-glyph">{KIND_GLYPH[item.kind] ?? "·"}</span>
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
                          <span class="ac-kind-glyph">{KIND_GLYPH[item.kind] ?? "·"}</span>
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
