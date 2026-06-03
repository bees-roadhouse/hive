import { createMemo, createSignal, For, onCleanup, onMount, Show, type Component } from "solid-js";
import type { AnchorKind, JournalEntryView, NewAnchor, Priority, ResolvedAnchor } from "@hive/shared";
import { PRIORITIES } from "@hive/shared";
import { api, getActor } from "./api.ts";
import { ANCHOR_GLYPH, Prose, relTime } from "./lib.tsx";
import { Icon } from "./icons.tsx";
import { EntityCard } from "./Boards.tsx";

interface Pending {
  text: string;
  kind: AnchorKind;
  title: string;
  priority: Priority;
}

const PAGE = 20;

/** "Today" / "Yesterday" / "Tuesday, June 2, 2026" for a day header. */
function dayLabel(iso: string): string {
  const d = new Date(iso);
  const key = (x: Date) => `${x.getFullYear()}-${x.getMonth()}-${x.getDate()}`;
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (key(d) === key(today)) return "Today";
  if (key(d) === key(yesterday)) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric", year: "numeric" });
}

export const Journal: Component = () => {
  const [body, setBody] = createSignal("");
  const [pending, setPending] = createSignal<Pending[]>([]);
  const [sel, setSel] = createSignal<{ start: number; end: number }>({ start: 0, end: 0 });
  const [open, setOpen] = createSignal<ResolvedAnchor | null>(null);

  // The feed is an infinite scroll: entries accumulate page by page, oldest
  // loaded on demand as a sentinel at the bottom scrolls into view, then group
  // into one "page" (comb cell) per calendar day.
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
  onMount(() => {
    void loadMore();
    const obs = new IntersectionObserver(
      (ents) => {
        if (ents.some((x) => x.isIntersecting)) void loadMore();
      },
      { rootMargin: "300px" },
    );
    obs.observe(sentinel);
    onCleanup(() => obs.disconnect());
  });

  let ta!: HTMLTextAreaElement;
  const trackSel = () => setSel({ start: ta.selectionStart, end: ta.selectionEnd });
  const selectedText = () => body().slice(sel().start, sel().end).trim();

  const mark = (kind: AnchorKind) => {
    const text = body().slice(sel().start, sel().end).trim();
    if (!text) return;
    setPending([
      ...pending(),
      { text, kind, title: text.split(/[.\n]/)[0].slice(0, 80), priority: "normal" },
    ]);
  };
  const removePending = (i: number) => setPending(pending().filter((_, j) => j !== i));

  const post = async () => {
    if (!body().trim()) return;
    // Recompute offsets from text at submit time so edits never desync anchors.
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
    await reload();
  };

  return (
    <section class="journal">
      <div class="composer">
        <div class="composer-hint">
          Write what happened. Select any phrase, then tag it — it becomes a task, decision, or
          event anchored to that text. <span class="mention">@mention</span> to notify someone.
        </div>
        <textarea
          ref={ta}
          rows={4}
          placeholder="e.g. Synced with @pia — we'll ship the Solid UI this week. Decided to stay on SQLite."
          value={body()}
          onInput={(e) => setBody(e.currentTarget.value)}
          onSelect={trackSel}
          onMouseUp={trackSel}
          onKeyUp={trackSel}
        />
        <div class="composer-bar">
          <div class="mark-group">
            <span class="dim">
              {selectedText() ? `tag “${selectedText().slice(0, 28)}…” as` : "select text to tag"}
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
                        {(pr) => <option value={pr} selected={pr === "normal"}>{pr}</option>}
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

      <div class="feed">
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
                  <article class="entry">
                    <header>
                      <span class="actor-chip">{e.author}</span>
                      <time>{relTime(e.created_at)}</time>
                      <Show when={e.anchors.length}>
                        <span class="dim">
                          · {e.anchors.length} anchor{e.anchors.length > 1 ? "s" : ""}
                        </span>
                      </Show>
                    </header>
                    <Prose body={e.body} anchors={e.anchors} onAnchor={setOpen} />
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
              <blockquote>“{a().text}”</blockquote>
              <EntityCard kind={a().kind} entity={a().entity} />
            </div>
          </div>
        )}
      </Show>
    </section>
  );
};
