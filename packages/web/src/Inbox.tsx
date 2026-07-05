import { createMemo, createResource, createSignal, For, Show, type Component } from "solid-js";
import { api, getActor, getCurrentUser } from "./api.ts";
import { liveRev } from "./live.ts";
import { Mentions, relTime } from "./lib.tsx";

const REASON_GLYPH: Record<string, string> = {
  mention: "@",
  assignment: "◻",
  decision: "◆",
  event: "◷",
};

/** Per-actor inbox. The tabs mirror the server's viewer gate: yourself and
 *  the AIs you own; admins can open anyone's. */
export const Inbox: Component = () => {
  const [who, setWho] = createSignal(getActor());
  const [unreadOnly, setUnreadOnly] = createSignal(true);
  const [people] = createResource(api.people);

  const tabs = createMemo(() => {
    const me = getActor();
    const all = people() ?? [];
    const visible =
      getCurrentUser()?.role === "admin"
        ? all
        : all.filter((p) => p.slug === me || (p.kind === "ai" && p.owner === me));
    // Self first, then humans, then AIs — stable regardless of API order.
    const rank = (p: { slug: string; kind: string }) =>
      p.slug === me ? 0 : p.kind === "human" ? 1 : 2;
    const sorted = visible
      .map((p) => ({ slug: p.slug, name: p.name, kind: p.kind }))
      .sort((a, b) => rank(a) - rank(b) || a.slug.localeCompare(b.slug));
    // Until people load (or if self has no person row yet), keep a self tab.
    return sorted.length ? sorted : [{ slug: me, name: me, kind: "human" }];
  });

  const [items, { refetch }] = createResource(
    () => ({ who: who(), unread: unreadOnly(), _r: liveRev() }),
    (k) => api.inbox(k.who, k.unread),
  );

  const read = async (id: string) => {
    await api.markRead(id);
    refetch();
  };
  const readAll = async () => {
    await api.markAllRead(who());
    refetch();
  };

  return (
    <section class="inbox">
      <div class="inbox-bar">
        <div class="who-tabs">
          <For each={tabs()}>
            {(a) => (
              <button classList={{ active: who() === a.slug }} onClick={() => setWho(a.slug)}>
                {a.name}
                <span class="kind-dot" classList={{ ai: a.kind === "ai" }} />
              </button>
            )}
          </For>
        </div>
        <label class="dim">
          <input
            type="checkbox"
            checked={unreadOnly()}
            onChange={(e) => setUnreadOnly(e.currentTarget.checked)}
          />
          unread only
        </label>
        <button onClick={readAll}>mark all read</button>
      </div>

      <Show
        when={items()?.length}
        fallback={<p class="dim pad">📭 nothing {unreadOnly() ? "unread" : "here"} for {who()}.</p>}
      >
        <For each={items()}>
          {(it) => (
            <div class="inbox-item" classList={{ unread: !it.read_at }}>
              <span class={`reason reason-${it.reason}`}>{REASON_GLYPH[it.reason] ?? "•"}</span>
              <div class="inbox-body">
                <div class="inbox-meta">
                  <span class="actor-chip sm">{it.from}</span>
                  <span class="dim">
                    {it.reason} · {it.ref_kind}
                  </span>
                  <time class="dim">{relTime(it.created_at)}</time>
                </div>
                <div class="inbox-snip">
                  <Mentions text={it.snippet} />
                </div>
              </div>
              <Show when={!it.read_at}>
                <button class="x" title="mark read" onClick={() => read(it.id)}>
                  ✓
                </button>
              </Show>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};
