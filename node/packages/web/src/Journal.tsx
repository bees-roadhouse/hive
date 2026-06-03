// Journal.tsx — calmer read-first experience with focused writing overlay.
// Load model: show only today's entries on open; each scroll-down reveals the
// previous calendar day, one at a time. Composer lives in a centered overlay.
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
import { api, getActor } from "./api.ts";
import { liveRev } from "./live.ts";
import { ANCHOR_GLYPH, relTime } from "./lib.tsx";
import { Icon } from "./icons.tsx";
import { JournalBody, Markdown } from "./markdown.tsx";
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

// Auto-grow a textarea to its content height (capped by CSS max-height).
function autoGrow(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${el.scrollHeight}px`;
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

export const Journal: Component = () => {
  // ---- composer state (lives in overlay) ----
  const [body, setBody] = createSignal("");
  const [preview, setPreview] = createSignal(false);
  const [pending, setPending] = createSignal<Pending[]>([]);
  const [sel, setSel] = createSignal<{ start: number; end: number }>({ start: 0, end: 0 });
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
  //
  // `buffer` accumulates all entries fetched from the API (newest first).
  // `visibleDays` is how many calendar-day groups are currently shown in the feed.
  // The sentinel fires `revealNextDay()` which either bumps visibleDays (if the
  // buffer already has a next day ready) or fetches more pages until it does.
  const [buffer, setBuffer] = createSignal<JournalEntryView[]>([]);
  const [fetching, setFetching] = createSignal(false);
  const [fetchDone, setFetchDone] = createSignal(false);
  const [visibleDays, setVisibleDays] = createSignal(1);

  // All calendar-day groups in the buffer.
  const allDayGroups = createMemo(() => groupByDay(buffer()));

  // The slice actually rendered: only visibleDays worth.
  const days = createMemo(() => allDayGroups().slice(0, visibleDays()));

  // True when there are buffered day groups not yet shown.
  const hasBufferedDays = createMemo(() => allDayGroups().length > visibleDays());

  // True when there is absolutely nothing more to show or fetch.
  const allDone = createMemo(() => fetchDone() && !hasBufferedDays());

  // Fetch pages from the API until we have at least one new calendar-day group
  // beyond what's currently in the buffer, or the API is exhausted.
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

  // Called by the sentinel: reveal the next older day (fetching if needed).
  const revealNextDay = async () => {
    if (hasBufferedDays()) {
      setVisibleDays((n) => n + 1);
      return;
    }
    const got = await fetchUntilNewDay();
    if (got) setVisibleDays((n) => n + 1);
  };

  // Full reload: clears buffer and resets to showing 1 day (today).
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

  // Reload on SSE bump (deferred — onMount handles the initial load).
  createEffect(on(liveRev, () => { void reload(); }, { defer: true }));

  let sentinel!: HTMLDivElement;
  let feedEl!: HTMLDivElement;

  onMount(() => {
    void reload();

    // Sentinel: entering view → reveal next day.
    const scrollObs = new IntersectionObserver(
      (ents) => {
        if (ents.some((x) => x.isIntersecting)) void revealNextDay();
      },
      { rootMargin: "300px" },
    );
    scrollObs.observe(sentinel);

    // Entry fade-in (respects prefers-reduced-motion).
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

  // ---- overlay open/close ----
  const openOverlay = () => {
    setOverlayOpen(true);
    // Focus the textarea after the overlay DOM mounts.
    queueMicrotask(() => { if (ta) { ta.focus(); autoGrow(ta); } });
  };

  const closeOverlay = () => {
    setOverlayOpen(false);
    setPreview(false);
    dismissAc();
  };

  // ---- textarea ref (mounted only while overlay is open) ----
  let ta!: HTMLTextAreaElement;
  const trackSel = () => {
    if (!ta) return;
    setSel({ start: ta.selectionStart, end: ta.selectionEnd });
  };
  const selectedText = () => body().slice(sel().start, sel().end).trim();

  // Auto-grow whenever body changes (only meaningful when overlay is open).
  createEffect(on(body, () => { if (ta && overlayOpen()) autoGrow(ta); }));

  // ---- markdown toolbar ----
  const surround = (pre: string, post = pre) => {
    if (!ta) return;
    const s = ta.selectionStart;
    const e = ta.selectionEnd;
    const v = body();
    const chosen = v.slice(s, e) || "text";
    setBody(v.slice(0, s) + pre + chosen + post + v.slice(e));
    queueMicrotask(() => {
      ta.focus();
      ta.selectionStart = s + pre.length;
      ta.selectionEnd = s + pre.length + chosen.length;
      trackSel();
    });
  };
  const prefixLine = (prefix: string) => {
    if (!ta) return;
    const s = ta.selectionStart;
    const v = body();
    const lineStart = v.lastIndexOf("\n", s - 1) + 1;
    setBody(v.slice(0, lineStart) + prefix + v.slice(lineStart));
    queueMicrotask(() => {
      ta.focus();
      ta.selectionStart = ta.selectionEnd = s + prefix.length;
      trackSel();
    });
  };

  const mark = (kind: AnchorKind) => {
    const text = body().slice(sel().start, sel().end).trim();
    if (!text) return;
    setPending([
      ...pending(),
      { text, kind, title: text.split(/[.\n]/)[0].slice(0, 80), priority: "normal" },
    ]);
  };
  const removePending = (i: number) => setPending(pending().filter((_, j) => j !== i));

  // ---- autocomplete ----
  const findTriggerContext = (
    text: string,
    caret: number,
  ): { trigger: string; query: string; triggerPos: number } | null => {
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
  };

  const dismissAc = () => {
    setAcItems([]);
    setAcTrigger(null);
    setAcQuery("");
    setAcActive(0);
    clearTimeout(acTimer);
  };

  const onBodyInput = (e: InputEvent & { currentTarget: HTMLTextAreaElement }) => {
    const val = e.currentTarget.value;
    setBody(val);
    trackSel();
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
      } catch {
        setAcItems([]);
      }
    }, 120);
  };

  const selectAcItem = (item: AutocompleteItem) => {
    if (!ta) return;
    const val = body();
    const caret = ta.selectionStart;
    const ctx = findTriggerContext(val, caret);
    if (!ctx) { dismissAc(); return; }
    const token = `[${item.kind}: ${item.label}]`;
    const before = val.slice(0, ctx.triggerPos);
    const after = val.slice(caret);
    setBody(`${before}${token}${after}`);
    dismissAc();
    queueMicrotask(() => {
      ta.focus();
      const pos = before.length + token.length;
      ta.selectionStart = ta.selectionEnd = pos;
      trackSel();
    });
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (acItems().length) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setAcActive((i) => Math.min(i + 1, acItems().length - 1));
        return;
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setAcActive((i) => Math.max(i - 1, 0));
        return;
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const item = acItems()[acActive()];
        if (item) selectAcItem(item);
        return;
      } else if (e.key === "Escape") {
        dismissAc();
        return;
      }
    }
    // Esc with no AC open → close overlay.
    if (e.key === "Escape") {
      e.stopPropagation();
      closeOverlay();
    }
  };

  // ---- submit ----
  const post = async () => {
    if (!body().trim()) return;
    let cursor = 0;
    const anchors: NewAnchor[] = [];
    for (const p of pending()) {
      const start = body().indexOf(p.text, cursor);
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
    await api.append({ author: getActor(), body: body(), anchors });
    setBody("");
    setPending([]);
    dismissAc();
    setOverlayOpen(false);
    setPreview(false);
    await reload();
  };

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

        {/* Sentinel: IntersectionObserver fires revealNextDay() */}
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

      {/* ---- floating action button (bottom-right) ---- */}
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
              <div class="seg">
                <button classList={{ active: !preview() }} onClick={() => setPreview(false)}>
                  write
                </button>
                <button classList={{ active: preview() }} onClick={() => setPreview(true)}>
                  preview
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

            {/* editor */}
            <Show
              when={!preview()}
              fallback={
                <div class="composer-preview overlay-preview">
                  <Markdown src={body().trim() || "*nothing to preview yet*"} />
                </div>
              }
            >
              <div class="md-toolbar">
                <button title="bold" onClick={() => surround("**")}><b>B</b></button>
                <button title="italic" onClick={() => surround("_")}><i>I</i></button>
                <button title="inline code" onClick={() => surround("`")}>{"</>"}</button>
                <button title="heading" onClick={() => prefixLine("## ")}>H</button>
                <button title="bullet" onClick={() => prefixLine("- ")}>•</button>
                <button title="checkbox task" onClick={() => prefixLine("- [ ] ")}>☑</button>
                <button title="quote" onClick={() => prefixLine("> ")}>❝</button>
                <button title="link" onClick={() => surround("[", "](https://)")}>🔗</button>
              </div>
              {/* composer-editor-wrap keeps the ac-dropdown positioned relative to it */}
              <div class="composer-editor-wrap">
                <textarea
                  ref={ta}
                  class="composer-ta overlay-ta"
                  placeholder="e.g. Synced with @pia — shipping the **Solid UI** this week. Staying on `SQLite`."
                  value={body()}
                  onInput={onBodyInput}
                  onSelect={trackSel}
                  onMouseUp={trackSel}
                  onKeyUp={trackSel}
                  onKeyDown={onKeyDown}
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
              <button class="primary" disabled={!body().trim()} onClick={post}>
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
