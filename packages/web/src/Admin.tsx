import { createMemo, createResource, createSignal, For, Show, type Component } from "solid-js";
import type { ActorDeleteResult, ActorMergeResult, Person } from "@hive/shared";
import { api } from "./api.ts";
import { relTime } from "./lib.tsx";
import { liveRev } from "./live.ts";

// ---- actor delete / merge confirm panel ----
// Both ops are destructive, so the flow is preview → review counts → confirm.
// The preview hits the same store path under ?dryRun=1, so the numbers shown
// match the real run exactly.

/** Non-zero per-table counts from a delete/merge result, for the confirm summary. */
const nonZeroCounts = (r: Record<string, unknown>): [string, number][] =>
  Object.entries(r)
    .filter(([k, v]) => typeof v === "number" && v > 0 && k !== "dryRun")
    .map(([k, v]) => [k, v as number]);

const ActorOps: Component<{ person: Person; people: Person[]; onDone: () => void }> = (props) => {
  const [mode, setMode] = createSignal<"delete" | "merge" | null>(null);
  const [target, setTarget] = createSignal(""); // merge-into slug
  const [preview, setPreview] = createSignal<ActorDeleteResult | ActorMergeResult | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);

  const others = createMemo(() => props.people.filter((p) => p.slug !== props.person.slug));

  const reset = () => {
    setMode(null);
    setTarget("");
    setPreview(null);
    setErr(null);
  };

  const runPreview = async () => {
    setErr(null);
    setBusy(true);
    try {
      const r =
        mode() === "delete"
          ? await api.previewDeleteActor(props.person.slug)
          : await api.previewMergeActor(props.person.slug, target());
      setPreview(r);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const confirm = async () => {
    setErr(null);
    setBusy(true);
    try {
      if (mode() === "delete") await api.deleteActor(props.person.slug);
      else await api.mergeActor(props.person.slug, target());
      reset();
      props.onDone();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <span class="actor-ops">
      <Show
        when={mode()}
        fallback={
          <>
            <button class="ghost" onClick={() => setMode("merge")}>merge…</button>
            <button class="ghost danger" onClick={() => setMode("delete")}>delete…</button>
          </>
        }
      >
        <div class="actor-ops-panel">
          <Show when={mode() === "merge"}>
            <label class="dim sm">
              into&nbsp;
              <select value={target()} onChange={(e) => { setTarget(e.currentTarget.value); setPreview(null); }}>
                <option value="">pick target…</option>
                <For each={others()}>{(o) => <option value={o.slug}>{o.name} ({o.slug})</option>}</For>
              </select>
            </label>
          </Show>

          <Show when={!preview()}>
            <button
              class="primary"
              disabled={busy() || (mode() === "merge" && !target())}
              onClick={runPreview}
            >
              {busy() ? "…" : "preview"}
            </button>
          </Show>

          <Show when={preview()}>
            {(p) => (
              <div class="actor-ops-preview">
                <p class="sm">
                  <strong class="danger">
                    {mode() === "delete"
                      ? `Delete ${props.person.name} and cascade:`
                      : `Fold ${props.person.name} → ${target()}:`}
                  </strong>
                </p>
                <div class="actor-ops-counts">
                  <For each={nonZeroCounts(p() as unknown as Record<string, unknown>)} fallback={<span class="dim sm">nothing to change.</span>}>
                    {([k, n]) => (
                      <span class="kv sm">
                        <code>{k}</code>
                        <span>{n}</span>
                      </span>
                    )}
                  </For>
                </div>
                <button class="primary danger" disabled={busy()} onClick={confirm}>
                  {busy() ? "…" : mode() === "delete" ? "confirm delete" : "confirm merge"}
                </button>
              </div>
            )}
          </Show>

          <button class="ghost" onClick={reset} disabled={busy()}>cancel</button>
          <Show when={err()}>{(e) => <span class="danger sm">{e()}</span>}</Show>
        </div>
      </Show>
    </span>
  );
};

// ---- Writers management ----

const WritersSection: Component = () => {
  const [people, { refetch }] = createResource(() => ({ _r: liveRev() }), () => api.people());
  const humans = () => (people() ?? []).filter((p) => p.kind === "human");
  const setOwner = async (p: Person, owner: string) => {
    await api.patchPerson(p.slug, { owner: owner || null });
    refetch();
  };

  // Add-writer form state
  const [newName, setNewName] = createSignal("");
  const [newKind, setNewKind] = createSignal<"human" | "ai">("human");
  const [adding, setAdding] = createSignal(false);

  // Inline-edit state: tracks which person is being edited and their draft values
  const [editId, setEditId] = createSignal<string | null>(null);
  const [editName, setEditName] = createSignal("");
  const [editKind, setEditKind] = createSignal<"human" | "ai">("human");

  const startEdit = (p: Person) => {
    setEditId(p.slug);
    setEditName(p.name);
    setEditKind(p.kind);
  };

  const cancelEdit = () => setEditId(null);

  const saveEdit = async () => {
    const id = editId();
    if (!id) return;
    await api.patchPerson(id, { name: editName().trim() || undefined, kind: editKind() });
    setEditId(null);
    refetch();
  };

  const addWriter = async () => {
    const name = newName().trim();
    if (!name) return;
    setAdding(true);
    try {
      await api.addPerson({ name, kind: newKind() });
      setNewName("");
      setNewKind("human");
      refetch();
    } finally {
      setAdding(false);
    }
  };

  return (
    <>
      <h3 class="sec">Writers</h3>
      <p class="dim sm pad">Actors who can write journal entries and receive inbox items. Seeded from ACTORS; add more here.</p>

      {/* Add-writer form */}
      <div class="source-form">
        <input
          class="grow"
          placeholder="Name…"
          value={newName()}
          onInput={(e) => setNewName(e.currentTarget.value)}
          onKeyDown={(e) => e.key === "Enter" && addWriter()}
        />
        <select value={newKind()} onChange={(e) => setNewKind(e.currentTarget.value as "human" | "ai")}>
          <option value="human">human</option>
          <option value="ai">ai</option>
        </select>
        <button class="primary" onClick={addWriter} disabled={adding() || !newName().trim()}>
          + add
        </button>
      </div>

      {/* Writers list */}
      <Show when={people()} fallback={<p class="dim sm">loading…</p>}>
        <For each={people()} fallback={<p class="dim sm">no writers yet.</p>}>
          {(p) => (
            <div class="source-row">
              <Show
                when={editId() === p.slug}
                fallback={
                  <>
                    <span class="source-main">
                      <span class="source-name">
                        <strong>{p.name}</strong>
                        <span class={`badge kind-badge-${p.kind}`}>{p.kind}</span>
                      </span>
                      <span class="dim sm">{p.slug}</span>
                    </span>
                    <Show when={p.kind === "ai"}>
                      <label class="writer-owner-label dim sm">
                        owner&nbsp;
                        <select
                          class="writer-owner-select"
                          value={p.owner ?? ""}
                          onChange={(e) => setOwner(p, e.currentTarget.value)}
                        >
                          <option value="">none</option>
                          <For each={humans()}>{(h) => <option value={h.slug}>{h.name}</option>}</For>
                        </select>
                      </label>
                    </Show>
                    <button class="ghost" onClick={() => startEdit(p)}>edit</button>
                    <ActorOps person={p} people={people() ?? []} onDone={refetch} />
                  </>
                }
              >
                {/* Inline edit row */}
                <input
                  value={editName()}
                  onInput={(e) => setEditName(e.currentTarget.value)}
                  onKeyDown={(e) => { if (e.key === "Enter") saveEdit(); if (e.key === "Escape") cancelEdit(); }}
                  style={{ flex: "1" }}
                />
                <select value={editKind()} onChange={(e) => setEditKind(e.currentTarget.value as "human" | "ai")}>
                  <option value="human">human</option>
                  <option value="ai">ai</option>
                </select>
                <button class="primary" onClick={saveEdit}>save</button>
                <button class="ghost" onClick={cancelEdit}>cancel</button>
              </Show>
            </div>
          )}
        </For>
      </Show>
    </>
  );
};

// ---- Admin shell ----

/** Operational view: worker heartbeat + last cycle, embedding coverage,
 * outbound job queue, and writer management. */
export const Admin: Component = () => {
  const [worker, { refetch: rw }] = createResource(() => ({ _r: liveRev() }), () => api.worker());
  const [emb, { refetch: re }] = createResource(() => ({ _r: liveRev() }), () => api.embeddings());
  const [outbox, { refetch: ro }] = createResource(() => ({ _r: liveRev() }), () => api.outbox());
  const refresh = () => {
    rw();
    re();
    ro();
  };

  const coverage = (e: { embeddable: number; pending: number }) =>
    e.embeddable ? Math.round(((e.embeddable - e.pending) / e.embeddable) * 100) : 100;

  return (
    <section class="admin">
      <WritersSection />

      <div class="admin-head">
        <h3 class="sec">Worker</h3>
        <button class="ghost" onClick={refresh}>↻ refresh</button>
      </div>

      <Show when={worker()} fallback={<p class="dim sm">loading…</p>}>
        {(s) => (
          <div class="worker-status">
            <div class="ws-dot" classList={{ live: !!s().heartbeat }} />
            <div>
              <strong>worker</strong>{" "}
              <span class="dim">
                {s().heartbeat ? `heartbeat ${relTime(s().heartbeat!)}` : "no heartbeat yet — start @hive/worker"}
              </span>
              <Show when={s().last_run}>
                {(r) => (
                  <div class="dim sm">
                    last run {relTime(r().at)} · polled {r().polled} · ingested {r().ingested} · outbox {r().outbox} ·
                    embedded {r().embedded} · {r().maintenance.join(", ")}
                  </div>
                )}
              </Show>
            </div>
            <div class="ws-stats">
              <span class="badge">{s().sources.enabled}/{s().sources.total} sources</span>
              <span class="badge">
                outbox {s().outbox.pending}p/{s().outbox.failed}f/{s().outbox.done}d
              </span>
            </div>
          </div>
        )}
      </Show>

      <h3 class="sec">Embeddings</h3>
      <Show when={emb()} fallback={<p class="dim sm">loading…</p>}>
        {(e) => (
          <div class="emb">
            <div class="emb-top">
              <span class="badge">{e().total} vectors</span>
              <span class="badge">{e().model}</span>
              <span class="badge" classList={{ warn: e().pending > 0 }}>
                {e().pending} pending / {e().embeddable} items
              </span>
              <span class="dim sm">{coverage(e())}% covered</span>
            </div>
            <div class="bar" title={`${e().embeddable - e().pending} of ${e().embeddable} embedded`}>
              <div class="bar-fill" style={{ width: `${coverage(e())}%` }} />
            </div>
            <div class="emb-grid">
              <div>
                <div class="dim sm">by kind</div>
                <For each={e().byKind} fallback={<div class="dim sm">none</div>}>
                  {(k) => (
                    <div class="kv">
                      <code>{k.kind}</code>
                      <span>{k.count}</span>
                    </div>
                  )}
                </For>
              </div>
              <div>
                <div class="dim sm">by model</div>
                <For each={e().byModel} fallback={<div class="dim sm">none</div>}>
                  {(m) => (
                    <div class="kv">
                      <code>
                        {m.model} · {m.dim}d
                      </code>
                      <span>{m.count}</span>
                    </div>
                  )}
                </For>
              </div>
            </div>
            <Show when={e().pending > 0}>
              <p class="dim sm">
                {e().pending} item(s) await (re)embedding on the next worker cycle (<code>pnpm worker:once</code>).
              </p>
            </Show>
          </div>
        )}
      </Show>

      <h3 class="sec">Jobs · outbound queue</h3>
      <Show when={outbox()?.length} fallback={<p class="dim sm">no jobs.</p>}>
        <For each={outbox()}>
          {(j) => (
            <div class="job-row" classList={{ failed: j.status === "failed" }}>
              <span class={`badge st-${j.status}`}>{j.status}</span>
              <code>{j.kind}</code>
              <span class="dim sm grow">{j.last_error ?? ""}</span>
              <Show when={j.attempts}>
                <span class="dim sm">{j.attempts} attempts</span>
              </Show>
              <time class="dim sm">{relTime(j.created_at)}</time>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};
