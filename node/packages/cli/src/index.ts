#!/usr/bin/env node
// hive — a small HTTP client over hive-api, in the spirit of the rust hive-cli
// (stateless, no local db). Usage: `hive <domain> <subcommand> [flags]`.
import type { Decision, SearchHit, Task } from "@hive/shared";

const BASE = process.env.HIVE_API_URL ?? "http://localhost:8787";
const ACTOR = process.env.HIVE_ACTOR ?? "cli";

async function api(path: string, init?: RequestInit) {
  const res = await fetch(`${BASE}/api${path}`, {
    ...init,
    headers: { "content-type": "application/json", "x-hive-actor": ACTOR, ...init?.headers },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status} ${res.statusText}: ${text}`);
  }
  return res.status === 204 ? null : res.json();
}

/** Parse `--key value` / `--key=value` / `--flag` into a record. */
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

const tags = (v: unknown) => (typeof v === "string" ? v.split(",").map((s) => s.trim()) : undefined);

function printTask(t: Task) {
  const mark = { todo: "○", doing: "◐", blocked: "✖", done: "●" }[t.status] ?? "○";
  console.log(`${mark} [${t.priority}] ${t.title}  ${dim(t.id)}`);
  if (t.tags.length) console.log(`    ${dim("#" + t.tags.join(" #"))}`);
}
const dim = (s: string) => `\x1b[2m${s}\x1b[0m`;

function printDecision(d: Decision) {
  const mark =
    { proposed: "◇", accepted: "◆", rejected: "✖", superseded: "⊘" }[d.status] ?? "◇";
  console.log(`${mark} [${d.status}] ${d.title}  ${dim(d.id)}`);
  if (d.decision) console.log(`    → ${d.decision}`);
}

const HELP = `hive — fun Node/Solid rewrite

  hive tasks [--status= --project=]      list tasks
  hive tasks add <title> [--project= --priority= --tags=a,b --body=]
  hive tasks done <id>
  hive tasks block <id>
  hive notes                             list notes
  hive notes add <title> [--tags= --body=]
  hive journal [--limit=]                list journal
  hive journal add <body> [--project= --tags=]
  hive decisions [--status=]             list decisions
  hive decisions add <title> --decision= [--context= --consequences= --status= --supersedes= --tags=]
  hive decisions accept <id>
  hive search <query>
  hive wire                              tail the event log

env: HIVE_API_URL (default ${BASE}), HIVE_ACTOR (default ${ACTOR})`;

async function main() {
  const [domain, sub, ...rest] = process.argv.slice(2);
  const f = flags(rest);
  const positional = rest.filter((a) => !a.startsWith("--"));

  switch (domain) {
    case "tasks": {
      if (sub === "add") {
        const t = await api("/tasks", {
          method: "POST",
          body: JSON.stringify({
            title: positional.join(" ") || f.title,
            project: f.project,
            priority: f.priority,
            body: f.body,
            tags: tags(f.tags),
          }),
        });
        return printTask(t as Task);
      }
      if (sub === "done" || sub === "block") {
        const status = sub === "done" ? "done" : "blocked";
        const t = await api(`/tasks/${positional[0]}`, {
          method: "PATCH",
          body: JSON.stringify({ status }),
        });
        return printTask(t as Task);
      }
      const q = new URLSearchParams();
      if (typeof f.status === "string") q.set("status", f.status);
      if (typeof f.project === "string") q.set("project", f.project);
      const list = (await api(`/tasks?${q}`)) as Task[];
      if (!list.length) return console.log(dim("no tasks"));
      return list.forEach(printTask);
    }

    case "notes": {
      if (sub === "add") {
        const n = await api("/notes", {
          method: "POST",
          body: JSON.stringify({
            title: positional.join(" ") || f.title,
            body: f.body,
            tags: tags(f.tags),
          }),
        });
        return console.log(`● ${(n as any).title}  ${dim((n as any).id)}`);
      }
      const list = (await api("/notes")) as any[];
      list.forEach((n) => console.log(`● ${n.title}  ${dim(n.id)}`));
      return;
    }

    case "journal": {
      if (sub === "add") {
        const e = await api("/journal", {
          method: "POST",
          body: JSON.stringify({
            body: positional.join(" ") || f.body,
            project: f.project,
            tags: tags(f.tags),
          }),
        });
        return console.log(`📓 ${dim((e as any).id)} ${(e as any).created_at}`);
      }
      const list = (await api(`/journal?limit=${f.limit ?? 20}`)) as any[];
      list.forEach((e) => console.log(`📓 ${dim(e.created_at)} ${e.body.slice(0, 80)}`));
      return;
    }

    case "decisions": {
      if (sub === "add") {
        const d = await api("/decisions", {
          method: "POST",
          body: JSON.stringify({
            title: positional.join(" ") || f.title,
            decision: f.decision,
            context: f.context,
            consequences: f.consequences,
            status: f.status,
            supersedes: f.supersedes,
            project: f.project,
            tags: tags(f.tags),
          }),
        });
        return printDecision(d as Decision);
      }
      if (sub === "accept") {
        const d = await api(`/decisions/${positional[0]}`, {
          method: "PATCH",
          body: JSON.stringify({ status: "accepted" }),
        });
        return printDecision(d as Decision);
      }
      const q = new URLSearchParams();
      if (typeof f.status === "string") q.set("status", f.status);
      const list = (await api(`/decisions?${q}`)) as Decision[];
      if (!list.length) return console.log(dim("no decisions"));
      return list.forEach(printDecision);
    }

    case "search": {
      const query = [sub, ...positional].filter(Boolean).join(" ");
      const hits = (await api(`/search?q=${encodeURIComponent(query)}`)) as SearchHit[];
      if (!hits.length) return console.log(dim("no matches"));
      hits.forEach((h) => console.log(`[${h.kind}] ${h.title}  ${dim(h.snippet)}`));
      return;
    }

    case "wire": {
      const list = (await api("/wire")) as any[];
      list.forEach((e) => console.log(`${dim(e.created_at)} ${e.actor} → ${e.kind}`));
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
