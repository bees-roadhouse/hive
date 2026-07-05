// Chats — hosted Claude Code sessions, presented as conversations rather than
// an ops panel. Recents rail on the left; the selected session renders as
// turns (your messages as bubbles, Claude's prose in the serif voice, tool
// traffic collapsed to quiet expandable rows) with a composer pinned at the
// bottom. Starting a chat is just typing the first prompt — the title derives
// from it. Credentials management lives in Settings, not here.
// Everything refetches on liveRev() (any SSE event), so transcripts stream in.
import { createEffect, createResource, createSignal, For, on, Show, type Component } from "solid-js";
import { api, type CcMessage, type CcSession } from "./api.ts";
import { Icon } from "./icons.tsx";
import { liveRev } from "./live.ts";
import { EmptyState } from "./primitives.tsx";

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

const StatusDot: Component<{ status: string }> = (p) => (
  <span class={`chat-dot st-${p.status}`} title={p.status.replace("_", " ")} />
);

// A transcript row folded into one of four calm shapes: your turn, Claude's
// turn, collapsed machine traffic (tools/thinking), or a system whisper.
type Turn =
  | { t: "user" | "ai"; text: string }
  | { t: "tool"; head: string; body: string }
  | { t: "sys"; text: string };

function toTurn(m: CcMessage): Turn {
  const c = (m.content ?? {}) as Record<string, unknown>;
  const text = typeof c.text === "string" ? c.text : "";
  if (m.kind === "tool_use") {
    const name = typeof c.name === "string" ? c.name : "tool";
    const input = c.input === undefined ? "" : JSON.stringify(c.input, null, 2);
    return { t: "tool", head: `→ ${name} ${oneLine(input)}`, body: input };
  }
  if (m.kind === "tool_result") {
    const out = typeof c.output === "string" ? c.output : JSON.stringify(c, null, 2);
    return { t: "tool", head: `← ${oneLine(out)}`, body: out };
  }
  if (m.kind === "thinking") return { t: "tool", head: `✳ ${oneLine(text || "thinking…")}`, body: text };
  if (m.role === "user") return { t: "user", text: text || JSON.stringify(c) };
  if (m.role === "assistant") return { t: "ai", text: text || JSON.stringify(c) };
  return { t: "sys", text: oneLine(text || JSON.stringify(c), 160) };
}

// Sessions still doing (or awaiting) something; the rail hides archived ones.
const ENDED = new Set(["archived", "completed", "failed"]);

export const Workspaces: Component = () => {
  const [selected, setSelected] = createSignal<string | null>(null);
  const [draft, setDraft] = createSignal("");
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

  // Both resources re-key on every SSE event; read .latest so an in-flight
  // refetch shows the previous state instead of flashing the UI empty.
  const rail = () => sessions.latest?.filter((s) => s.status !== "archived") ?? [];
  const current = (): CcSession | undefined => sessions.latest?.find((s) => s.id === selected());
  const msgs = () => transcript.latest ?? [];
  const canSend = () => {
    const s = current();
    return !!s && !ENDED.has(s.status);
  };

  // Keep the newest turn in view as transcripts stream, and hand focus to the
  // composer whenever the conversation (or the blank slate) changes.
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
        const ws = await api.createWorkspace({
          title: oneLine(text, 60),
          prompt: text,
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
        ↑
      </button>
    </div>
  );

  return (
    <div class="chat">
      <div class="chat-rail">
        <button class="chat-new" onClick={() => { setSelected(null); queueMicrotask(() => inputEl?.focus()); }}>
          <Icon name="chats" size={15} /> New chat
        </button>
        <div class="chat-rows">
          <For
            each={rail()}
            fallback={<EmptyState icon="chats" title="No chats yet." hint="Start one by describing a task." />}
          >
            {(s) => (
              <button class="chat-row" classList={{ selected: selected() === s.id }} onClick={() => setSelected(s.id)}>
                <span class="chat-row-title">{s.title || "Untitled chat"}</span>
                <span class="chat-row-meta">
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
            <div class="chat-hero">
              <span class="chat-hero-icon"><Icon name="chats" size={30} /></span>
              <h3>What should Claude Code do?</h3>
              <p class="dim">Each chat runs in its own hosted sandbox and writes back to the hive.</p>
              {composer("Describe the task…")}
              <Show when={err()}><div class="chat-err">{err()}</div></Show>
            </div>
          }
        >
          {(s) => (
            <>
              <div class="chat-head">
                <span class="chat-title">{s().title || "Untitled chat"}</span>
                <span class="chat-status">
                  <StatusDot status={s().status} />
                  {s().status.replace("_", " ")}
                </span>
                <button class="x" onClick={() => archive(s().id)} title="Archive chat" aria-label="Archive chat">✕</button>
              </div>
              <div class="chat-scroll" ref={scrollEl}>
                <Show when={msgs().length > 0} fallback={<p class="dim sm">Nothing here yet — Claude is warming up.</p>}>
                  <For each={msgs()}>
                    {(m) => {
                      const turn = toTurn(m);
                      return turn.t === "user" ? (
                        <div class="chat-turn-user">{turn.text}</div>
                      ) : turn.t === "ai" ? (
                        <div class="chat-turn-ai">{turn.text}</div>
                      ) : turn.t === "tool" ? (
                        <details class="chat-tool">
                          <summary>{turn.head}</summary>
                          <pre>{turn.body}</pre>
                        </details>
                      ) : (
                        <div class="chat-sys">{turn.text}</div>
                      );
                    }}
                  </For>
                </Show>
              </div>
              <Show when={canSend()} fallback={<div class="chat-ended">This chat has ended.</div>}>
                {composer(s().status === "waiting_input" ? "Claude is waiting on you…" : "Reply…")}
              </Show>
              <Show when={err()}><div class="chat-err">{err()}</div></Show>
            </>
          )}
        </Show>
      </div>
    </div>
  );
};
