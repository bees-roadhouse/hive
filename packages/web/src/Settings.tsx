import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTOR_NAMES, SEVERITIES, type Severity, type SourceKind } from "@hive/shared";
import { api, getActor } from "./api.ts";
import { relTime } from "./lib.tsx";
import { liveRev } from "./live.ts";
import { EmptyState } from "./primitives.tsx";

/** Worker configuration: ingest sources (GUI ⇄ MCP), status, outbound queue,
 * and the runtime credentials that power hosted conversations. */
export const Settings: Component = () => {
  const actor = getActor();
  const [sources, { refetch }] = createResource(() => ({ _r: liveRev() }), () => api.sources(actor));
  const [status, { refetch: refetchStatus }] = createResource(() => ({ _r: liveRev() }), () => api.worker());
  const [outbox] = createResource(() => ({ _r: liveRev() }), () => api.outbox());
  const [creds, { refetch: refetchCreds }] = createResource(() => ({ _r: liveRev() }), () => api.ccCredentials());

  const [credForm, setCredForm] = createSignal({ kind: "codex_oauth", label: "", secret: "" });
  const [credPanelOpen, setCredPanelOpen] = createSignal(false);
  let secretInput: HTMLInputElement | undefined;
  const credentialDefaults = (kind: string) => {
    switch (kind) {
      case "codex_oauth":
        return { kind: "oauth_token", runtime: "codex", label: "Codex subscription" };
      case "codex_api_key":
        return { kind: "api_key", runtime: "codex", label: "Codex API key" };
      case "claude_oauth":
        return { kind: "oauth_token", runtime: "claude_code", label: "Claude Code subscription" };
      case "anthropic_api_key":
        return { kind: "api_key", runtime: "claude_code", provider: "anthropic", label: "Anthropic API key" };
      case "opencode_provider_key":
        return { kind: "api_key", runtime: "opencode", label: "OpenCode provider key" };
      default:
        return { kind, runtime: "claude_code", label: kind };
    }
  };
  const connectRuntime = (kind: "codex_oauth" | "claude_oauth") => {
    const defaults = credentialDefaults(kind);
    setCredForm({ kind, label: defaults.label, secret: "" });
    setCredPanelOpen(true);
    queueMicrotask(() => secretInput?.focus());
  };
  const saveCred = async (e: Event) => {
    e.preventDefault();
    const f = credForm();
    if (!f.secret.trim()) return;
    const defaults = credentialDefaults(f.kind);
    await api.saveCcCredential({
      kind: defaults.kind,
      runtime: defaults.runtime,
      provider: defaults.provider,
      label: f.label.trim() || defaults.label,
      secret: f.secret.trim(),
    });
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

      <For
        each={sources()}
        fallback={<EmptyState icon="wire" title="No sources yet." hint="Add one above to feed the wire." />}
      >
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
      <Show
        when={outbox()?.length}
        fallback={<EmptyState icon="inbox" title="No outbound jobs." hint="Deliveries queue here on their way out." />}
      >
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

      <h3 class="sec">Agent runtime sign-in</h3>
      <p class="dim sm">Connect Codex or Claude Code once, then choose that runtime when starting a Conversation. Secrets are encrypted and never shown again.</p>

      <div class="runtime-connect-cards">
        <button type="button" class="runtime-connect-card rt-codex" onClick={() => connectRuntime("codex_oauth")}>
          <strong>Connect Codex</strong>
          <span>Use your Codex subscription token from the Codex sign-in flow.</span>
          <em>Continue with Codex</em>
        </button>
        <button type="button" class="runtime-connect-card rt-claude_code" onClick={() => connectRuntime("claude_oauth")}>
          <strong>Connect Claude Code</strong>
          <span>Use your Claude Code setup token or subscription token.</span>
          <em>Continue with Claude Code</em>
        </button>
      </div>

      <details class="runtime-advanced" open={credPanelOpen()} onToggle={(e) => setCredPanelOpen(e.currentTarget.open)}>
        <summary>Advanced: paste or replace a credential manually</summary>

      <form class="source-form runtime-cred-form" onSubmit={saveCred}>
        <select value={credForm().kind} onChange={(e) => setCredForm({ ...credForm(), kind: e.currentTarget.value })}>
          <option value="codex_oauth">Codex subscription OAuth/token</option>
          <option value="codex_api_key">Codex API token</option>
          <option value="claude_oauth">Claude Code subscription OAuth token</option>
          <option value="anthropic_api_key">Claude Code / Anthropic API key</option>
          <option value="opencode_provider_key">OpenCode provider API key</option>
        </select>
        <input placeholder="label / provider hint (optional)" value={credForm().label} onInput={(e) => setCredForm({ ...credForm(), label: e.currentTarget.value })} />
        <input ref={secretInput} class="grow" type="password" placeholder="paste token or API key" value={credForm().secret} onInput={(e) => setCredForm({ ...credForm(), secret: e.currentTarget.value })} />
        <button class="primary" type="submit">save credential</button>
      </form>
      </details>

      <For
        each={creds()}
        fallback={<EmptyState icon="chats" title="No runtime credentials yet." hint="Conversations can't start until a runtime token or provider key is saved." />}
      >
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
