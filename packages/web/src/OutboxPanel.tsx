// OutboxPanel.tsx — the offline sync surface: a status pill pinned to the
// viewport corner, and a drawer where queued writes that hit a conflict get
// their human decision (docs/DECISION-offline-conflict-model.md).
//
// The pill appears only when there's something to say: offline, writes
// waiting to replay, or conflicts/failures that need a person. The drawer
// lays a conflicted op next to the server's current row; the choices are the
// decision doc's three — keep mine (re-base and replay), take theirs (the
// server's row stands), discard (throw the queued write away).

import { createSignal, For, Show, type Component } from "solid-js";
import { online } from "./live.ts";
import {
  attentionOps,
  drainOutbox,
  queuedOps,
  resolveDiscard,
  resolveKeepMine,
  resolveTakeTheirs,
  retryOp,
  type OutboxOp,
} from "./outbox.ts";
import { relTime } from "./lib.tsx";
import { StatusDot } from "./primitives.tsx";

const fmtVal = (v: unknown): string => {
  if (v === undefined) return "—";
  if (v === null) return "—";
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > 120 ? `${s.slice(0, 120)}…` : s;
};

/** The fields a queued op wants to change (never the bookkeeping keys). */
const opFields = (op: OutboxOp): string[] =>
  Object.keys(op.body ?? {}).filter((k) => k !== "base_updated_at" && k !== "id");

const OpCard: Component<{ op: OutboxOp }> = (props) => {
  const op = () => props.op;
  const currentRow = () =>
    op().current && typeof op().current === "object"
      ? (op().current as Record<string, unknown>)
      : null;
  const gone = () => op().state === "conflict" && op().status === 404;
  return (
    <div class="conflict-card" classList={{ "conflict-card-queued": op().state === "queued" }}>
      <div class="conflict-head">
        <span class="badge">{op().kind}</span>
        <span class="conflict-label">{op().label}</span>
        <time class="dim sm">{relTime(op().enqueuedAt)}</time>
      </div>

      <Show when={op().state === "queued"}>
        <p class="dim sm conflict-note">Waiting for the network.</p>
      </Show>

      <Show when={gone()}>
        <p class="dim sm conflict-note">
          This row was deleted on the server while you were offline. There's nothing left to apply
          the change to.
        </p>
      </Show>

      <Show when={op().state === "conflict" && op().status === 409}>
        <p class="dim sm conflict-note">
          The server's row changed while you were offline. Yours is on the left, theirs on the
          right.
        </p>
        <div class="conflict-grid">
          <span class="dim sm">field</span>
          <span class="dim sm">yours</span>
          <span class="dim sm">server now</span>
          <For each={opFields(op())}>
            {(k) => (
              <>
                <span class="conflict-field-name">{k}</span>
                <span class="conflict-mine">{fmtVal((op().body ?? {})[k])}</span>
                <span class="conflict-theirs">{fmtVal(currentRow()?.[k])}</span>
              </>
            )}
          </For>
        </div>
      </Show>

      <Show when={op().state === "failed"}>
        <p class="dim sm conflict-note">
          The server rejected this one outright{op().status ? ` (HTTP ${op().status})` : ""}
          {op().error ? ` — ${op().error}` : ""}. Retry it, or let it go.
        </p>
      </Show>

      <Show when={op().state !== "queued"}>
        <div class="conflict-actions">
          <Show when={op().state === "conflict" && op().status === 409}>
            <button class="primary" onClick={() => void resolveKeepMine(op())}>
              keep mine
            </button>
            <button onClick={() => void resolveTakeTheirs(op())}>take theirs</button>
          </Show>
          <Show when={op().state === "failed"}>
            <button class="primary" onClick={() => void retryOp(op())}>
              retry
            </button>
          </Show>
          <button class="ghost" onClick={() => void resolveDiscard(op())}>
            discard
          </button>
        </div>
      </Show>
    </div>
  );
};

export const OutboxPanel: Component = () => {
  const [open, setOpen] = createSignal(false);
  const needsAttention = () => attentionOps().length;
  const waiting = () => queuedOps().length;
  const visible = () => !online() || needsAttention() > 0 || waiting() > 0;

  const pillText = () => {
    if (needsAttention() > 0)
      return `${needsAttention()} change${needsAttention() > 1 ? "s" : ""} need${needsAttention() === 1 ? "s" : ""} your call`;
    if (!online() && waiting() > 0)
      return `offline — ${waiting()} change${waiting() > 1 ? "s" : ""} queued`;
    if (!online()) return "offline — reading from cache";
    return `${waiting()} queued — syncing…`;
  };

  return (
    <>
      <Show when={visible()}>
        <button
          class="sync-pill"
          onClick={() => setOpen(true)}
          title="Offline sync — queued writes and conflicts"
        >
          <StatusDot
            tone={needsAttention() > 0 ? "danger" : online() ? "live" : "waiting"}
            pulse={online() && waiting() > 0 && needsAttention() === 0}
          />
          <span>{pillText()}</span>
        </button>
      </Show>

      <Show when={open()}>
        <div class="drawer-backdrop" onClick={() => setOpen(false)}>
          <div class="drawer" onClick={(ev) => ev.stopPropagation()}>
            <div class="drawer-head">
              <h3>offline sync</h3>
              <button class="x" onClick={() => setOpen(false)}>
                ✕
              </button>
            </div>

            <Show when={!online()}>
              <p class="dim sm">
                You're offline. Reads are coming from this device's cache; writes queue here and
                replay in order when the network returns.
              </p>
            </Show>

            <Show when={needsAttention() > 0}>
              <h4 class="sync-section-head">needs your call</h4>
              <For each={attentionOps()}>{(op) => <OpCard op={op} />}</For>
            </Show>

            <Show when={waiting() > 0}>
              <h4 class="sync-section-head">waiting to send</h4>
              <For each={queuedOps()}>{(op) => <OpCard op={op} />}</For>
              <Show when={online()}>
                <button class="ghost" onClick={() => void drainOutbox()}>
                  ↻ try again now
                </button>
              </Show>
            </Show>

            <Show when={needsAttention() === 0 && waiting() === 0}>
              <p class="dim sm">Nothing queued. Writes you make while offline land here first.</p>
            </Show>
          </div>
        </div>
      </Show>
    </>
  );
};
