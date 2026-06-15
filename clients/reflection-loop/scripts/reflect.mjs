#!/usr/bin/env node
// hive reflection loop.
//
// Drains the conversation reflection queue for ONE identity (the HIVE_TOKEN):
//   GET  /api/conversations/pending  -> conversations with reflected_at IS NULL
//   GET  /api/conversations/{id}     -> full transcript
//   (LLM) reflect -> rolling summary + a journal narrative + proposed
//         tasks/decisions
//   POST /api/journal                -> durable memory (anchors materialize
//                                       tasks/decisions in `auto` mode)
//   POST /api/conversations/{id}/reflected -> store summary, drain the queue
//
// REFLECTION_MODE (per identity, default `suggest`):
//   off     — do nothing (exit). The identity opted out of reflection.
//   suggest — write the journal narrative + a plain "Proposed follow-ups"
//             section (tagged `suggestion`); NO anchors, so nothing is
//             auto-created. A human reviews. Summary still stored.
//   auto    — additionally anchor the tasks/decisions so hive materializes
//             them immediately.
//
// One pass by default; `--watch` polls forever with REFLECTION_INTERVAL_SECS.

import {
  hiveConfig,
  hive,
  llmAuth,
  warnIfSubscription,
  llm,
  parseJsonObject,
} from "./lib.mjs";

const MODE = (process.env.REFLECTION_MODE || "suggest").toLowerCase();
const MODEL = process.env.REFLECTION_MODEL || "claude-fable-5";
const BATCH = Number(process.env.REFLECTION_BATCH || 20);
const INTERVAL = Number(process.env.REFLECTION_INTERVAL_SECS || 300) * 1000;
const WATCH = process.argv.includes("--watch");

const SYSTEM = `You are a reflection process for an AI's long-term memory ("hive").
You receive a transcript of a past session the AI had. Produce a faithful,
concise reflection. Return ONLY a JSON object, no prose, with this shape:
{
  "summary": "1-3 sentence rolling summary of what happened (durable memory cue)",
  "narrative": "a short markdown reflection: what was discussed, decided, learned",
  "tasks": [{"title": "actionable follow-up, imperative, <100 chars"}],
  "decisions": [{"text": "a decision that was made, stated as a fact"}]
}
Only include tasks/decisions that genuinely occurred. Empty arrays are fine.
Never invent. Write in the AI's own voice (first person).`;

function transcriptToText(view) {
  const msgs = view.messages || [];
  return msgs
    .map((m) => `### ${m.role}\n${m.content}`)
    .join("\n\n")
    .slice(0, 60_000); // keep the prompt bounded
}

/**
 * Assemble the journal body and (in auto mode) the anchors. Anchor offsets are
 * UTF-16 code units — which is exactly what JS string indexing yields, so
 * indexOf/length match hive's `js_slice_utf16` semantics natively.
 */
function buildEntry(reflection, anchored) {
  const tasks = Array.isArray(reflection.tasks) ? reflection.tasks : [];
  const decisions = Array.isArray(reflection.decisions) ? reflection.decisions : [];
  let body = (reflection.narrative || "").trim();
  const anchors = [];

  const section = (heading, items, kind) => {
    if (!items.length) return;
    body += `\n\n## ${heading}\n`;
    for (const it of items) {
      const label = (it.title || it.text || "").trim();
      if (!label) continue;
      const line = `- ${label}\n`;
      const at = body.length + 2; // after "- "
      body += line;
      if (anchored) {
        anchors.push({ start: at, end: at + label.length, kind });
      }
    }
  };

  section(anchored ? "Follow-ups" : "Proposed follow-ups", tasks, "task");
  section(anchored ? "Decisions" : "Proposed decisions", decisions, "decision");

  return { body, anchors: anchored ? anchors : undefined };
}

async function reflectOne(cfg, auth, convo) {
  const view = await hive(cfg, "GET", `/api/conversations/${convo.id}`);
  if (!view || !(view.messages || []).length) {
    // Nothing to reflect on — still drain it so it doesn't loop forever.
    await hive(cfg, "POST", `/api/conversations/${convo.id}/reflected`, { summary: "" });
    return { id: convo.id, skipped: "empty" };
  }

  const out = await llm(auth, {
    model: MODEL,
    system: SYSTEM,
    user: `Transcript of session "${view.name || convo.id}":\n\n${transcriptToText(view)}`,
  });
  const reflection = parseJsonObject(out);

  const anchored = MODE === "auto";
  const { body, anchors } = buildEntry(reflection, anchored);

  if (body.trim()) {
    const tags = ["reflection", convo.app || "conversation"];
    if (!anchored) tags.push("suggestion");
    await hive(cfg, "POST", "/api/journal", { body, tags, anchors });
  }

  await hive(cfg, "POST", `/api/conversations/${convo.id}/reflected`, {
    summary: (reflection.summary || "").trim(),
  });

  return {
    id: convo.id,
    tasks: (reflection.tasks || []).length,
    decisions: (reflection.decisions || []).length,
    anchored,
  };
}

async function onePass(cfg, auth) {
  const pending = (await hive(cfg, "GET", `/api/conversations/pending?limit=${BATCH}`)) || [];
  if (!pending.length) {
    process.stderr.write("[reflect] queue empty\n");
    return 0;
  }
  process.stderr.write(`[reflect] ${pending.length} pending (mode=${MODE})\n`);
  for (const convo of pending) {
    try {
      const r = await reflectOne(cfg, auth, convo);
      process.stderr.write(`[reflect] ${convo.id} -> ${JSON.stringify(r)}\n`);
    } catch (err) {
      // Leave it pending; a later pass retries. Never crash the whole drain.
      process.stderr.write(`[reflect] ${convo.id} FAILED: ${err.message}\n`);
    }
  }
  return pending.length;
}

async function main() {
  if (MODE === "off") {
    process.stderr.write("[reflect] REFLECTION_MODE=off — nothing to do.\n");
    process.exit(0);
  }
  const cfg = hiveConfig();
  const auth = llmAuth();
  warnIfSubscription(auth);

  if (!WATCH) {
    await onePass(cfg, auth);
    return;
  }
  // --watch: drain, then poll on the interval.
  for (;;) {
    await onePass(cfg, auth);
    await new Promise((r) => setTimeout(r, INTERVAL));
  }
}

main().catch((err) => {
  process.stderr.write(`[reflect] fatal: ${err.message}\n`);
  process.exit(1);
});
