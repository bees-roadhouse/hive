import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTORS } from "@hive/shared";
import { api, getActor } from "./api.ts";
import { liveRev } from "./live.ts";
import { Mentions, relTime } from "./lib.tsx";
import { EmptyState } from "./primitives.tsx";

const REASON_GLYPH: Record<string, string> = {
  mention: "@",
  assignment: "◻",
  decision: "◆",
  event: "◷",
};

/** Per-actor inbox. Humans and AIs each get one; switch the recipient to peek. */
export const Inbox: Component = () => {
  const [who, setWho] = createSignal(getActor());
  const [unreadOnly, setUnreadOnly] = createSignal(true);
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
          <For each={ACTORS}>
            {(a) => (
              <button classList={{ active: who() === a.name }} onClick={() => setWho(a.name)}>
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
        fallback={
          <EmptyState
            icon="inbox"
            title={unreadOnly() ? "Nothing unread." : "Nothing here yet."}
            hint={`Mentions, assignments, and updates for ${who()} land here.`}
          />
        }
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
