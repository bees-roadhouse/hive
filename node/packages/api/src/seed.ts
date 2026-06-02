// Seeds a handful of rows so a fresh `hive` has something to show. Idempotent-ish:
// it just appends, so run it once on a clean db (the SessionStart hook does this).
import { migrate } from "./db.ts";
import { decisions, journal, links, notes, tasks } from "./store.ts";

migrate();

const t1 = tasks.create(
  {
    title: "Ship the Node + Solid rewrite of hive",
    body: "Replace the rust workspace with a fun, single-binary-spirit Node app.",
    project: "hive",
    status: "doing",
    priority: "high",
    tags: ["rewrite", "node", "solid"],
  },
  "cera",
);

tasks.create(
  {
    title: "Wire up FTS5 search across tasks/notes/journal",
    project: "hive",
    status: "done",
    priority: "normal",
    tags: ["search"],
  },
  "apis",
);

tasks.create(
  {
    title: "Decide if pgvector semantic search comes back",
    body: "fastembed in rust; node could use onnxruntime or a python sidecar.",
    project: "hive",
    status: "blocked",
    priority: "low",
    tags: ["embeddings", "later"],
  },
  "pia",
);

const n1 = notes.create(
  {
    title: "Why SQLite for the fun rewrite",
    body: "Zero infra — spins up instantly in a fresh container. The rust hive uses postgres+pgvector for prod scale; this keeps it to one file.",
    tags: ["decision", "db"],
  },
  "cera",
);

journal.create(
  {
    body: "Kicked off the Node/Solid port of hive. Kept the domain (tasks, journal, notes, decisions, links, wire) faithful in spirit to the rust crates.",
    project: "hive",
    tags: ["log"],
  },
  "cera",
);

const d1 = decisions.create(
  {
    title: "Use SQLite (not Postgres) for the fun rewrite",
    context:
      "The rust hive runs Postgres + pgvector for prod scale. This port's goal is to spin up instantly in an ephemeral container with zero external services.",
    decision:
      "Persist everything in a single SQLite file via better-sqlite3, with FTS5 for search.",
    consequences:
      "No semantic/vector search until we add an embedder. Trivially portable; the whole DB is one file under data/.",
    status: "accepted",
    project: "hive",
    tags: ["db", "architecture"],
  },
  "cera",
);

// A knowledge-graph edge: the rewrite task relates to the design note + decision.
links.create("task", t1.id, "note", n1.id, "documented-by", "cera");
links.create("task", t1.id, "decision", d1.id, "decided-by", "cera");

console.log("🌱 seeded hive with starter tasks, a note, a journal entry, and a link.");
