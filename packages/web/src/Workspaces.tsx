// Workspaces — hosted Claude Code sessions hive spins up and drives in isolated
// sandboxes. Left: your sessions + a "new session" form + your Claude Code
// credentials. Right: the live transcript of the selected session + an input box.
// Everything refetches on liveRev() (any SSE event), so transcripts stream in.
import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { api, type CcMessage, type CcSession } from "./api.ts";
import { liveRev } from "./live.ts";

const STATUS_COLOR: Record<string, string> = {
  provisioning: "#b58900",
  running: "#268bd2",
  idle: "#2aa198",
  waiting_input: "#6c71c4",
  completed: "#859900",
  failed: "#dc322f",
  archived: "#586e75",
};

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

const StatusChip: Component<{ status: string }> = (p) => (
  <span
    style={{
      background: STATUS_COLOR[p.status] ?? "#586e75",
      color: "#fff",
      "border-radius": "4px",
      padding: "1px 6px",
      "font-size": "11px",
      "text-transform": "uppercase",
      "letter-spacing": "0.04em",
    }}
  >
    {p.status}
  </span>
);

function messageText(m: CcMessage): string {
  const c = (m.content ?? {}) as Record<string, unknown>;
  if (typeof c.text === "string") return c.text;
  if (m.kind === "tool_use" && typeof c.name === "string") {
    return `→ ${c.name}(${c.input ? JSON.stringify(c.input) : ""})`;
  }
  if (m.kind === "tool_result") return typeof c.output === "string" ? c.output : JSON.stringify(c);
  return JSON.stringify(c);
}

const Message: Component<{ m: CcMessage }> = (p) => {
  const muted = () => p.m.role === "system" || p.m.kind === "thinking";
  return (
    <div style={{ margin: "8px 0", opacity: muted() ? 0.7 : 1 }}>
      <div class="dim" style={{ "font-size": "11px", "margin-bottom": "2px" }}>
        {p.m.role} · {p.m.kind} · #{p.m.seq}
      </div>
      <pre
        style={{
          margin: 0,
          "white-space": "pre-wrap",
          "word-break": "break-word",
          "font-family": p.m.kind === "tool_use" || p.m.kind === "tool_result" ? "monospace" : "inherit",
          "font-size": "13px",
        }}
      >
        {messageText(p.m)}
      </pre>
    </div>
  );
};

export const Workspaces: Component = () => {
  const [selected, setSelected] = createSignal<string | null>(null);
  const [title, setTitle] = createSignal("");
  const [prompt, setPrompt] = createSignal("");
  const [input, setInput] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);

  const [sessions, { refetch: refetchSessions }] = createResource(
    () => liveRev(),
    () => api.workspaces(100),
  );
  const [transcript] = createResource(
    () => ({ id: selected(), _r: liveRev() }),
    (k) => (k.id ? api.transcript(k.id) : Promise.resolve([] as CcMessage[])),
  );
  const [creds, { refetch: refetchCreds }] = createResource(
    () => liveRev(),
    () => api.ccCredentials(),
  );

  const current = (): CcSession | undefined => sessions()?.find((s) => s.id === selected());

  const start = async () => {
    setErr(null);
    setBusy(true);
    try {
      const ws = await api.createWorkspace({
        title: title().trim() || undefined,
        prompt: prompt().trim() || undefined,
      });
      setTitle("");
      setPrompt("");
      setSelected(ws.id);
      await refetchSessions();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const send = async () => {
    const id = selected();
    const text = input().trim();
    if (!id || !text) return;
    setInput("");
    try {
      await api.sendInput(id, text);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  const archive = async (id: string) => {
    try {
      await api.archiveWorkspace(id);
      await refetchSessions();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  // ---- credentials sub-form ----
  const [credKind, setCredKind] = createSignal("oauth_token");
  const [credLabel, setCredLabel] = createSignal("");
  const [credSecret, setCredSecret] = createSignal("");
  const saveCred = async () => {
    if (!credSecret().trim()) return;
    try {
      await api.saveCcCredential({ kind: credKind(), label: credLabel().trim() || undefined, secret: credSecret().trim() });
      setCredSecret("");
      setCredLabel("");
      await refetchCreds();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div style={{ display: "grid", "grid-template-columns": "340px 1fr", gap: "16px", "align-items": "start" }}>
      {/* LEFT: new session + list + credentials */}
      <div style={{ display: "flex", "flex-direction": "column", gap: "16px" }}>
        <div style={{ border: "1px solid var(--line, #333)", "border-radius": "8px", padding: "12px" }}>
          <strong>New session</strong>
          <input
            placeholder="title (optional)"
            value={title()}
            onInput={(e) => setTitle(e.currentTarget.value)}
            style={{ width: "100%", "margin-top": "8px" }}
          />
          <textarea
            placeholder="first prompt — what should Claude Code do?"
            rows="3"
            value={prompt()}
            onInput={(e) => setPrompt(e.currentTarget.value)}
            style={{ width: "100%", "margin-top": "8px" }}
          />
          <button onClick={start} disabled={busy()} style={{ "margin-top": "8px" }}>
            {busy() ? "Starting…" : "Start session"}
          </button>
        </div>

        <div>
          <div class="dim" style={{ "margin-bottom": "6px" }}>Sessions</div>
          <Show when={(sessions()?.length ?? 0) > 0} fallback={<div class="dim">No sessions yet.</div>}>
            <For each={sessions()}>
              {(s) => (
                <div
                  onClick={() => setSelected(s.id)}
                  style={{
                    border: "1px solid var(--line, #333)",
                    "border-radius": "6px",
                    padding: "8px",
                    "margin-bottom": "6px",
                    cursor: "pointer",
                    background: selected() === s.id ? "rgba(120,120,120,0.15)" : "transparent",
                  }}
                >
                  <div style={{ display: "flex", "justify-content": "space-between", gap: "6px" }}>
                    <strong style={{ overflow: "hidden", "text-overflow": "ellipsis", "white-space": "nowrap" }}>
                      {s.title || "(untitled)"}
                    </strong>
                    <StatusChip status={s.status} />
                  </div>
                  <div class="dim" style={{ "font-size": "11px", "margin-top": "2px" }}>
                    {s.owner} · {rel(s.last_activity_at ?? s.created_at)}
                  </div>
                </div>
              )}
            </For>
          </Show>
        </div>

        <div style={{ border: "1px solid var(--line, #333)", "border-radius": "8px", padding: "12px" }}>
          <strong>Claude Code credentials</strong>
          <div class="dim" style={{ "font-size": "12px", "margin": "4px 0 8px" }}>
            Stored encrypted; used to run your sessions. Never shown again.
          </div>
          <For each={creds()}>
            {(c) => (
              <div style={{ display: "flex", "justify-content": "space-between", "align-items": "center", "margin-bottom": "4px" }}>
                <span style={{ "font-size": "12px" }}>
                  {c.kind} <code>{c.tail}</code> {c.label ? `· ${c.label}` : ""}
                </span>
                <button onClick={() => api.deleteCcCredential(c.id).then(refetchCreds)} style={{ "font-size": "11px" }}>
                  delete
                </button>
              </div>
            )}
          </For>
          <select value={credKind()} onChange={(e) => setCredKind(e.currentTarget.value)} style={{ "margin-top": "8px", width: "100%" }}>
            <option value="oauth_token">Subscription OAuth token (claude setup-token)</option>
            <option value="api_key">Anthropic API key</option>
          </select>
          <input
            placeholder="label (optional)"
            value={credLabel()}
            onInput={(e) => setCredLabel(e.currentTarget.value)}
            style={{ width: "100%", "margin-top": "6px" }}
          />
          <input
            type="password"
            placeholder="paste secret"
            value={credSecret()}
            onInput={(e) => setCredSecret(e.currentTarget.value)}
            style={{ width: "100%", "margin-top": "6px" }}
          />
          <button onClick={saveCred} style={{ "margin-top": "6px" }}>Save credential</button>
        </div>
      </div>

      {/* RIGHT: transcript + input */}
      <div>
        <Show when={current()} fallback={<div class="dim">Select or start a session to see its transcript.</div>}>
          {(s) => (
            <div>
              <div style={{ display: "flex", "justify-content": "space-between", "align-items": "center", "margin-bottom": "8px" }}>
                <div>
                  <strong>{s().title || "(untitled)"}</strong> <StatusChip status={s().status} />
                  <div class="dim" style={{ "font-size": "11px" }}>
                    {s().workdir}{s().claude_session_id ? ` · cc:${s().claude_session_id.slice(0, 8)}` : ""}
                  </div>
                </div>
                <Show when={s().status !== "archived"}>
                  <button onClick={() => archive(s().id)} style={{ "font-size": "12px" }}>Archive</button>
                </Show>
              </div>

              <div
                style={{
                  border: "1px solid var(--line, #333)",
                  "border-radius": "8px",
                  padding: "12px",
                  "min-height": "240px",
                  "max-height": "60vh",
                  "overflow-y": "auto",
                }}
              >
                <Show when={(transcript()?.length ?? 0) > 0} fallback={<div class="dim">No messages yet.</div>}>
                  <For each={transcript()}>{(m) => <Message m={m} />}</For>
                </Show>
              </div>

              <Show when={s().status !== "archived" && s().status !== "completed"}>
                <div style={{ display: "flex", gap: "8px", "margin-top": "8px" }}>
                  <textarea
                    placeholder="send input to the session…"
                    rows="2"
                    value={input()}
                    onInput={(e) => setInput(e.currentTarget.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) send();
                    }}
                    style={{ flex: 1 }}
                  />
                  <button onClick={send}>Send</button>
                </div>
              </Show>
            </div>
          )}
        </Show>
      </div>

      <Show when={err()}>
        <div style={{ "grid-column": "1 / -1", color: "#dc322f", "font-size": "12px" }}>{err()}</div>
      </Show>
    </div>
  );
};
