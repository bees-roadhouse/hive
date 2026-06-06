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
  workerStatus,
} from "@hive/api/store";
import { db } from "@hive/api/db";

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
        console.log(`[outbox:${job.kind}]`, JSON.stringify(job.payload));
      }
      outbox.complete(job.id);
      done++;
    } catch (err) {
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
  console.log(
    `🐝 worker cycle: polled ${polled} · ingested ${ingested} · outbox ${drained} · embedded ${embedded} · ${maintenance.join(", ")}`,
  );
}

async function main(): Promise<void> {
  console.log(`🐝 hive worker starting (${once ? "once" : `loop every ${TICK_SECS}s`})`);
  emit("worker.started", "worker", { once, tick: TICK_SECS });
  await cycle();
  if (once) {
    console.log(JSON.stringify(workerStatus(), null, 2));
    process.exit(0);
  }
  setInterval(() => cycle().catch((e) => console.error("cycle error", e)), TICK_SECS * 1000);
}

main().catch((e) => {
  console.error("worker fatal", e);
  process.exit(1);
});
