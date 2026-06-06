// Seed hive the way it's meant to be used: by writing journal entries, with
// spans of the prose anchored into tasks / decisions / events. Offsets are
// computed from the text with a small helper so the entries stay readable.
import { migrate } from "./db.ts";
import { journal, outbox, profiles, recall, seedActors, sources } from "./store.ts";
import type { AnchorKind, AnchorFields } from "@hive/shared";

migrate();
seedActors();

const BASE = process.env.HIVE_API_URL ?? "http://localhost:8787";

/** Anchor the (first) occurrence of `span` within `body`. */
function at(body: string, span: string, kind: AnchorKind, fields?: AnchorFields) {
  const start = body.indexOf(span);
  if (start === -1) throw new Error(`seed: span not found: ${span}`);
  return { start, end: start + span.length, kind, fields };
}

function write(author: string, body: string, spans: ReturnType<typeof at>[], tags: string[] = []) {
  journal.append({ author, body, tags, anchors: spans });
}

{
  const body =
    "Synced with @pia on the Node + Solid rewrite of hive. We'll ship the Solid UI this week — that's the next big push for @pia. Decided to stay on SQLite for now; infra-free matters more than scale here.";
  write(
    "cera",
    body,
    [
      at(body, "We'll ship the Solid UI this week", "task", {
        title: "Ship the Solid UI",
        priority: "high",
        assignees: ["pia"],
        status: "doing",
      }),
      at(body, "Decided to stay on SQLite for now; infra-free matters more than scale here", "decision", {
        title: "Stay on SQLite for the fun rewrite",
        context: "The rust hive runs Postgres + pgvector for scale; this port optimises for zero-infra spin-up.",
        consequences: "No vector search yet; the whole DB is a single file under data/.",
        status: "accepted",
      }),
    ],
    ["rewrite", "node"],
  );
}

{
  const body =
    "Made hive MCP-first today. The HTTP MCP server at /mcp is the primary surface now — @apis should wire the agent tools to journal_append next. Demo for @nate is Thursday 3pm.";
  write(
    "cera",
    body,
    [
      at(body, "@apis should wire the agent tools to journal_append next", "task", {
        title: "Wire agent tools to journal_append over MCP",
        priority: "urgent",
        assignees: ["apis"],
      }),
      at(body, "Demo for @nate is Thursday 3pm", "event", {
        title: "Demo the journal-first hive",
        at: "Thursday 3pm",
        assignees: ["nate"],
      }),
    ],
    ["mcp"],
  );
}

{
  const body =
    "Reviewed the inbox design with @maggie. Everyone — humans and AIs — gets their own inbox; @apis and @pia each see what they're assigned. Logged the decision to make the journal strictly write-only so the prose stays the source of truth.";
  write(
    "apis",
    body,
    [
      at(body, "Logged the decision to make the journal strictly write-only so the prose stays the source of truth", "decision", {
        title: "Journal is write-only; prose is source of truth",
        context: "Structured items must always trace back to an exact span of an entry.",
        status: "accepted",
      }),
    ],
    ["inbox", "design"],
  );
}

{
  const body =
    "Quick log from @pia: started on the dashboard + reporting view so we can see tasks, decisions and events across the board and drill down. @cera to review the layout.";
  write(
    "pia",
    body,
    [
      at(body, "started on the dashboard + reporting view so we can see tasks, decisions and events across the board and drill down", "task", {
        title: "Build dashboard + reporting view",
        priority: "high",
        assignees: ["pia"],
        status: "doing",
      }),
      at(body, "@cera to review the layout", "task", {
        title: "Review dashboard layout",
        assignees: ["cera"],
      }),
    ],
    ["dashboard"],
  );
}

// Worker config: a sample RSS source (served locally by the API) the worker
// polls into wire events, a scrape source, plus a demo outbound job.
sources.create(
  {
    name: "Bee feed (sample)",
    url: `${BASE}/api/_fixtures/sample.xml`,
    kind: "rss",
    category: "deps",
    severity: "low",
    interval_secs: 300,
    notify: "apis",
    owner: null,
  },
  "cera",
);
sources.create(
  {
    name: "Bee page (sample scrape)",
    url: `${BASE}/api/_fixtures/sample.html`,
    kind: "scrape",
    category: "deps",
    severity: "low",
    interval_secs: 300,
    notify: "pia",
    owner: null,
  },
  "cera",
);
outbox.enqueue("log", { note: "hello from the seed — worker will drain this" }, undefined, "cera");

// Bracket-token demo entries — exercise the new inline emergence model.
{
  const body =
    "Synced with [person: Maggie] on [project: Roadhouse Site] — [task: ship the editor]. " +
    "The new [topic: entity-model] work lands this sprint.";
  write("nate", body, [], ["planning"]);
}

{
  const body =
    "Kicking off [project: Roadhouse Site] with a [phase: Discovery] phase. " +
    "[person: Pia] owns the initial scoping — [task: scope the discovery phase].";
  write("nate", body, [], ["roadhouse"]);
}

// Profile cards — the durable identity layer. Sections deep-merge across writes.
profiles.update(
  "nate",
  {
    display_name: "Nate Smith",
    kind: "human",
    sections: {
      identity: "CTO of DTC Inc.; principal of Bee's Roadhouse. Lives at The Roadhouse in Loganton, PA.",
      working_style: "Direct, decisive, depth over breadth. Lead with the answer; skip the preamble.",
    },
  },
  "cera",
);
profiles.update(
  "pia",
  {
    display_name: "Pia (Apiara)",
    kind: "ai",
    sections: {
      identity: "Assistant to the CTO + VP of Technology for Bee's Roadhouse.",
      relationships: "Peers with Apis (DTC). Bridges BR canon for her.",
    },
  },
  "pia",
);
// Second write proves section deep-merge (adds a key, keeps the rest).
profiles.update("pia", { sections: { preferences: "Born-green PRs; verify before trust." } }, "pia");

// Recall smoke — compose Pia's session-start brief focused on Nate. Exercises
// profile cards + scoped semantic retrieval + open tasks + inbox in one call.
const r = await recall({ identity: "pia", peer: "nate" });
const piaCard = r.data.profiles.find((p) => p.actor === "pia");
if (!piaCard) throw new Error("seed: recall returned no Pia profile card");
if (!piaCard.body.sections.preferences || !piaCard.body.sections.identity)
  throw new Error("seed: profile sections did not deep-merge across updates");
if (!r.brief.includes("Recall for pia")) throw new Error("seed: recall brief missing header");

console.log("🌱 seeded hive: people, journal + anchors, inboxes, a sample RSS source, a scrape source, an outbox job, bracket-token entries, profile cards, and a recall smoke.");
