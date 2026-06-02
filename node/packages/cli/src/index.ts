#!/usr/bin/env node
// hive — a small HTTP client over hive-api, journal-first edition.
// (The richer surface is MCP at POST /mcp; this mirrors the common reads + the
// one write path: journal append.)
import type { Decision, EventItem, InboxItem, JournalEntryView, SearchHit, Task } from "@hive/shared";

const BASE = process.env.HIVE_API_URL ?? "http://localhost:8787";
const ACTOR = process.env.HIVE_ACTOR ?? "cli";
const dim = (s: string) => `\x1b[2m${s}\x1b[0m`;

async function api(path: string, init?: RequestInit) {
  const res = await fetch(`${BASE}/api${path}`, {
    ...init,
    headers: { "content-type": "application/json", "x-hive-actor": ACTOR, ...init?.headers },
  });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}: ${await res.text()}`);
  return res.status === 204 ? null : res.json();
}

function flags(args: string[]): Record<string, string | boolean> {
  const out: Record<string, string | boolean> = {};
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (!a.startsWith("--")) continue;
    const eq = a.indexOf("=");
    if (eq !== -1) out[a.slice(2, eq)] = a.slice(eq + 1);
    else if (args[i + 1] && !args[i + 1].startsWith("--")) out[a.slice(2)] = args[++i];
    else out[a.slice(2)] = true;
  }
  return out;
}

const TASK_GLYPH: Record<string, string> = { todo: "○", doing: "◐", blocked: "✖", done: "●" };

const HELP = `hive — journal-first (fun Node/Solid rewrite)

  hive journal                          recent entries (prose + anchors)
  hive journal add <prose…> [--tags=a,b]   write an entry (@mention to notify)
  hive inbox <actor> [--all]            an actor's inbox (unread by default)
  hive tasks [--status= --assignee=]    tasks that emerged from the journal
  hive decisions [--status=]
  hive events
  hive search <query>
  hive dashboard
  hive wire

env: HIVE_API_URL (${BASE}), HIVE_ACTOR (${ACTOR})
note: tasks/decisions/events are created by anchoring spans of a journal entry,
      which the GUI or an MCP client (journal_append) does. The CLI writes prose.`;

async function main() {
  const [domain, sub, ...rest] = process.argv.slice(2);
  const f = flags(rest);
  const positional = rest.filter((a) => !a.startsWith("--"));

  switch (domain) {
    case "journal": {
      if (sub === "add") {
        const e = (await api("/journal", {
          method: "POST",
          body: JSON.stringify({
            author: ACTOR,
            body: positional.join(" "),
            tags: typeof f.tags === "string" ? f.tags.split(",") : undefined,
          }),
        })) as JournalEntryView;
        return console.log(`📓 ${dim(e.id)}  ${e.mentions.length ? "→ " + e.mentions.map((m) => "@" + m).join(" ") : ""}`);
      }
      const list = (await api("/journal?limit=20")) as JournalEntryView[];
      for (const e of list) {
        console.log(`📓 ${dim(e.created_at)} ${e.author}: ${e.body.slice(0, 90)}`);
        if (e.anchors.length) console.log(dim(`    ${e.anchors.map((a) => a.kind).join(", ")}`));
      }
      return;
    }

    case "inbox": {
      const who = sub;
      if (!who) return console.log("usage: hive inbox <actor> [--all]");
      const list = (await api(`/inbox/${who}?unread=${f.all ? 0 : 1}`)) as InboxItem[];
      if (!list.length) return console.log(dim(`📭 nothing for ${who}`));
      for (const i of list)
        console.log(`${i.read_at ? " " : "•"} [${i.reason}] from ${i.from}: ${i.snippet.slice(0, 70)} ${dim(i.id)}`);
      return;
    }

    case "tasks": {
      const q = new URLSearchParams();
      if (typeof f.status === "string") q.set("status", f.status);
      if (typeof f.assignee === "string") q.set("assignee", f.assignee);
      const list = (await api(`/tasks?${q}`)) as Task[];
      if (!list.length) return console.log(dim("no tasks"));
      for (const t of list)
        console.log(`${TASK_GLYPH[t.status] ?? "○"} [${t.priority}] ${t.title}  ${dim(t.assignees.map((a) => "@" + a).join(" "))}`);
      return;
    }

    case "decisions": {
      const q = typeof f.status === "string" ? `?status=${f.status}` : "";
      const list = (await api(`/decisions${q}`)) as Decision[];
      for (const d of list) console.log(`◆ [${d.status}] ${d.title}\n    → ${d.decision}`);
      return;
    }

    case "events": {
      const list = (await api("/events")) as EventItem[];
      for (const e of list) console.log(`◷ ${e.at ? `[${e.at}] ` : ""}${e.title}`);
      return;
    }

    case "search": {
      const query = [sub, ...positional].filter(Boolean).join(" ");
      const hits = (await api(`/search?q=${encodeURIComponent(query)}`)) as SearchHit[];
      if (!hits.length) return console.log(dim("no matches"));
      for (const h of hits) console.log(`[${h.kind}] ${h.title}  ${dim(h.snippet)}`);
      return;
    }

    case "dashboard": {
      const s = (await api("/dashboard")) as any;
      console.log(`entries ${s.entries} · tasks ${s.tasks.total} · decisions ${s.decisions.total} · events ${s.events}`);
      console.log("tasks:", s.tasks);
      console.log("inboxes:", s.inbox.map((i: any) => `${i.recipient}:${i.unread}/${i.total}`).join("  "));
      return;
    }

    case "wire": {
      const list = (await api("/wire")) as { created_at: string; actor: string; kind: string }[];
      for (const e of list) console.log(`${dim(e.created_at)} ${e.actor} → ${e.kind}`);
      return;
    }

    default:
      console.log(HELP);
  }
}

main().catch((e) => {
  console.error(`✖ ${e.message}`);
  console.error(dim(`(is hive-api running? ${BASE})`));
  process.exit(1);
});
