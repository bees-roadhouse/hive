// hive worker — a long-running node process that does the out-of-band work the
// request-serving API shouldn't: polling feeds into wire events, draining the
// outbound queue, refreshing embeddings, and keeping the SQLite db tidy.
//
//   pnpm --filter @hive/worker start     # loop forever
//   pnpm --filter @hive/worker once      # one cycle then exit (CI / demo)
import {
  embeddings,
  emit,
  outbox,
  pollSources,
  setHeartbeat,
  setLastRun,
} from "@hive/api/store";
import { db } from "@hive/api/db";
import { logger } from "@hive/api/log";

const log = logger("worker");
const TICK_SECS = Number(process.env.HIVE_WORKER_TICK ?? 30);
const once = process.argv.includes("--once");

async function drainOutbox(): Promise<number> {
  let done = 0;
  for (const job of outbox.claim(20)) {
    try {
      if (job.kind === "webhook") {
        const p = job.payload as { url: string; body?: unknown };
        const res = await fetch(p.url, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(p.body ?? {}),
        });
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
      } else {
        // "log" and unknown kinds just succeed (room to grow).
        log.debug(`outbox job ran`, { kind: job.kind });
      }
      outbox.complete(job.id);
      done++;
    } catch (err) {
      // Expected/transient (a webhook 5xx, a flaky endpoint) — one clean line,
      // no stack; the job is retried per its attempt count.
      log.warn(`outbox job failed, will retry`, { kind: job.kind, attempt: job.attempts + 1, reason: (err as Error).message });
      outbox.fail(job.id, (err as Error).message, job.attempts + 1);
    }
  }
  return done;
}

let cycles = 0;
function maintain(): string[] {
  const did: string[] = [];
  db.pragma("wal_checkpoint(TRUNCATE)");
  did.push("wal-checkpoint");
  db.prepare("INSERT INTO search(search) VALUES('optimize')").run();
  did.push("fts-optimize");
  // Keep the wire log bounded.
  const pruned = db
    .prepare("DELETE FROM wire WHERE id NOT IN (SELECT id FROM wire ORDER BY created_at DESC LIMIT 2000)")
    .run().changes;
  if (pruned) did.push(`pruned-wire(${pruned})`);
  if (once || cycles % 20 === 0) {
    db.exec("VACUUM");
    did.push("vacuum");
  }
  return did;
}

async function cycle(): Promise<void> {
  setHeartbeat();
  const { polled, ingested } = await pollSources();
  const drained = await drainOutbox();
  const embedded = await embeddings.backfill();
  const maintenance = maintain();
  const stats = { at: new Date().toISOString(), polled, ingested, outbox: drained, embedded, maintenance };
  setLastRun(stats);
  cycles++;
  log.info("cycle complete", {
    polled,
    ingested,
    outbox: drained,
    embedded,
    maintenance: maintenance.join(",") || "none",
  });
}

async function main(): Promise<void> {
  log.info(`starting`, { mode: once ? "once" : `loop`, tick_secs: once ? undefined : TICK_SECS });
  emit("worker.started", "worker", { once, tick: TICK_SECS });
  await cycle();
  if (once) {
    log.info("one-shot run done, exiting");
    process.exit(0);
  }
  setInterval(() => cycle().catch((e) => log.unexpected("cycle threw", e)), TICK_SECS * 1000);
}

main().catch((e) => {
  log.unexpected("worker fatal, exiting", e);
  process.exit(1);
});
