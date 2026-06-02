import { createResource, createSignal, For, Show, type Component } from "solid-js";
import type { AnchorKind, NewAnchor, Priority, ResolvedAnchor } from "@hive/shared";
import { PRIORITIES } from "@hive/shared";
import { api, getActor } from "./api.ts";
import { ANCHOR_GLYPH, Prose, relTime } from "./lib.tsx";
import { EntityCard } from "./Boards.tsx";

interface Pending {
  text: string;
  kind: AnchorKind;
  title: string;
  priority: Priority;
}

export const Journal: Component = () => {
  const [entries, { refetch }] = createResource(() => api.journal());
  const [body, setBody] = createSignal("");
  const [pending, setPending] = createSignal<Pending[]>([]);
  const [sel, setSel] = createSignal<{ start: number; end: number }>({ start: 0, end: 0 });
  const [open, setOpen] = createSignal<ResolvedAnchor | null>(null);

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
    refetch();
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
        <For each={entries()}>
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
