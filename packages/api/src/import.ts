// Import an older hive database (the python/Rust hive at ~/.hive/hive.db, schema
// journal_entries/tasks/projects/links) into this node hive's schema. Maps the
// source rows, rebuilds the FTS index, and lets the worker backfill embeddings.
//
//   HIVE_DB=/data/hive.db node --experimental-strip-types packages/api/src/import.ts <source.db>
//   pnpm import <source.db>
//
// Skipped: embeddings (re-derived by the worker), messages, notes, and the
// (often corrupted) wire_events table.
import Database from "better-sqlite3";
import { nanoid } from "nanoid";
import { parseMentions } from "@hive/shared";
import { logger } from "./log.ts";

const log = logger("import");
import { db, migrate } from "./db.ts";

const now = () => new Date().toISOString();
const id = (p: string) => `${p}_${nanoid(12)}`;
const slugify = (s: string) =>
  s.toLowerCase().replace(/\s+/g, "-").replace(/[^a-z0-9-]/g, "");

// Source → node value maps.
const STATUS: Record<string, string> = {
  open: "todo",
  todo: "todo",
  in_progress: "doing",
  doing: "doing",
  blocked: "blocked",
  done: "done",
  dropped: "done",
};
const PRIORITY: Record<string, string> = {
  low: "low",
  medium: "normal",
  normal: "normal",
  next: "high",
  high: "high",
  "⚡ next": "high",
  "🌱 someday": "low",
  "🔥 hot": "urgent",
};
const AI_NAMES = new Set(["pia", "apis", "cera"]);
const KIND_OF: Record<string, string> = { journal_entries: "journal", tasks: "task", projects: "project" };

function indexSearch(kind: string, refId: string, title: string, body: string, tags: string[] = []): void {
  db.prepare("DELETE FROM search WHERE kind = ? AND ref_id = ?").run(kind, refId);
  db.prepare("INSERT INTO search (kind, ref_id, title, body) VALUES (?, ?, ?, ?)").run(
    kind,
    refId,
    title,
    `${body} ${tags.join(" ")}`,
  );
}

export interface ImportCounts {
  people: number;
  projects: number;
  journal: number;
  tasks: number;
  links: number;
  skippedLinks: number;
}

export function runImport(sourcePath: string): ImportCounts {
  migrate(); // ensure the target schema exists
  const src = new Database(sourcePath, { readonly: true, fileMustExist: true });
  const counts: ImportCounts = { people: 0, projects: 0, journal: 0, tasks: 0, links: 0, skippedLinks: 0 };

  const ensurePerson = (slug: string): void => {
    if (!slug) return;
    if (db.prepare("SELECT 1 FROM people WHERE slug = ?").get(slug)) return;
    const kind = AI_NAMES.has(slug) ? "ai" : "human";
    const owner = kind === "ai" ? "nate" : null;
    db.prepare(
      "INSERT INTO people (id, slug, name, kind, owner, bio, role, created_at) VALUES (?, ?, ?, ?, ?, NULL, NULL, ?)",
    ).run(id("per"), slug, slug, kind, owner, now());
    counts.people++;
  };

  const tx = db.transaction(() => {
    // people — from journal authors and task owners
    for (const r of src.prepare("SELECT DISTINCT ai FROM journal_entries").all() as { ai: string }[]) ensurePerson(r.ai);

    // projects — keep a map from source project NAME → node project id
    const projByName = new Map<string, string>();
    const ensureProject = (name: string): string => {
      const slug = slugify(name);
      const existing = db.prepare("SELECT id FROM projects WHERE slug = ?").get(slug) as { id: string } | undefined;
      if (existing) {
        projByName.set(name, existing.id);
        return existing.id;
      }
      const pid = id("prj");
      db.prepare("INSERT INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)").run(pid, name, slug, now());
      projByName.set(name, pid);
      counts.projects++;
      return pid;
    };
    for (const p of src.prepare("SELECT name FROM projects").all() as { name: string }[]) ensureProject(p.name);

    // journal_entries → journal (title folded into the body as an H1)
    const jMap = new Map<number, string>();
    for (const e of src
      .prepare("SELECT id, ai, title, body, tags, entry_date, created_at FROM journal_entries ORDER BY id")
      .all() as Array<{ id: number; ai: string; title: string | null; body: string | null; tags: string | null; entry_date: string | null; created_at: string | null }>) {
      const jid = id("jrn");
      const tags = (e.tags ?? "").split(",").map((s) => s.trim()).filter(Boolean);
      const body = e.title ? `# ${e.title}\n\n${e.body ?? ""}` : e.body ?? "";
      db.prepare("INSERT INTO journal (id, author, body, tags, mentions, created_at) VALUES (?, ?, ?, ?, ?, ?)").run(
        jid,
        e.ai,
        body,
        JSON.stringify(tags),
        JSON.stringify(parseMentions(body)),
        e.created_at ?? e.entry_date ?? now(),
      );
      indexSearch("journal", jid, e.title ?? "", body, tags);
      jMap.set(e.id, jid);
      counts.journal++;
    }

    // tasks → tasks
    const tMap = new Map<number, string>();
    for (const t of src.prepare("SELECT * FROM tasks ORDER BY id").all() as Array<Record<string, unknown>>) {
      const tid = id("tsk");
      const status = STATUS[String(t.status)] ?? "todo";
      const priority = PRIORITY[String(t.priority)] ?? "normal";
      const project = t.project ? projByName.get(String(t.project)) ?? ensureProject(String(t.project)) : null;
      let body = (t.body as string) ?? "";
      if (t.block_reason) body += `\n\n[blocked: ${String(t.block_reason)}]`;
      const assignees = t.owner ? [String(t.owner)] : [];
      db.prepare(
        "INSERT INTO tasks (id, project, title, body, status, priority, tags, assignees, due, origin_entry_id, anchor_text, created_at, updated_at) " +
          "VALUES (?, ?, ?, ?, ?, ?, '[]', ?, ?, NULL, NULL, ?, ?)",
      ).run(
        tid,
        project,
        (t.title as string) ?? "(untitled)",
        body,
        status,
        priority,
        JSON.stringify(assignees),
        (t.due as string) ?? null,
        (t.created_at as string) ?? now(),
        (t.updated_at as string) ?? (t.created_at as string) ?? now(),
      );
      indexSearch("task", tid, (t.title as string) ?? "", body, []);
      tMap.set(t.id as number, tid);
      counts.tasks++;
    }

    // links → links (only journal/task endpoints survive; message links drop)
    const resolve = (table: string, oldId: number): string | undefined =>
      table === "journal_entries" ? jMap.get(oldId) : table === "tasks" ? tMap.get(oldId) : undefined;
    for (const l of src.prepare("SELECT * FROM links").all() as Array<Record<string, unknown>>) {
      const sk = KIND_OF[String(l.source_table)];
      const tk = KIND_OF[String(l.target_table)];
      const sid = resolve(String(l.source_table), l.source_id as number);
      const tgt = resolve(String(l.target_table), l.target_id as number);
      if (!sk || !tk || !sid || !tgt) {
        counts.skippedLinks++;
        continue;
      }
      const rel = String(l.link_type ?? "relates_to").replace(/-/g, "_");
      db.prepare(
        "INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
      ).run(id("lnk"), sk, sid, tk, tgt, rel, (l.created_at as string) ?? now());
      counts.links++;
    }
  });
  tx();
  src.close();
  return counts;
}

// CLI entry: `node ... import.ts <source.db>`
const sourceArg = process.argv[2];
if (sourceArg) {
  const c = runImport(sourceArg);
  log.info("import complete", c as unknown as Record<string, unknown>);
}
