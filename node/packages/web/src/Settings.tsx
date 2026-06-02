import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTOR_NAMES, SEVERITIES, type Severity } from "@hive/shared";
import { api } from "./api.ts";
import { relTime } from "./lib.tsx";

/** Worker configuration: ingest sources (GUI ⇄ MCP), status, outbound queue. */
export const Settings: Component = () => {
  const [sources, { refetch }] = createResource(() => api.sources());
  const [status, { refetch: refetchStatus }] = createResource(() => api.worker());
  const [outbox] = createResource(() => api.outbox());
  const [form, setForm] = createSignal({ name: "", url: "", severity: "info" as Severity, notify: "" });

  const refreshAll = () => {
    refetch();
    refetchStatus();
  };

  const add = async (e: Event) => {
    e.preventDefault();
    const f = form();
    if (!f.name.trim() || !f.url.trim()) return;
    await api.addSource({ name: f.name, url: f.url, severity: f.severity, notify: f.notify || null });
    setForm({ name: "", url: "", severity: "info", notify: "" });
    refreshAll();
  };
  const toggle = async (id: string, enabled: boolean) => {
    await api.patchSource(id, { enabled });
    refreshAll();
  };
  const remove = async (id: string) => {
    await api.delSource(id);
    refreshAll();
  };

  return (
    <section class="settings">
      <Show when={status()}>
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
                    last run {relTime(r().at)} · polled {r().polled} · ingested {r().ingested} · outbox{" "}
                    {r().outbox} · embedded {r().embedded} · {r().maintenance.join(", ")}
                  </div>
                )}
              </Show>
            </div>
            <div class="ws-stats">
              <span class="badge">{s().embeddings.count} embeddings</span>
              <span class="badge">{s().embeddings.model}</span>
              <span class="badge">outbox {s().outbox.pending}p/{s().outbox.failed}f</span>
            </div>
          </div>
        )}
      </Show>

      <h3 class="sec">Ingest sources</h3>
      <p class="dim sm">Feeds the worker polls into the wire. Configurable here or via MCP (<code>sources_add</code>).</p>

      <form class="source-form" onSubmit={add}>
        <input placeholder="name" value={form().name} onInput={(e) => setForm({ ...form(), name: e.currentTarget.value })} />
        <input class="grow" placeholder="https://…/feed.xml" value={form().url} onInput={(e) => setForm({ ...form(), url: e.currentTarget.value })} />
        <select value={form().severity} onChange={(e) => setForm({ ...form(), severity: e.currentTarget.value as Severity })}>
          <For each={SEVERITIES}>{(s) => <option value={s}>{s}</option>}</For>
        </select>
        <select value={form().notify} onChange={(e) => setForm({ ...form(), notify: e.currentTarget.value })}>
          <option value="">no notify</option>
          <For each={ACTOR_NAMES}>{(a) => <option value={a}>notify @{a}</option>}</For>
        </select>
        <button class="primary" type="submit">add source</button>
      </form>

      <For each={sources()}>
        {(s) => (
          <div class="source-row" classList={{ off: !s.enabled }}>
            <label class="sw">
              <input type="checkbox" checked={s.enabled} onChange={(e) => toggle(s.id, e.currentTarget.checked)} />
            </label>
            <div class="source-main">
              <div class="source-name">
                {s.name} <span class="badge">{s.severity}</span>
                <Show when={s.notify}><span class="actor-chip sm">@{s.notify}</span></Show>
              </div>
              <div class="dim sm">{s.url}</div>
              <Show when={s.last_status}>
                <div class="dim sm">last poll: {s.last_status} · {s.last_polled_at ? relTime(s.last_polled_at) : "never"}</div>
              </Show>
            </div>
            <span class="dim sm">every {Math.round(s.interval_secs / 60)}m</span>
            <button class="x" onClick={() => remove(s.id)}>✕</button>
          </div>
        )}
      </For>

      <h3 class="sec">Outbound queue</h3>
      <Show when={outbox()?.length} fallback={<p class="dim sm">no outbound jobs.</p>}>
        <For each={outbox()}>
          {(j) => (
            <div class="wire-row">
              <span class={`badge st-${j.status}`}>{j.status}</span>
              <code>{j.kind}</code>
              <span class="dim sm">{j.attempts ? `${j.attempts} attempts` : ""}</span>
              <time>{relTime(j.created_at)}</time>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};
