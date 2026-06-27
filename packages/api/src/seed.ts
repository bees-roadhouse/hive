// Seed hive the way it's meant to be used: by writing journal entries, with
// spans of the prose anchored into tasks / decisions / events. Offsets are
// computed from the text with a small helper so the entries stay readable.
import { db, migrate } from "./db.ts";
import {
  actors,
  backfillIdentityCards,
  decisions,
  deleteJournalEntry,
  embeddings,
  events,
  inbox,
  journal,
  outbox,
  people,
  profiles,
  recall,
  seedActors,
  semanticSearch,
  sessions,
  shares,
  sources,
  tasks,
  tokens,
  users,
} from "./store.ts";
import { embed, embedQuery, rerank, rerankAvailable } from "./embed.ts";
import type { AnchorKind, AnchorFields } from "@hive/shared";

migrate();
seedActors();

const BASE = process.env.HIVE_API_URL ?? "http://localhost:7878";

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

// An entry that folds a Markdown heading into the prose — recall should derive
// the journal-hit title from this `#` heading, not from a stored column.
{
  const body =
    "# Solid UI rewrite plan\n\nLaying out the milestones for the Node + Solid port with @pia. " +
    "Editor first, then the dashboard.";
  write("cera", body, [], ["rewrite"]);
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

// Backfill embeddings so the semantic path is exercised end-to-end. In CI this
// runs under HIVE_EMBED=hash (no model download); a real deploy uses the default
// local BGE provider. Either way the read-side semantic_search + recall journal
// section should be populated below.
const embedded = await embeddings.backfill();
if (embedded === 0) throw new Error("seed: embeddings.backfill embedded nothing");

// Semantic search smoke — the seeded entries talk about the Solid UI rewrite.
const hits = await semanticSearch("Solid UI rewrite", { limit: 5 });
if (hits.length === 0) throw new Error("seed: semantic_search returned no hits after backfill");

// ---- search mode (standard / precision) + blanket flag smoke ----
// CI runs the hash embedder (rerankAvailable()=false), so precision must FALL
// BACK to standard cleanly and still return hits — never error, never empty.
{
  const Q = "Solid UI rewrite";
  const std = await semanticSearch(Q, { limit: 5, mode: "standard" });
  if (std.length === 0) throw new Error("seed: standard mode returned no hits");

  const prec = await semanticSearch(Q, { limit: 5, mode: "precision" });
  if (prec.length === 0)
    throw new Error("seed: precision mode returned no hits (fallback to standard must still produce results)");

  // Under the hash provider the cross-encoder is unavailable, so precision is a
  // clean fallback to the standard blend — same hit on top, no throw.
  if (!rerankAvailable() && prec[0].id !== std[0].id)
    throw new Error("seed: precision fallback diverged from standard despite no reranker");

  // Default (no mode) === standard.
  const def = await semanticSearch(Q, { limit: 5 });
  if (def.length === 0 || def[0].id !== std[0].id)
    throw new Error("seed: default mode did not match standard");

  // Blanket flag on (default) vs off — both must return hits; toggling it must
  // not error or empty the result set.
  const blanketOn = await semanticSearch(Q, { limit: 5, blanket: true });
  const blanketOff = await semanticSearch(Q, { limit: 5, blanket: false });
  if (blanketOn.length === 0 || blanketOff.length === 0)
    throw new Error("seed: blanket flag toggling produced an empty result set");

  // The standard `rerank` flag is also a clean no-op under hash (no reranker).
  const reranked = await semanticSearch(Q, { limit: 5, mode: "standard", rerank: true });
  if (reranked.length === 0) throw new Error("seed: standard+rerank returned no hits under hash");

  console.log(
    `   · search modes ok (rerank ${rerankAvailable() ? "available" : "unavailable → precision falls back"}): ` +
      `standard ${std.length}, precision ${prec.length}, blanket on/off ${blanketOn.length}/${blanketOff.length}`,
  );
}

// ---- embedder resilience seam ----
// The deployed transformers embedder can fail to load (missing model cache /
// offline). embed/embedQuery/rerank must DEGRADE, never throw. Under hash here
// the seam is exercised directly: rerank() returns null cleanly and the embed
// functions produce vectors. (The transformers→hash fallback path itself is
// covered by a bogus-model manual check noted in the PR.)
{
  const v1 = await embed("resilience probe");
  const v2 = await embedQuery("resilience probe");
  if (v1.length === 0 || v2.length === 0) throw new Error("seed: embed/embedQuery returned empty vector");
  const rr = await rerank("q", ["a", "b"]);
  if (rerankAvailable() === false && rr !== null)
    throw new Error("seed: rerank() must return null when no reranker is available");
}

// Recall smoke — compose Pia's session-start brief focused on Nate. Exercises
// profile cards + scoped semantic retrieval + open tasks + inbox in one call.
const r = await recall({ identity: "pia", peer: "nate" });
const piaCard = r.data.profiles.find((p) => p.actor === "pia");
if (!piaCard) throw new Error("seed: recall returned no Pia profile card");
if (!piaCard.body.sections.preferences || !piaCard.body.sections.identity)
  throw new Error("seed: profile sections did not deep-merge across updates");
if (!r.brief.includes("Recall for pia")) throw new Error("seed: recall brief missing header");
// The journal section was empty before embeddings existed; with the backfill it
// must now surface scoped journal hits.
if (r.data.journal.length === 0)
  throw new Error("seed: recall journal section empty after embedding backfill");
// Journal titles are DERIVED from the body (no title column). The heading entry
// must surface its Markdown `#` heading as the hit title — never the synthetic
// "author: slice" form.
const planned = await recall({ identity: "pia", query: "Solid UI rewrite plan milestones" });
const headingHit = planned.data.journal.find((h) => h.hit.title === "Solid UI rewrite plan");
if (!headingHit) throw new Error("seed: recall did not derive the journal title from the body heading");

// ---- identity reconciliation smoke (#31 bio/role → #37 card, canonical) ----
// 1) A legacy person with bio/role only in the people columns (no card) must be
//    folded into the card by backfillIdentityCards.
people.upsert("zane", "Zane Legacy", "human");
// Set the legacy columns directly (NOT via people.update, which would mirror to
// the card) to simulate pre-reconciliation data: bio/role in the column, no card.
db.prepare("UPDATE people SET bio = ?, role = ? WHERE slug = ?").run("Built the old wire scraper.", "Infra", "zane");
const migrated = backfillIdentityCards();
if (migrated < 1) throw new Error("seed: backfillIdentityCards migrated nothing");
const zane = profiles.get("zane");
if (zane?.body.sections.bio !== "Built the old wire scraper." || zane?.body.sections.role !== "Infra")
  throw new Error("seed: people.bio/role did not fold into the profile card");
// 2) Backfill is non-destructive: a hand-edited card section is not clobbered.
profiles.update("zane", { sections: { bio: "Edited bio wins." } }, "zane");
backfillIdentityCards();
if (profiles.get("zane")?.body.sections.bio !== "Edited bio wins.")
  throw new Error("seed: backfill clobbered an existing card section");
// 3) people.update mirrors bio/role edits into the card (one source of truth).
people.update("zane", { role: "Platform" }, "zane");
if (profiles.get("zane")?.body.sections.role !== "Platform")
  throw new Error("seed: people.update did not mirror role into the card");

// ============================================================================
// Admin actor delete + merge smoke. Proves the cascade rule (an entry's
// anchored tasks/decisions/events die with it, and no ref is left dangling) and
// the merge reassignment. Uses throwaway actors so it never touches seed data.
// ============================================================================

const countWhere = (sql: string, ...args: unknown[]) =>
  (db.prepare(sql).get(...args) as { n: number }).n;

// ---- 1) delete-with-cascade ----
{
  people.upsert("ghostwriter", "Ghost Writer", "human");
  // An admin login + token so the cascade has user/session/token rows to reap.
  const gw = users.create(
    { name: "Ghost Writer", actor: "ghostwriter", email: "ghost@example.com", password: "ghostpass1", role: "member" },
    "seed",
  );
  sessions.create(gw.id); // a live session that must be reaped with the user
  tokens.create({ actor: "ghostwriter", label: "ghost token" }, "seed");

  // One rich entry: an anchored task + an anchored decision + an anchored event,
  // plus a bracket-token task — all of which must vanish with the entry.
  const body =
    "Ghost plan: We will build the ghost feature. Decided to use the ghost approach for now. Kickoff meeting next week. Also [task: ghost bracket task].";
  const span = (s: string) => ({ start: body.indexOf(s), end: body.indexOf(s) + s.length });
  const entry = journal.append({
    author: "ghostwriter",
    body,
    tags: ["ghost"],
    anchors: [
      { ...span("We will build the ghost feature"), kind: "task", fields: { title: "Build ghost feature", assignees: ["ghostwriter", "pia"] } },
      { ...span("Decided to use the ghost approach for now"), kind: "decision", fields: { title: "Use the ghost approach" } },
      { ...span("Kickoff meeting next week"), kind: "event", fields: { title: "Ghost kickoff" } },
    ],
  });
  // @mention + share so there's cross-actor cleanup to verify.
  journal.append({ author: "pia", body: "Following up with @ghostwriter on the ghost work.", tags: [] });
  shares.create({ scope: "journal", ref: "ghostwriter", viewer: "pia" });
  await embeddings.backfill(); // give the entry + entities embeddings to purge

  // Capture the ids that must disappear.
  const anchored = db.prepare("SELECT kind, ref_id FROM anchors WHERE entry_id = ?").all(entry.id) as {
    kind: string;
    ref_id: string;
  }[];
  const bracketTask = tasks.list().find((t) => t.title === "ghost bracket task");
  if (!bracketTask) throw new Error("seed: bracket-token ghost task not created");
  if (anchored.length !== 3) throw new Error(`seed: expected 3 anchored entities, got ${anchored.length}`);

  // Preview must NOT mutate but must report the journal entry + entities.
  const preview = actors.removePreview("ghostwriter");
  if (!preview.dryRun) throw new Error("seed: removePreview did not flag dryRun");
  if (preview.journal < 1 || preview.tasks < 1 || preview.decisions < 1 || preview.events < 1)
    throw new Error(`seed: preview undercounts the cascade: ${JSON.stringify(preview)}`);
  if (!people.get("ghostwriter")) throw new Error("seed: removePreview deleted the actor (should be dry run)");
  if (!journal.get(entry.id)) throw new Error("seed: removePreview deleted the entry (should be dry run)");

  // Real delete.
  const res = actors.remove("ghostwriter");
  if (res.dryRun) throw new Error("seed: remove reported dryRun");

  // The actor is gone.
  if (people.get("ghostwriter")) throw new Error("seed: people row survived delete");
  if (users.list().some((u) => u.actor === "ghostwriter")) throw new Error("seed: user survived delete");
  if (countWhere("SELECT count(*) n FROM sessions WHERE user_id = ?", gw.id) !== 0)
    throw new Error("seed: session survived delete");
  if (countWhere("SELECT count(*) n FROM api_tokens WHERE actor = ?", "ghostwriter") !== 0)
    throw new Error("seed: api_token survived delete");
  if (profiles.get("ghostwriter")) throw new Error("seed: profile survived delete");

  // The entry and every anchored entity are gone.
  if (journal.get(entry.id)) throw new Error("seed: authored entry survived delete");
  for (const a of anchored) {
    const tbl = a.kind === "task" ? "tasks" : a.kind === "decision" ? "decisions" : "events";
    if (countWhere(`SELECT count(*) n FROM ${tbl} WHERE id = ?`, a.ref_id) !== 0)
      throw new Error(`seed: anchored ${a.kind} ${a.ref_id} survived delete`);
  }
  if (tasks.get(bracketTask.id)) throw new Error("seed: bracket-token task survived delete");

  // No dangling refs anywhere.
  if (countWhere("SELECT count(*) n FROM anchors WHERE entry_id = ?", entry.id) !== 0)
    throw new Error("seed: anchors survived delete");
  if (countWhere("SELECT count(*) n FROM embeddings WHERE ref_id = ?", entry.id) !== 0)
    throw new Error("seed: entry embedding survived delete");
  for (const a of anchored) {
    if (countWhere("SELECT count(*) n FROM embeddings WHERE ref_id = ?", a.ref_id) !== 0)
      throw new Error(`seed: embedding for ${a.ref_id} survived delete`);
    if (countWhere("SELECT count(*) n FROM search WHERE ref_id = ?", a.ref_id) !== 0)
      throw new Error(`seed: search row for ${a.ref_id} survived delete`);
    if (countWhere("SELECT count(*) n FROM links WHERE source_id = ? OR target_id = ?", a.ref_id, a.ref_id) !== 0)
      throw new Error(`seed: link for ${a.ref_id} survived delete`);
  }
  if (countWhere("SELECT count(*) n FROM search WHERE ref_id = ?", entry.id) !== 0)
    throw new Error("seed: entry search row survived delete");
  if (countWhere("SELECT count(*) n FROM links WHERE source_id = ? OR target_id = ?", entry.id, entry.id) !== 0)
    throw new Error("seed: entry links survived delete");
  if (countWhere("SELECT count(*) n FROM shares WHERE viewer = ? OR (scope='journal' AND ref = ?)", "ghostwriter", "ghostwriter") !== 0)
    throw new Error("seed: shares referencing the actor survived delete");
  if (countWhere('SELECT count(*) n FROM inbox WHERE recipient = ? OR "from" = ?', "ghostwriter", "ghostwriter") !== 0)
    throw new Error("seed: inbox referencing the actor survived delete");
  // Pia's follow-up entry @mentioned ghostwriter — the mention must be scrubbed.
  const piaFollowup = journal.list(50).find((e) => e.author === "pia" && e.body.includes("ghost work"));
  if (piaFollowup && piaFollowup.mentions.includes("ghostwriter"))
    throw new Error("seed: dangling @mention of deleted actor survived");
}

// ---- 2) merge / fold-ownership ----
{
  people.upsert("dupe-nate", "Nate (dupe)", "human");
  people.upsert("main-nate", "Nate (main)", "human");
  const dupeEntry = journal.append({
    author: "dupe-nate",
    body: "Dupe account log: shipped the merge feature. Decided merge folds ownership.",
    tags: ["merge"],
    anchors: [
      { start: 0, end: 18, kind: "task", fields: { title: "Merge feature task", assignees: ["dupe-nate"] } },
    ],
  });
  inbox.add("dupe-nate", "pia", "mention", "journal", dupeEntry.id, dupeEntry.id, "ping for dupe");
  tokens.create({ actor: "dupe-nate", label: "dupe token" }, "seed");

  const mPreview = actors.mergePreview("dupe-nate", "main-nate");
  if (!mPreview.dryRun || mPreview.journal < 1)
    throw new Error(`seed: mergePreview undercounts: ${JSON.stringify(mPreview)}`);
  if (!people.get("dupe-nate")) throw new Error("seed: mergePreview removed the from actor (should be dry run)");

  const m = actors.merge("dupe-nate", "main-nate");
  if (m.journal < 1) throw new Error("seed: merge did not reassign any journal entries");

  // `from` is gone; its data now belongs to `to`.
  if (people.get("dupe-nate")) throw new Error("seed: from actor survived merge");
  if (countWhere("SELECT count(*) n FROM journal WHERE author = ?", "dupe-nate") !== 0)
    throw new Error("seed: journal still authored by merged-away actor");
  if (journal.get(dupeEntry.id)?.author !== "main-nate")
    throw new Error("seed: entry was not reassigned to the merge target");
  // The anchored task's assignee moved to the target.
  const movedTask = tasks.list().find((t) => t.title === "Merge feature task");
  if (!movedTask) throw new Error("seed: merged task missing");
  if (!movedTask.assignees.includes("main-nate") || movedTask.assignees.includes("dupe-nate"))
    throw new Error(`seed: task assignee not folded into target: ${JSON.stringify(movedTask.assignees)}`);
  if (countWhere("SELECT count(*) n FROM inbox WHERE recipient = ?", "dupe-nate") !== 0)
    throw new Error("seed: inbox still addressed to merged-away actor");
  if (countWhere("SELECT count(*) n FROM api_tokens WHERE actor = ?", "dupe-nate") !== 0)
    throw new Error("seed: token still owned by merged-away actor");
  if (countWhere("SELECT count(*) n FROM api_tokens WHERE actor = ?", "main-nate") < 1)
    throw new Error("seed: token was not reassigned to the merge target");
}

// ---- 3) standalone deleteJournalEntry cascade ----
{
  const body = "Standalone entry: do the standalone task now.";
  const e = journal.append({
    author: "pia",
    body,
    tags: [],
    anchors: [{ start: body.indexOf("do the standalone task now"), end: body.length - 1, kind: "task", fields: { title: "Standalone task" } }],
  });
  const standalone = tasks.list().find((t) => t.title === "Standalone task");
  if (!standalone) throw new Error("seed: standalone task not created");
  deleteJournalEntry(e.id);
  if (journal.get(e.id)) throw new Error("seed: deleteJournalEntry left the entry");
  if (tasks.get(standalone.id)) throw new Error("seed: deleteJournalEntry left the anchored task");
}

console.log(
  `🌱 seeded hive: people, journal + anchors, inboxes, a sample RSS source, a scrape source, an outbox job, ` +
    `bracket-token entries, profile cards, a recall smoke (embedded ${embedded} items, ${hits.length} semantic hits, ` +
    `${r.data.journal.length} recalled journal hits), and an admin actor delete+merge cascade smoke.`,
);
