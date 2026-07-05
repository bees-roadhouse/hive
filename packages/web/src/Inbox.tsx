import { createEffect, createMemo, createResource, createSignal, For, Show, type Component } from "solid-js";
import type { Person } from "@hive/shared";
import { api, getActor, getCurrentUser } from "./api.ts";
import { liveRev } from "./live.ts";
import { Mentions, relTime } from "./lib.tsx";
import { EmptyState } from "./primitives.tsx";

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
  // Live like the other people views so mid-session grants/revocations move
  // the tabs. A failed fetch keeps the previous list (resolving with a
  // fallback would overwrite Solid's stale-value retention and collapse the
  // tabs mid-session); before any list has loaded it degrades to the self tab.
  const [people] = createResource<Person[], { _r: number }>(
    () => ({ _r: liveRev() }),
    (_k, info) => api.people().catch(() => info.value ?? []),
  );

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
    const sorted = [...visible].sort((a, b) => rank(a) - rank(b) || a.slug.localeCompare(b.slug));
    // Self always keeps a tab — while people load, and when its person row
    // is missing (a self-rename re-slugs people but not users.actor).
    return sorted.some((p) => p.slug === me)
      ? sorted
      : [{ slug: me, name: me, kind: "human" }, ...sorted];
  });

  // A revoked/vanished tab falls back to your own inbox instead of pinning
  // a recipient the server now refuses.
  createEffect(() => {
    if (!tabs().some((t) => t.slug === who())) setWho(getActor());
  });

  const [items, { refetch }] = createResource(
    () => ({ who: who(), unread: unreadOnly(), _r: liveRev() }),
    // A 403 (tab revoked between refetches) reads as an empty inbox; real
    // outages still surface through the app-level boundary.
    (k) =>
      api.inbox(k.who, k.unread).catch((e) => {
        if (String(e?.message ?? e).startsWith("403")) return [];
        throw e;
      }),
  );

  const read = async (id: string) => {
    try {
      await api.markRead(id);
    } catch (e) {
      console.error("mark read failed", e);
    } finally {
      refetch();
    }
  };
  const readAll = async () => {
    try {
      await api.markAllRead(who());
    } catch (e) {
      console.error("mark all read failed", e);
    } finally {
      refetch();
    }
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
