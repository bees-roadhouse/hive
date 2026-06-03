import { createResource, For, Show, type Component } from "solid-js";
import { api } from "./api.ts";
import { relTime } from "./lib.tsx";

/** Operational view: worker heartbeat + last cycle, embedding coverage, and the
 * outbound job queue. Read-only — the worker drives the writes. */
export const Admin: Component = () => {
  const [worker, { refetch: rw }] = createResource(() => api.worker());
  const [emb, { refetch: re }] = createResource(() => api.embeddings());
  const [outbox, { refetch: ro }] = createResource(() => api.outbox());
  const refresh = () => {
    rw();
    re();
    ro();
  };

  const coverage = (e: { embeddable: number; pending: number }) =>
    e.embeddable ? Math.round(((e.embeddable - e.pending) / e.embeddable) * 100) : 100;

  return (
    <section class="admin">
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
