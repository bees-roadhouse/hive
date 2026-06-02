import { nanoid } from "nanoid";
import type {
  Decision,
  DecisionPatch,
  JournalEntry,
  Link,
  NewDecision,
  NewJournalEntry,
  NewNote,
  NewTask,
  Note,
  Project,
  SearchHit,
  Task,
  TaskPatch,
  WireEvent,
} from "@hive/shared";
import { db, tx } from "./db.ts";

const now = () => new Date().toISOString();
const id = (prefix: string) => `${prefix}_${nanoid(12)}`;

// ---- row <-> domain mapping (tags & payload are JSON-encoded columns) ----

type TaskRow = Omit<Task, "tags"> & { tags: string };
const toTask = (r: TaskRow): Task => ({ ...r, tags: JSON.parse(r.tags) });

type JournalRow = Omit<JournalEntry, "tags"> & { tags: string };
const toJournal = (r: JournalRow): JournalEntry => ({ ...r, tags: JSON.parse(r.tags) });

type NoteRow = Omit<Note, "tags"> & { tags: string };
const toNote = (r: NoteRow): Note => ({ ...r, tags: JSON.parse(r.tags) });

type DecisionRow = Omit<Decision, "tags"> & { tags: string };
const toDecision = (r: DecisionRow): Decision => ({ ...r, tags: JSON.parse(r.tags) });

// ---- search index helpers ----

function indexEntity(kind: string, refId: string, title: string, body: string, tags: string[]) {
  db.prepare("DELETE FROM search WHERE kind = ? AND ref_id = ?").run(kind, refId);
  db.prepare(
    "INSERT INTO search (kind, ref_id, title, body) VALUES (?, ?, ?, ?)",
  ).run(kind, refId, title, `${body} ${tags.join(" ")}`);
}

function deindex(kind: string, refId: string) {
  db.prepare("DELETE FROM search WHERE kind = ? AND ref_id = ?").run(kind, refId);
}

// ---- wire log ----

export function emit(kind: string, actor: string, payload: unknown): WireEvent {
  const ev: WireEvent = { id: id("wire"), kind, actor, payload, created_at: now() };
  db.prepare(
    "INSERT INTO wire (id, kind, actor, payload, created_at) VALUES (?, ?, ?, ?, ?)",
  ).run(ev.id, ev.kind, ev.actor, JSON.stringify(ev.payload), ev.created_at);
  return ev;
}

export function wire(limit = 100): WireEvent[] {
  return db
    .prepare("SELECT * FROM wire ORDER BY created_at DESC LIMIT ?")
    .all(limit)
    .map((r) => {
      const row = r as Omit<WireEvent, "payload"> & { payload: string };
      return { ...row, payload: JSON.parse(row.payload) };
    });
}

// ---- projects ----

export const projects = {
  list: (): Project[] => db.prepare("SELECT * FROM projects ORDER BY name").all() as Project[],
  ensure(name: string): Project {
    const existing = db.prepare("SELECT * FROM projects WHERE name = ?").get(name) as
      | Project
      | undefined;
    if (existing) return existing;
    const p: Project = { id: id("proj"), name, created_at: now() };
    db.prepare("INSERT INTO projects (id, name, created_at) VALUES (?, ?, ?)").run(
      p.id,
      p.name,
      p.created_at,
    );
    return p;
  },
};

// ---- tasks ----

export const tasks = {
  list(filter: { status?: string; project?: string } = {}): Task[] {
    let sql = "SELECT * FROM tasks";
    const where: string[] = [];
    const args: string[] = [];
    if (filter.status) (where.push("status = ?"), args.push(filter.status));
    if (filter.project) (where.push("project = ?"), args.push(filter.project));
    if (where.length) sql += ` WHERE ${where.join(" AND ")}`;
    sql += " ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC";
    return (db.prepare(sql).all(...args) as TaskRow[]).map(toTask);
  },

  get(taskId: string): Task | undefined {
    const r = db.prepare("SELECT * FROM tasks WHERE id = ?").get(taskId) as TaskRow | undefined;
    return r ? toTask(r) : undefined;
  },

  create(input: NewTask, actor = "system"): Task {
    return tx(() => {
      if (input.project) projects.ensure(input.project);
      const t: Task = {
        id: id("task"),
        project: input.project ?? null,
        title: input.title,
        body: input.body ?? "",
        status: input.status ?? "todo",
        priority: input.priority ?? "normal",
        tags: input.tags ?? [],
        created_at: now(),
        updated_at: now(),
      };
      db.prepare(
        `INSERT INTO tasks (id, project, title, body, status, priority, tags, created_at, updated_at)
         VALUES (@id, @project, @title, @body, @status, @priority, @tags, @created_at, @updated_at)`,
      ).run({ ...t, tags: JSON.stringify(t.tags) });
      indexEntity("task", t.id, t.title, t.body, t.tags);
      emit("task.created", actor, { id: t.id, title: t.title });
      return t;
    });
  },

  update(taskId: string, patch: TaskPatch, actor = "system"): Task | undefined {
    return tx(() => {
      const current = tasks.get(taskId);
      if (!current) return undefined;
      const next: Task = { ...current, ...patch, id: current.id, updated_at: now() };
      db.prepare(
        `UPDATE tasks SET project=@project, title=@title, body=@body, status=@status,
         priority=@priority, tags=@tags, updated_at=@updated_at WHERE id=@id`,
      ).run({ ...next, tags: JSON.stringify(next.tags) });
      indexEntity("task", next.id, next.title, next.body, next.tags);
      emit("task.updated", actor, { id: next.id, status: next.status });
      return next;
    });
  },

  remove(taskId: string, actor = "system"): boolean {
    return tx(() => {
      const info = db.prepare("DELETE FROM tasks WHERE id = ?").run(taskId);
      if (info.changes === 0) return false;
      deindex("task", taskId);
      emit("task.deleted", actor, { id: taskId });
      return true;
    });
  },
};

// ---- notes ----

export const notes = {
  list: (): Note[] =>
    (db.prepare("SELECT * FROM notes ORDER BY updated_at DESC").all() as NoteRow[]).map(toNote),

  get(noteId: string): Note | undefined {
    const r = db.prepare("SELECT * FROM notes WHERE id = ?").get(noteId) as NoteRow | undefined;
    return r ? toNote(r) : undefined;
  },

  create(input: NewNote, actor = "system"): Note {
    return tx(() => {
      const n: Note = {
        id: id("note"),
        title: input.title,
        body: input.body ?? "",
        tags: input.tags ?? [],
        created_at: now(),
        updated_at: now(),
      };
      db.prepare(
        `INSERT INTO notes (id, title, body, tags, created_at, updated_at)
         VALUES (@id, @title, @body, @tags, @created_at, @updated_at)`,
      ).run({ ...n, tags: JSON.stringify(n.tags) });
      indexEntity("note", n.id, n.title, n.body, n.tags);
      emit("note.created", actor, { id: n.id, title: n.title });
      return n;
    });
  },

  remove(noteId: string, actor = "system"): boolean {
    return tx(() => {
      const info = db.prepare("DELETE FROM notes WHERE id = ?").run(noteId);
      if (info.changes === 0) return false;
      deindex("note", noteId);
      emit("note.deleted", actor, { id: noteId });
      return true;
    });
  },
};

// ---- decisions (ADR-style records) ----

export const decisions = {
  list(filter: { status?: string; project?: string } = {}): Decision[] {
    let sql = "SELECT * FROM decisions";
    const where: string[] = [];
    const args: string[] = [];
    if (filter.status) (where.push("status = ?"), args.push(filter.status));
    if (filter.project) (where.push("project = ?"), args.push(filter.project));
    if (where.length) sql += ` WHERE ${where.join(" AND ")}`;
    sql += " ORDER BY created_at DESC";
    return (db.prepare(sql).all(...args) as DecisionRow[]).map(toDecision);
  },

  get(decisionId: string): Decision | undefined {
    const r = db.prepare("SELECT * FROM decisions WHERE id = ?").get(decisionId) as
      | DecisionRow
      | undefined;
    return r ? toDecision(r) : undefined;
  },

  create(input: NewDecision, actor = "system"): Decision {
    return tx(() => {
      if (input.project) projects.ensure(input.project);
      const d: Decision = {
        id: id("dec"),
        title: input.title,
        context: input.context ?? "",
        decision: input.decision,
        consequences: input.consequences ?? "",
        status: input.status ?? "proposed",
        project: input.project ?? null,
        supersedes: input.supersedes ?? null,
        tags: input.tags ?? [],
        created_at: now(),
        updated_at: now(),
      };
      db.prepare(
        `INSERT INTO decisions (id, title, context, decision, consequences, status, project,
           supersedes, tags, created_at, updated_at)
         VALUES (@id, @title, @context, @decision, @consequences, @status, @project,
           @supersedes, @tags, @created_at, @updated_at)`,
      ).run({ ...d, tags: JSON.stringify(d.tags) });
      indexEntity("decision", d.id, d.title, `${d.context} ${d.decision} ${d.consequences}`, d.tags);
      // Accepting a decision that supersedes another retires the old one and
      // records the link, so the graph shows the lineage.
      if (d.supersedes) {
        const prior = decisions.get(d.supersedes);
        if (prior) {
          db.prepare("UPDATE decisions SET status = 'superseded', updated_at = ? WHERE id = ?").run(
            now(),
            prior.id,
          );
          links.create("decision", d.id, "decision", prior.id, "supersedes", actor);
        }
      }
      emit("decision.created", actor, { id: d.id, title: d.title, status: d.status });
      return d;
    });
  },

  update(decisionId: string, patch: DecisionPatch, actor = "system"): Decision | undefined {
    return tx(() => {
      const current = decisions.get(decisionId);
      if (!current) return undefined;
      const next: Decision = { ...current, ...patch, id: current.id, updated_at: now() };
      db.prepare(
        `UPDATE decisions SET title=@title, context=@context, decision=@decision,
           consequences=@consequences, status=@status, project=@project, supersedes=@supersedes,
           tags=@tags, updated_at=@updated_at WHERE id=@id`,
      ).run({ ...next, tags: JSON.stringify(next.tags) });
      indexEntity(
        "decision",
        next.id,
        next.title,
        `${next.context} ${next.decision} ${next.consequences}`,
        next.tags,
      );
      emit("decision.updated", actor, { id: next.id, status: next.status });
      return next;
    });
  },

  remove(decisionId: string, actor = "system"): boolean {
    return tx(() => {
      const info = db.prepare("DELETE FROM decisions WHERE id = ?").run(decisionId);
      if (info.changes === 0) return false;
      deindex("decision", decisionId);
      emit("decision.deleted", actor, { id: decisionId });
      return true;
    });
  },
};

// ---- journal ----

export const journal = {
  list: (limit = 100): JournalEntry[] =>
    (db.prepare("SELECT * FROM journal ORDER BY created_at DESC LIMIT ?").all(limit) as JournalRow[]).map(
      toJournal,
    ),

  create(input: NewJournalEntry, actor = "system"): JournalEntry {
    return tx(() => {
      if (input.project) projects.ensure(input.project);
      const e: JournalEntry = {
        id: id("jrnl"),
        project: input.project ?? null,
        body: input.body,
        tags: input.tags ?? [],
        created_at: now(),
      };
      db.prepare(
        `INSERT INTO journal (id, project, body, tags, created_at)
         VALUES (@id, @project, @body, @tags, @created_at)`,
      ).run({ ...e, tags: JSON.stringify(e.tags) });
      indexEntity("journal", e.id, e.body.slice(0, 60), e.body, e.tags);
      emit("journal.created", actor, { id: e.id });
      return e;
    });
  },
};

// ---- links (knowledge graph) ----

export const links = {
  create(
    source_kind: Link["source_kind"],
    source_id: string,
    target_kind: Link["target_kind"],
    target_id: string,
    rel = "relates",
    actor = "system",
  ): Link {
    const l: Link = {
      id: id("link"),
      source_kind,
      source_id,
      target_kind,
      target_id,
      rel,
      created_at: now(),
    };
    db.prepare(
      `INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at)
       VALUES (@id, @source_kind, @source_id, @target_kind, @target_id, @rel, @created_at)`,
    ).run(l);
    emit("link.created", actor, { id: l.id, rel });
    return l;
  },

  /** Edges touching an entity in either direction. */
  forEntity(refId: string): Link[] {
    return db
      .prepare("SELECT * FROM links WHERE source_id = ? OR target_id = ? ORDER BY created_at DESC")
      .all(refId, refId) as Link[];
  },
};

// ---- search ----

export function search(query: string, limit = 25): SearchHit[] {
  if (!query.trim()) return [];
  // bm25() returns lower = better; flip the sign so higher score = better hit.
  const rows = db
    .prepare(
      `SELECT kind, ref_id, title, snippet(search, 3, '[', ']', '…', 12) AS snip, bm25(search) AS rank
       FROM search WHERE search MATCH ? ORDER BY rank LIMIT ?`,
    )
    .all(toMatchQuery(query), limit) as {
    kind: SearchHit["kind"];
    ref_id: string;
    title: string;
    snip: string;
    rank: number;
  }[];
  return rows.map((r) => ({
    kind: r.kind,
    id: r.ref_id,
    title: r.title,
    snippet: r.snip,
    score: Math.round((1 / (1 + Math.abs(r.rank))) * 1000) / 1000,
  }));
}

/** Turn loose user input into a forgiving FTS5 prefix query. */
function toMatchQuery(q: string): string {
  return q
    .split(/\s+/)
    .filter(Boolean)
    .map((term) => `${term.replace(/[^\p{L}\p{N}]/gu, "")}*`)
    .filter((t) => t.length > 1)
    .join(" ");
}
