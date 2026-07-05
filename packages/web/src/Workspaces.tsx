// Chats — terminal-native agent sessions. The UI keeps a Codex/OpenCode-like
// split: recents/runtime rail on the left, transcript/tool stream in the main
// terminal pane, and a composer asking what the agent should do.
import { createEffect, createMemo, createResource, createSignal, For, on, Show, type Component } from "solid-js";
import { api, type CcMessage, type CcSession } from "./api.ts";
import { Icon } from "./icons.tsx";
import { liveRev } from "./live.ts";
import { EmptyState } from "./primitives.tsx";

type RuntimeId = "codex" | "claude_code" | "opencode";

const RUNTIMES: Array<{ id: RuntimeId; label: string; hint: string; accent: string; modelPlaceholder: string }> = [
  { id: "codex", label: "Codex", hint: "subscription OAuth or token", accent: "blue", modelPlaceholder: "auto" },
  { id: "claude_code", label: "Claude Code", hint: "Anthropic / Claude subscription", accent: "green", modelPlaceholder: "sonnet (default)" },
  { id: "opencode", label: "OpenCode", hint: "provider API key", accent: "red", modelPlaceholder: "e.g. openai/gpt-4.1" },
];

function rel(iso: string | null | undefined): string {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, (Date.now() - t) / 1000);
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

const oneLine = (s: string, max = 90): string => {
  const flat = s.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max)}…` : flat;
};

const metaOf = (s: CcSession | undefined): Record<string, unknown> =>
  s && s.meta && typeof s.meta === "object" && !Array.isArray(s.meta) ? s.meta as Record<string, unknown> : {};

const runtimeOf = (s: CcSession | undefined): RuntimeId => {
  const m = metaOf(s);
  const r = typeof m.runtime === "string" ? m.runtime : undefined;
  if (r === "codex" || r === "opencode" || r === "claude_code") return r;
  return "claude_code";
};
const runtimeInfo = (id: RuntimeId) => RUNTIMES.find((r) => r.id === id) ?? RUNTIMES[1];

const StatusDot: Component<{ status: string }> = (p) => (
  <span class={`chat-dot st-${p.status}`} title={p.status.replace("_", " ")} />
);
const RuntimeDot: Component<{ runtime: RuntimeId }> = (p) => <span class={`runtime-dot rt-${p.runtime}`} />;

type Turn =
  | { t: "user" | "ai"; text: string }
  | { t: "tool"; dir: "in" | "out" | "think"; head: string; body: string }
  | { t: "sys"; text: string };

function toTurn(m: CcMessage): Turn {
  const c = (m.content ?? {}) as Record<string, unknown>;
  const text = typeof c.text === "string" ? c.text : "";
  if (m.kind === "tool_use") {
    const name = typeof c.name === "string" ? c.name : "tool";
    const input = c.input === undefined ? "" : JSON.stringify(c.input, null, 2);
    return { t: "tool", dir: "in", head: `$ ${name} ${oneLine(input)}`, body: input || "{}" };
  }
  if (m.kind === "tool_result") {
    const out = typeof c.output === "string" ? c.output : JSON.stringify(c, null, 2);
    return { t: "tool", dir: "out", head: `↳ ${oneLine(out)}`, body: out };
  }
  if (m.kind === "thinking") return { t: "tool", dir: "think", head: `… ${oneLine(text || "thinking")}`, body: text };
  if (m.role === "user") return { t: "user", text: text || JSON.stringify(c) };
  if (m.role === "assistant") return { t: "ai", text: text || JSON.stringify(c) };
  return { t: "sys", text: oneLine(text || JSON.stringify(c), 160) };
}

const ENDED = new Set(["archived", "completed", "failed"]);

export const Workspaces: Component = () => {
  const [selected, setSelected] = createSignal<string | null>(null);
  const [draft, setDraft] = createSignal("");
  const [runtime, setRuntime] = createSignal<RuntimeId>("codex");
  const [model, setModel] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);
  let scrollEl: HTMLDivElement | undefined;
  let inputEl: HTMLTextAreaElement | undefined;

  const [sessions, { refetch: refetchSessions }] = createResource(
    () => liveRev(),
    () => api.workspaces(100),
  );
  const [transcript] = createResource(
    () => ({ id: selected(), _r: liveRev() }),
    (k) => (k.id ? api.transcript(k.id) : Promise.resolve([] as CcMessage[])),
  );

  const rail = () => sessions.latest?.filter((s) => s.status !== "archived") ?? [];
  const current = (): CcSession | undefined => sessions.latest?.find((s) => s.id === selected());
  const msgs = () => transcript.latest ?? [];
  const canSend = () => {
    const s = current();
    return !!s && !ENDED.has(s.status);
  };
  const counts = createMemo(() => RUNTIMES.map((r) => ({ ...r, count: rail().filter((s) => runtimeOf(s) === r.id).length })));

  createEffect(on(transcript, () => {
    if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight;
  }));
  createEffect(on(selected, () => {
    setErr(null);
    queueMicrotask(() => inputEl?.focus());
  }));

  const submit = async () => {
    const text = draft().trim();
    if (!text || busy()) return;
    setErr(null);
    const id = selected();
    try {
      if (id) {
        setDraft("");
        await api.sendInput(id, text);
      } else {
        setBusy(true);
        const selectedRuntime = runtime();
        const selectedModel = model().trim();
        const info = runtimeInfo(selectedRuntime);
        const ws = await api.createWorkspace({
          title: oneLine(text, 60),
          prompt: text,
          runtime: selectedRuntime,
          provider: selectedRuntime === "opencode" ? "opencode" : info.label.toLowerCase().replace(/ /g, "_"),
          model: selectedModel || undefined,
        });
        setDraft("");
        setSelected(ws.id);
        await refetchSessions();
      }
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const archive = async (id: string) => {
    try {
      await api.archiveWorkspace(id);
      setSelected(null);
      await refetchSessions();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  const onComposerKey = (e: KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  const runtimeControls = () => (
    <div class="runtime-controls" aria-label="Runtime controls">
      <div class="runtime-tabs">
        <For each={RUNTIMES}>
          {(r) => (
            <button class={`runtime-tab rt-${r.id}`} classList={{ active: runtime() === r.id }} onClick={() => setRuntime(r.id)} type="button">
              <RuntimeDot runtime={r.id} />
              <span>{r.label}</span>
            </button>
          )}
        </For>
      </div>
      <Show when={runtime() === "opencode"} fallback={<div class="runtime-hint">{runtimeInfo(runtime()).hint}</div>}>
        <label class="model-field">
          <span>model</span>
          <input
            value={model()}
            placeholder={runtimeInfo(runtime()).modelPlaceholder}
            onInput={(e) => setModel(e.currentTarget.value)}
          />
        </label>
      </Show>
    </div>
  );

  const composer = (placeholder: string) => (
    <div class="chat-composer" classList={{ waiting: current()?.status === "waiting_input" }}>
      <textarea
        ref={inputEl}
        rows="1"
        placeholder={placeholder}
        value={draft()}
        onInput={(e) => setDraft(e.currentTarget.value)}
        onKeyDown={onComposerKey}
        aria-label={placeholder}
      />
      <button class="write-send" onClick={submit} disabled={busy() || !draft().trim()} title="Send (Enter)" aria-label="Send">
        ↵
      </button>
    </div>
  );

  return (
    <div class="chat">
      <div class="chat-rail">
        <button class="chat-new" onClick={() => { setSelected(null); queueMicrotask(() => inputEl?.focus()); }}>
          <Icon name="chats" size={15} /> New agent run
        </button>
        <div class="runtime-strip">
          <For each={counts()}>
            {(r) => <span class={`runtime-pill rt-${r.id}`}><RuntimeDot runtime={r.id} /> {r.label} <b>{r.count}</b></span>}
          </For>
        </div>
        <div class="chat-rows">
          <For
            each={rail()}
            fallback={<EmptyState icon="chats" title="No chats yet." hint="Pick a runtime and describe a task." />}
          >
            {(s) => (
              <button class="chat-row" classList={{ selected: selected() === s.id }} onClick={() => setSelected(s.id)}>
                <span class="chat-row-title">{s.title || "Untitled run"}</span>
                <span class="chat-row-meta">
                  <RuntimeDot runtime={runtimeOf(s)} />
                  {runtimeInfo(runtimeOf(s)).label}
                  <StatusDot status={s.status} />
                  {rel(s.last_activity_at ?? s.created_at)}
                </span>
              </button>
            )}
          </For>
        </div>
      </div>

      <div class="chat-main">
        <Show
          when={current()}
          fallback={
            <div class="chat-hero terminal-panel">
              <span class="chat-hero-icon"><Icon name="chats" size={30} /></span>
              <h3>What should the agent do?</h3>
              <p class="dim">Choose a runtime, then start a sandboxed terminal chat that writes back to the hive.</p>
              {runtimeControls()}
              {composer("Describe the task…")}
              <Show when={err()}><div class="chat-err">{err()}</div></Show>
            </div>
          }
        >
          {(s) => {
            const rt = () => runtimeOf(s());
            return (
              <>
                <div class="chat-head">
                  <span class="chat-title">{s().title || "Untitled run"}</span>
                  <span class={`runtime-pill rt-${rt()}`}><RuntimeDot runtime={rt()} /> {runtimeInfo(rt()).label}</span>
                  <Show when={s().model}><code class="chat-model">{s().model}</code></Show>
                  <span class="chat-status">
                    <StatusDot status={s().status} />
                    {s().status.replace("_", " ")}
                  </span>
                  <button class="x" onClick={() => archive(s().id)} title="Archive chat" aria-label="Archive chat">✕</button>
                </div>
                <div class="chat-status-strip">
                  <span>owner <code>@{s().owner}</code></span>
                  <span>workdir <code>{s().workdir}</code></span>
                  <span>{msgs().length} rows</span>
                </div>
                <div class="chat-scroll terminal-panel" ref={scrollEl}>
                  <Show when={msgs().length > 0} fallback={<p class="dim sm">No transcript yet — runtime is provisioning.</p>}>
                    <For each={msgs()}>
                      {(m) => {
                        const turn = toTurn(m);
                        return turn.t === "user" ? (
                          <div class="chat-turn-user"><span class="turn-label">you</span>{turn.text}</div>
                        ) : turn.t === "ai" ? (
                          <div class="chat-turn-ai"><span class="turn-label">agent</span>{turn.text}</div>
                        ) : turn.t === "tool" ? (
                          <details class={`chat-tool tool-${turn.dir}`}>
                            <summary><span class="tool-kind">{turn.dir}</span>{turn.head}</summary>
                            <pre>{turn.body}</pre>
                          </details>
                        ) : (
                          <div class="chat-sys"># {turn.text}</div>
                        );
                      }}
                    </For>
                  </Show>
                </div>
                <Show when={canSend()} fallback={<div class="chat-ended">This run has ended.</div>}>
                  {composer(s().status === "waiting_input" ? "Runtime is waiting on you…" : "Reply…")}
                </Show>
                <Show when={err()}><div class="chat-err">{err()}</div></Show>
              </>
            );
          }}
        </Show>
      </div>
    </div>
  );
};
