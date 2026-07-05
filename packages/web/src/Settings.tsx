import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTOR_NAMES, SEVERITIES, type Severity, type SourceKind } from "@hive/shared";
import { api, getActor } from "./api.ts";
import { relTime } from "./lib.tsx";
import { liveRev } from "./live.ts";

/** Worker configuration: ingest sources (GUI ⇄ MCP), status, outbound queue,
 * and the Claude Code credentials that power hosted chats. */
export const Settings: Component = () => {
  const actor = getActor();
  const [sources, { refetch }] = createResource(() => ({ _r: liveRev() }), () => api.sources(actor));
  const [status, { refetch: refetchStatus }] = createResource(() => ({ _r: liveRev() }), () => api.worker());
  const [outbox] = createResource(() => ({ _r: liveRev() }), () => api.outbox());
  const [creds, { refetch: refetchCreds }] = createResource(() => ({ _r: liveRev() }), () => api.ccCredentials());

  const [credForm, setCredForm] = createSignal({ kind: "oauth_token", label: "", secret: "" });
  const saveCred = async (e: Event) => {
    e.preventDefault();
    const f = credForm();
    if (!f.secret.trim()) return;
    await api.saveCcCredential({ kind: f.kind, label: f.label.trim() || undefined, secret: f.secret.trim() });
    setCredForm({ kind: f.kind, label: "", secret: "" });
    refetchCreds();
  };
  const [form, setForm] = createSignal({
    name: "",
    url: "",
    kind: "rss" as SourceKind,
    severity: "info" as Severity,
    notify: "",
    scope: "global" as "global" | "me",
  });

  const refreshAll = () => {
    refetch();
    refetchStatus();
  };

  const add = async (e: Event) => {
    e.preventDefault();
    const f = form();
    if (!f.name.trim() || !f.url.trim()) return;
    await api.addSource({
      name: f.name,
      url: f.url,
      kind: f.kind,
      severity: f.severity,
      notify: f.notify || null,
      scope: f.scope,
    });
    setForm({ name: "", url: "", kind: "rss", severity: "info", notify: "", scope: "global" });
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
        <input class="grow" placeholder="https://…" value={form().url} onInput={(e) => setForm({ ...form(), url: e.currentTarget.value })} />
        <select value={form().kind} onChange={(e) => setForm({ ...form(), kind: e.currentTarget.value as SourceKind })}>
          <option value="rss">rss</option>
          <option value="scrape">scrape</option>
        </select>
        <select value={form().severity} onChange={(e) => setForm({ ...form(), severity: e.currentTarget.value as Severity })}>
          <For each={SEVERITIES}>{(s) => <option value={s}>{s}</option>}</For>
        </select>
        <select value={form().notify} onChange={(e) => setForm({ ...form(), notify: e.currentTarget.value })}>
          <option value="">no notify</option>
          <For each={ACTOR_NAMES}>{(a) => <option value={a}>notify @{a}</option>}</For>
        </select>
        <select value={form().scope} onChange={(e) => setForm({ ...form(), scope: e.currentTarget.value as "global" | "me" })}>
          <option value="global">global</option>
          <option value="me">personal</option>
        </select>
        <button class="primary" type="submit">add source</button>
      </form>

      <For each={sources()} fallback={<p class="dim sm pad">no sources yet — add one above.</p>}>
        {(s) => (
          <div class="source-row" classList={{ off: !s.enabled }}>
            <label class="sw">
              <input type="checkbox" checked={s.enabled} onChange={(e) => toggle(s.id, e.currentTarget.checked)} />
            </label>
            <div class="source-main">
              <div class="source-name">
                {s.name}
                <span class="badge">{s.kind}</span>
                <span class="badge">{s.severity}</span>
                <span class="badge dim">{s.owner ? `@${s.owner}` : "global"}</span>
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

      <h3 class="sec">Claude Code</h3>
      <p class="dim sm">Credentials that run your hosted chats. Stored encrypted; the secret is never shown again.</p>

      <form class="source-form" onSubmit={saveCred}>
        <select value={credForm().kind} onChange={(e) => setCredForm({ ...credForm(), kind: e.currentTarget.value })}>
          <option value="oauth_token">Subscription OAuth token (claude setup-token)</option>
          <option value="api_key">Anthropic API key</option>
        </select>
        <input placeholder="label (optional)" value={credForm().label} onInput={(e) => setCredForm({ ...credForm(), label: e.currentTarget.value })} />
        <input class="grow" type="password" placeholder="paste secret" value={credForm().secret} onInput={(e) => setCredForm({ ...credForm(), secret: e.currentTarget.value })} />
        <button class="primary" type="submit">save credential</button>
      </form>

      <For each={creds()} fallback={<p class="dim sm pad">no credentials yet — chats can't start without one.</p>}>
        {(c) => (
          <div class="wire-row">
            <span class="badge">{c.kind}</span>
            <code>…{c.tail}</code>
            <span class="dim sm">{c.label}</span>
            <span class="dim sm">{c.last_used_at ? `used ${relTime(c.last_used_at)}` : `added ${relTime(c.created_at)}`}</span>
            <button class="x" onClick={() => api.deleteCcCredential(c.id).then(refetchCreds)}>✕</button>
          </div>
        )}
      </For>
    </section>
  );
};
