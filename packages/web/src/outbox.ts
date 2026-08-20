// outbox.ts — the offline write queue, per docs/DECISION-offline-conflict-model.md.
//
// The model in one paragraph: writes that fail on a NETWORK error (never a 4xx)
// enqueue here with everything replay needs — endpoint, method, body, the
// client-minted id for creates, the base updated_at for patches. Replay is
// strictly ordered, drains on reconnect, and stops at the first hard failure.
// A 409 (base precondition failed) or a 404 (the id dangles — the row was
// deleted while we were offline) parks the op in "conflict" with the server's
// current row attached, and a human picks keep-mine / take-theirs / discard.
// Any other HTTP failure lands in "failed" (the dead-letter state) so a
// rejected write neither blocks the queue forever nor vanishes.
//
// Ops are stamped with the signing-in user's id; after a logout/login as
// someone else, replaying them would mis-attribute authorship, so they fail
// loudly instead.

import { createSignal } from "solid-js";
import { OUTBOX, idbClear, idbDel, idbEntries, idbPut } from "./idb.ts";

export type OutboxKind = "create" | "patch" | "delete" | "action";
export type OutboxState = "queued" | "conflict" | "failed";

export interface OutboxOp {
  /** IndexedDB auto-increment key; key order is replay order. Negative seqs
   *  are the in-memory fallback when IndexedDB is unavailable. */
  seq?: number;
  kind: OutboxKind;
  method: string;
  path: string;
  /** JSON body as an object. Creates carry the client-minted `id`; patches
   *  carry `base_updated_at`. */
  body?: Record<string, unknown>;
  label: string;
  enqueuedAt: string;
  state: OutboxState;
  /** User id at enqueue time; a mismatch at drain fails the op. */
  userId?: string;
  /** 409: the server's current row. 404: null (deleted while offline). */
  current?: unknown;
  status?: number;
  error?: string;
}

const [ops, setOps] = createSignal<OutboxOp[]>([]);
export const queuedOps = () => ops().filter((o) => o.state === "queued");
export const attentionOps = () => ops().filter((o) => o.state !== "queued");

// api.ts injects the signed-in user's id at module load (outbox can't import
// api — api imports outbox).
let getUserId: () => string | undefined = () => undefined;
export const setIdentityGetter = (f: () => string | undefined) => {
  getUserId = f;
};

// live.ts injects its revision bump so a drain that applied writes triggers
// one refetch pass, without outbox importing live.
let afterDrain: () => void = () => {};
export const setAfterDrain = (f: () => void) => {
  afterDrain = f;
};

// Hydrate the signal from IndexedDB once at module load.
let loaded = false;
export async function loadOutbox(): Promise<void> {
  if (loaded) return;
  loaded = true;
  try {
    const entries = await idbEntries<OutboxOp>(OUTBOX);
    setOps(
      entries.map(({ key, value }) => ({
        ...value,
        seq: typeof key === "number" ? key : value.seq,
      })),
    );
  } catch (e) {
    console.warn("outbox: failed to load", e);
  }
}
void loadOutbox();

let memSeq = -1; // in-memory fallback keys when IndexedDB is unavailable

async function persist(op: OutboxOp): Promise<void> {
  if (op.seq === undefined || op.seq < 0) return; // never had a real IDB key
  try {
    await idbPut(OUTBOX, op, op.seq);
  } catch (e) {
    console.warn("outbox: persist failed", e);
  }
}

export async function enqueue(
  input: Omit<OutboxOp, "seq" | "enqueuedAt" | "state" | "userId"> & {
    state?: OutboxState;
    current?: unknown;
    status?: number;
  },
): Promise<OutboxOp> {
  const op: OutboxOp = {
    ...input,
    enqueuedAt: new Date().toISOString(),
    state: input.state ?? "queued",
    userId: getUserId(),
  };
  try {
    const key = await idbPut(OUTBOX, op);
    op.seq = typeof key === "number" ? key : memSeq--;
  } catch (e) {
    console.warn("outbox: persist failed, keeping in memory only", e);
    op.seq = memSeq--;
  }
  setOps((prev) => [...prev, op]);
  return op;
}

async function remove(op: OutboxOp): Promise<void> {
  if (op.seq !== undefined && op.seq >= 0) {
    try {
      await idbDel(OUTBOX, op.seq);
    } catch (e) {
      console.warn("outbox: delete failed", e);
    }
  }
  setOps((prev) => prev.filter((o) => o.seq !== op.seq));
}

async function mark(
  op: OutboxOp,
  patch: Partial<Pick<OutboxOp, "state" | "current" | "status" | "error" | "body">>,
): Promise<void> {
  Object.assign(op, patch);
  await persist(op);
  setOps((prev) => prev.map((o) => (o.seq === op.seq ? { ...op } : o)));
}

/** Drop everything. NOT called on logout — queued writes survive logout so
 *  they can replay for the user who wrote them (the identity stamp guards
 *  against replaying under a different user). */
export async function clearOutbox(): Promise<void> {
  try {
    await idbClear(OUTBOX);
  } catch (e) {
    console.warn("outbox: clear failed", e);
  }
  setOps([]);
}

// ---- replay ----

class ReplayHttp extends Error {
  status: number;
  bodyText: string;
  constructor(status: number, bodyText: string) {
    super(`replay failed: ${status}`);
    this.status = status;
    this.bodyText = bodyText;
  }
}

async function replay(op: OutboxOp): Promise<void> {
  const res = await fetch(`/api${op.path}`, {
    method: op.method,
    credentials: "include",
    headers: { "content-type": "application/json" },
    body: op.body !== undefined ? JSON.stringify(op.body) : undefined,
  });
  if (!res.ok) throw new ReplayHttp(res.status, await res.text());
}

/** The 409 contract: {"error":"conflict","current":{…}}. */
export function parseConflictRow(bodyText: string): unknown {
  try {
    const parsed = JSON.parse(bodyText) as { current?: unknown };
    return parsed.current ?? null;
  } catch {
    return null;
  }
}

let draining = false;

/** Ordered replay. Stops at the first hard failure; a dropped network mid-drain
 *  leaves the rest queued for the next 'online'. */
export async function drainOutbox(): Promise<void> {
  if (draining || !navigator.onLine) return;
  draining = true;
  let applied = 0;
  try {
    for (;;) {
      const next = ops()
        .filter((o) => o.state === "queued")
        .sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0))[0];
      if (!next) break;

      const me = getUserId();
      if (next.userId && me && next.userId !== me) {
        await mark(next, {
          state: "failed",
          error: "Queued by a different signed-in user; replaying it now would mis-attribute the write.",
        });
        break;
      }

      try {
        await replay(next);
        applied++;
        await remove(next);
      } catch (e) {
        if (e instanceof ReplayHttp) {
          if (e.status === 404 && next.kind === "delete") {
            // A replayed delete that 404s already has the desired end state.
            await remove(next);
            continue;
          }
          if (e.status === 409 || e.status === 404) {
            await mark(next, {
              state: "conflict",
              status: e.status,
              current: e.status === 409 ? parseConflictRow(e.bodyText) : null,
            });
          } else {
            await mark(next, {
              state: "failed",
              status: e.status,
              error: e.bodyText.slice(0, 300) || `HTTP ${e.status}`,
            });
          }
          break; // hard failure: the human (or a retry) unblocks the queue
        }
        break; // network dropped again mid-drain; stay queued
      }
    }
  } finally {
    draining = false;
    if (applied > 0) afterDrain();
  }
}

// ---- human resolution (the conflict surface drives these) ----

/** Keep mine: re-base a patch on the server's current row and replay again.
 *  For a 404 (row gone) there is nothing to re-base onto — the UI doesn't
 *  offer this. */
export async function resolveKeepMine(op: OutboxOp): Promise<void> {
  if (
    op.kind === "patch" &&
    op.current &&
    typeof op.current === "object" &&
    "updated_at" in op.current
  ) {
    await mark(op, {
      state: "queued",
      status: undefined,
      current: undefined,
      error: undefined,
      body: {
        ...(op.body ?? {}),
        base_updated_at: (op.current as { updated_at: string }).updated_at,
      },
    });
  } else {
    await mark(op, { state: "queued", status: undefined, current: undefined, error: undefined });
  }
  void drainOutbox();
}

/** Take theirs: the server's row stands; the queued change goes away. */
export async function resolveTakeTheirs(op: OutboxOp): Promise<void> {
  await remove(op);
  void drainOutbox();
}

/** Discard: throw the queued write away without adopting anything. */
export async function resolveDiscard(op: OutboxOp): Promise<void> {
  await remove(op);
  void drainOutbox();
}

/** Retry a dead-lettered op (the failure may have been transient). */
export async function retryOp(op: OutboxOp): Promise<void> {
  await mark(op, { state: "queued", status: undefined, current: undefined, error: undefined });
  void drainOutbox();
}
