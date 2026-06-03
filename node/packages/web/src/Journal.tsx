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

// Kind glyph for the autocomplete dropdown.
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

export const Journal: Component = () => {
  const [body, setBody] = createSignal("");
  const [preview, setPreview] = createSignal(false);
  const [pending, setPending] = createSignal<Pending[]>([]);
  const [sel, setSel] = createSignal<{ start: number; end: number }>({ start: 0, end: 0 });
  const [open, setOpen] = createSignal<ResolvedAnchor | null>(null);

  // Autocomplete state.
  const [acItems, setAcItems] = createSignal<AutocompleteItem[]>([]);
  const [acTrigger, setAcTrigger] = createSignal<string | null>(null);
  const [acQuery, setAcQuery] = createSignal("");
  const [acActive, setAcActive] = createSignal(0);
  let acTimer: ReturnType<typeof setTimeout> | undefined;

  // Infinite scroll feed state.
  const [entries, setEntries] = createSignal<JournalEntryView[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [done, setDone] = createSignal(false);

  const loadMore = async () => {
    if (loading() || done()) return;
    setLoading(true);
    try {
      const batch = await api.journal(PAGE, entries().length);
      setEntries([...entries(), ...batch]);
      if (batch.length < PAGE) setDone(true);
    } finally {
      setLoading(false);
    }
  };
  const reload = async () => {
    setDone(false);
    setEntries([]);
    await loadMore();
  };

  const days = createMemo(() => {
    const out: { day: string; label: string; items: JournalEntryView[] }[] = [];
    const idx = new Map<string, number>();
    for (const e of entries()) {
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
  });

  let sentinel!: HTMLDivElement;
  let feedEl!: HTMLDivElement;

  onMount(() => {
    void loadMore();

    // Infinite scroll sentinel.
    const scrollObs = new IntersectionObserver(
      (ents) => {
        if (ents.some((x) => x.isIntersecting)) void loadMore();
      },
      { rootMargin: "300px" },
    );
    scrollObs.observe(sentinel);

    // Fade-in for entry cards as they enter the viewport.
    // Respect prefers-reduced-motion — if set, skip the animation entirely.
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
      // Observe existing entries and any that arrive later.
      const observeEntries = () => {
        if (!fadeObs) return;
        feedEl?.querySelectorAll(".entry:not(.entry-visible)").forEach((el) => {
          fadeObs!.observe(el);
        });
      };
      observeEntries();
      // Re-observe after each feed update.
      const mo = new MutationObserver(observeEntries);
      mo.observe(feedEl, { childList: true, subtree: true });
      onCleanup(() => mo.disconnect());
    }

    onCleanup(() => {
      scrollObs.disconnect();
      fadeObs?.disconnect();
    });
  });

  let ta!: HTMLTextAreaElement;
  const trackSel = () => setSel({ start: ta.selectionStart, end: ta.selectionEnd });
  const selectedText = () => body().slice(sel().start, sel().end).trim();

  // Auto-grow the textarea whenever body changes.
  createEffect(
    on(body, () => {
      if (ta) autoGrow(ta);
    }),
  );

  // Markdown toolbar helpers.
  const surround = (pre: string, post = pre) => {
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

  // Autocomplete: scan backward from caret for a trigger char + query text.
  const findTriggerContext = (
    text: string,
    caret: number,
  ): { trigger: string; query: string; triggerPos: number } | null => {
    // Scan back up to 64 chars (long enough for any realistic name).
    const limit = Math.max(0, caret - 64);
    for (let i = caret - 1; i >= limit; i--) {
      const ch = text[i];
      if (ch in TRIGGERS) {
        // Make sure the char just before the trigger isn't a word char (no mid-word trigger).
        const before = i === 0 ? "" : text[i - 1];
        if (before && /\w/.test(before)) continue;
        return { trigger: ch, query: text.slice(i + 1, caret), triggerPos: i };
      }
      // Stop if we hit whitespace (no trigger on a multi-word gap without a trigger).
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
    const val = body();
    const caret = ta.selectionStart;
    const ctx = findTriggerContext(val, caret);
    if (!ctx) { dismissAc(); return; }
    // Build the bracket token to insert.
    const token = `[${item.kind}: ${item.label}]`;
    const before = val.slice(0, ctx.triggerPos);
    const after = val.slice(caret);
    const next = `${before}${token}${after}`;
    setBody(next);
    dismissAc();
    // Place caret right after the inserted token.
    queueMicrotask(() => {
      ta.focus();
      const pos = before.length + token.length;
      ta.selectionStart = ta.selectionEnd = pos;
      trackSel();
    });
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (!acItems().length) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setAcActive((i) => Math.min(i + 1, acItems().length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setAcActive((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      const item = acItems()[acActive()];
      if (item) selectAcItem(item);
    } else if (e.key === "Escape") {
      dismissAc();
    }
  };

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
    await reload();
  };

  return (
    <section class="journal">
      <div class="composer">
        <div class="composer-top">
          <div class="composer-hint">
            Write in <strong>markdown</strong>. Use{" "}
            <code class="trigger-hint">@name</code> for people,{" "}
            <code class="trigger-hint">#topic</code>,{" "}
            <code class="trigger-hint">+project</code>,{" "}
            <code class="trigger-hint">!task</code>, or{" "}
            <code class="trigger-hint">[kind: …]</code> for any entity.
            Select text to tag it as a task, decision, or event.
          </div>
          <div class="seg">
            <button classList={{ active: !preview() }} onClick={() => setPreview(false)}>
              write
            </button>
            <button classList={{ active: preview() }} onClick={() => setPreview(true)}>
              preview
            </button>
          </div>
        </div>
        <Show
          when={!preview()}
          fallback={
            <div class="composer-preview">
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
          <div class="composer-editor-wrap">
            <textarea
              ref={ta}
              class="composer-ta"
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
          <Show when={loading()}>
            <span class="dim sm">gathering…</span>
          </Show>
          <Show when={!loading() && !done() && entries().length > 0}>
            <button class="ghost" onClick={() => void loadMore()}>
              load earlier
            </button>
          </Show>
          <Show when={done() && entries().length > 0}>
            <span class="dim sm">— the first cell —</span>
          </Show>
          <Show when={done() && entries().length === 0}>
            <span class="dim sm">no entries yet. write the first one above.</span>
          </Show>
        </div>
      </div>

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
