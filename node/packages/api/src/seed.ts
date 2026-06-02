// Seed hive the way it's meant to be used: by writing journal entries, with
// spans of the prose anchored into tasks / decisions / events. Offsets are
// computed from the text with a small helper so the entries stay readable.
import { migrate } from "./db.ts";
import { journal } from "./store.ts";
import type { AnchorKind, AnchorFields } from "@hive/shared";

migrate();

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

console.log("🌱 seeded hive: journal entries with anchored tasks/decisions/events, inboxes populated.");
