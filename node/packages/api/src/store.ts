import { nanoid } from "nanoid";
import {
  type Anchor,
  type AnchorFields,
  type DashboardStats,
  type Decision,
  type DecisionPatch,
  type DecisionStatus,
  type EventItem,
  type InboxItem,
  type InboxReason,
  type JournalEntry,
  type JournalEntryView,
  type Link,
  type NewAnchor,
  type NewJournalEntry,
  type Note,
  type Project,
  type ResolvedAnchor,
  type SearchHit,
  type Task,
  type TaskPatch,
  type TaskStatus,
  type WireEvent,
  ACTORS,
  isAi,
  parseMentions,
  TASK_STATUSES,
  DECISION_STATUSES,
} from "@hive/shared";
import { db, tx } from "./db.ts";

const now = () => new Date().toISOString();
const id = (prefix: string) => `${prefix}_${nanoid(12)}`;
const json = <T>(s: string): T => JSON.parse(s) as T;
const snip = (s: string, n = 140) => (s.length > n ? `${s.slice(0, n)}…` : s);

// ---- search index helpers ----

function indexEntity(kind: string, refId: string, title: string, body: string, tags: string[] = []) {
  db.prepare("DELETE FROM search WHERE kind = ? AND ref_id = ?").run(kind, refId);
  db.prepare("INSERT INTO search (kind, ref_id, title, body) VALUES (?, ?, ?, ?)").run(
    kind,
    refId,
    title,
    `${body} ${tags.join(" ")}`,
  );
}

// ---- wire log ----

export function emit(kind: string, actor: string, payload: unknown): WireEvent {
  const ev: WireEvent = { id: id("wire"), kind, actor, payload, created_at: now() };
  db.prepare("INSERT INTO wire (id, kind, actor, payload, created_at) VALUES (?, ?, ?, ?, ?)").run(
    ev.id,
    ev.kind,
    ev.actor,
    JSON.stringify(ev.payload),
    ev.created_at,
  );
  return ev;
}

export function wire(limit = 100): WireEvent[] {
  return db
    .prepare("SELECT * FROM wire ORDER BY created_at DESC LIMIT ?")
    .all(limit)
    .map((r) => {
      const row = r as Omit<WireEvent, "payload"> & { payload: string };
      return { ...row, payload: json(row.payload) };
    });
}

// ---- inbox ----

export const inbox = {
  add(
    recipient: string,
    from: string,
    reason: InboxReason,
    ref_kind: InboxItem["ref_kind"],
    ref_id: string,
    entry_id: string | null,
    snippet: string,
  ): InboxItem | null {
    if (recipient === from) return null; // don't notify yourself
    const item: InboxItem = {
      id: id("inb"),
      recipient,
      from,
      reason,
      ref_kind,
      ref_id,
      entry_id,
      snippet: snip(snippet),
      created_at: now(),
      read_at: null,
    };
    db.prepare(
      `INSERT INTO inbox (id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at)
       VALUES (@id, @recipient, @from, @reason, @ref_kind, @ref_id, @entry_id, @snippet, @created_at, @read_at)`,
    ).run(item);
    emit("inbox.delivered", from, { to: recipient, reason, ref_kind, ref_id });
    return item;
  },

  list(recipient: string, unreadOnly = false): InboxItem[] {
    const sql = `SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at
                 FROM inbox WHERE recipient = ?${unreadOnly ? " AND read_at IS NULL" : ""}
                 ORDER BY created_at DESC`;
    return db.prepare(sql).all(recipient) as InboxItem[];
  },

  markRead(itemId: string): boolean {
    return db.prepare("UPDATE inbox SET read_at = ? WHERE id = ? AND read_at IS NULL").run(now(), itemId)
      .changes > 0;
  },

  markAllRead(recipient: string): number {
    return db
      .prepare("UPDATE inbox SET read_at = ? WHERE recipient = ? AND read_at IS NULL")
      .run(now(), recipient).changes;
  },

  unreadCount(recipient: string): number {
    return (
      db
        .prepare("SELECT count(*) AS n FROM inbox WHERE recipient = ? AND read_at IS NULL")
        .get(recipient) as { n: number }
    ).n;
  },
};

// ---- projects ----

export const projects = {
  list: (): Project[] => db.prepare("SELECT * FROM projects ORDER BY name").all() as Project[],
  ensure(name: string): void {
    if (db.prepare("SELECT 1 FROM projects WHERE name = ?").get(name)) return;
    db.prepare("INSERT INTO projects (id, name, created_at) VALUES (?, ?, ?)").run(
      id("proj"),
      name,
      now(),
    );
  },
};

// ---- structured entities (created internally from journal anchors) ----

type TaskRow = Omit<Task, "tags" | "assignees"> & { tags: string; assignees: string };
const toTask = (r: TaskRow): Task => ({ ...r, tags: json(r.tags), assignees: json(r.assignees) });

export const tasks = {
  list(filter: { status?: string; assignee?: string; project?: string } = {}): Task[] {
    const rows = db
      .prepare(
        "SELECT * FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
      )
      .all() as TaskRow[];
    return rows
      .map(toTask)
      .filter((t) => !filter.status || t.status === filter.status)
      .filter((t) => !filter.project || t.project === filter.project)
      .filter((t) => !filter.assignee || t.assignees.includes(filter.assignee));
  },

  get(taskId: string): Task | undefined {
    const r = db.prepare("SELECT * FROM tasks WHERE id = ?").get(taskId) as TaskRow | undefined;
    return r ? toTask(r) : undefined;
  },

  create(input: Partial<Task> & { title: string }, actor = "system"): Task {
    if (input.project) projects.ensure(input.project);
    const t: Task = {
      id: id("task"),
      title: input.title,
      body: input.body ?? "",
      status: (input.status as TaskStatus) ?? "todo",
      priority: input.priority ?? "normal",
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      project: input.project ?? null,
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
      updated_at: now(),
    };
    db.prepare(
      `INSERT INTO tasks (id, project, title, body, status, priority, tags, assignees, origin_entry_id, anchor_text, created_at, updated_at)
       VALUES (@id, @project, @title, @body, @status, @priority, @tags, @assignees, @origin_entry_id, @anchor_text, @created_at, @updated_at)`,
    ).run({ ...t, tags: JSON.stringify(t.tags), assignees: JSON.stringify(t.assignees) });
    indexEntity("task", t.id, t.title, t.body, t.tags);
    emit("task.created", actor, { id: t.id, title: t.title });
    return t;
  },

  update(taskId: string, patch: TaskPatch, actor = "system"): Task | undefined {
    const current = tasks.get(taskId);
    if (!current) return undefined;
    const next: Task = { ...current, ...patch, id: current.id, updated_at: now() };
    db.prepare(
      `UPDATE tasks SET title=@title, body=@body, status=@status, priority=@priority,
       tags=@tags, assignees=@assignees, updated_at=@updated_at WHERE id=@id`,
    ).run({ ...next, tags: JSON.stringify(next.tags), assignees: JSON.stringify(next.assignees) });
    indexEntity("task", next.id, next.title, next.body, next.tags);
    emit("task.updated", actor, { id: next.id, status: next.status });
    return next;
  },
};

type DecisionRow = Omit<Decision, "tags" | "assignees"> & { tags: string; assignees: string };
const toDecision = (r: DecisionRow): Decision => ({
  ...r,
  tags: json(r.tags),
  assignees: json(r.assignees),
});

export const decisions = {
  list(filter: { status?: string } = {}): Decision[] {
    const rows = db.prepare("SELECT * FROM decisions ORDER BY created_at DESC").all() as DecisionRow[];
    return rows.map(toDecision).filter((d) => !filter.status || d.status === filter.status);
  },

  get(decisionId: string): Decision | undefined {
    const r = db.prepare("SELECT * FROM decisions WHERE id = ?").get(decisionId) as
      | DecisionRow
      | undefined;
    return r ? toDecision(r) : undefined;
  },

  create(input: Partial<Decision> & { title: string; decision: string }, actor = "system"): Decision {
    if (input.project) projects.ensure(input.project);
    const d: Decision = {
      id: id("dec"),
      title: input.title,
      context: input.context ?? "",
      decision: input.decision,
      consequences: input.consequences ?? "",
      status: (input.status as DecisionStatus) ?? "proposed",
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      project: input.project ?? null,
      supersedes: input.supersedes ?? null,
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
      updated_at: now(),
    };
    db.prepare(
      `INSERT INTO decisions (id, title, context, decision, consequences, status, tags, assignees,
         project, supersedes, origin_entry_id, anchor_text, created_at, updated_at)
       VALUES (@id, @title, @context, @decision, @consequences, @status, @tags, @assignees,
         @project, @supersedes, @origin_entry_id, @anchor_text, @created_at, @updated_at)`,
    ).run({ ...d, tags: JSON.stringify(d.tags), assignees: JSON.stringify(d.assignees) });
    indexEntity("decision", d.id, d.title, `${d.context} ${d.decision} ${d.consequences}`, d.tags);
    if (d.supersedes) {
      const prior = decisions.get(d.supersedes);
      if (prior) {
        db.prepare("UPDATE decisions SET status='superseded', updated_at=? WHERE id=?").run(
          now(),
          prior.id,
        );
        links.create("decision", d.id, "decision", prior.id, "supersedes", actor);
      }
    }
    emit("decision.created", actor, { id: d.id, title: d.title, status: d.status });
    return d;
  },

  update(decisionId: string, patch: DecisionPatch, actor = "system"): Decision | undefined {
    const current = decisions.get(decisionId);
    if (!current) return undefined;
    const next: Decision = { ...current, ...patch, id: current.id, updated_at: now() };
    db.prepare(
      `UPDATE decisions SET title=@title, context=@context, decision=@decision, consequences=@consequences,
       status=@status, tags=@tags, assignees=@assignees, updated_at=@updated_at WHERE id=@id`,
    ).run({ ...next, tags: JSON.stringify(next.tags), assignees: JSON.stringify(next.assignees) });
    indexEntity(
      "decision",
      next.id,
      next.title,
      `${next.context} ${next.decision} ${next.consequences}`,
      next.tags,
    );
    emit("decision.updated", actor, { id: next.id, status: next.status });
    return next;
  },
};

type EventRow = Omit<EventItem, "tags" | "assignees"> & { tags: string; assignees: string };
const toEvent = (r: EventRow): EventItem => ({
  ...r,
  tags: json(r.tags),
  assignees: json(r.assignees),
});

export const events = {
  list: (): EventItem[] =>
    (db.prepare("SELECT * FROM events ORDER BY COALESCE(at, created_at) DESC").all() as EventRow[]).map(
      toEvent,
    ),

  get(eventId: string): EventItem | undefined {
    const r = db.prepare("SELECT * FROM events WHERE id = ?").get(eventId) as EventRow | undefined;
    return r ? toEvent(r) : undefined;
  },

  create(input: Partial<EventItem> & { title: string }, actor = "system"): EventItem {
    const e: EventItem = {
      id: id("evt"),
      title: input.title,
      body: input.body ?? "",
      at: input.at ?? null,
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
    };
    db.prepare(
      `INSERT INTO events (id, title, body, at, tags, assignees, origin_entry_id, anchor_text, created_at)
       VALUES (@id, @title, @body, @at, @tags, @assignees, @origin_entry_id, @anchor_text, @created_at)`,
    ).run({ ...e, tags: JSON.stringify(e.tags), assignees: JSON.stringify(e.assignees) });
    indexEntity("event", e.id, e.title, e.body, e.tags);
    emit("event.created", actor, { id: e.id, title: e.title });
    return e;
  },
};

const entityById = (kind: string, refId: string): Task | Decision | EventItem | null => {
  if (kind === "task") return tasks.get(refId) ?? null;
  if (kind === "decision") return decisions.get(refId) ?? null;
  if (kind === "event") return events.get(refId) ?? null;
  return null;
};

// ---- anchors ----

const anchorsFor = (entryId: string): ResolvedAnchor[] =>
  (db.prepare('SELECT id, entry_id, start, "end", text, kind, ref_id, created_at FROM anchors WHERE entry_id = ? ORDER BY start').all(
    entryId,
  ) as Anchor[]).map((a) => ({ ...a, entity: entityById(a.kind, a.ref_id) }));

// ---- journal (write-only source of truth) ----

export const journal = {
  list(limit = 100): JournalEntryView[] {
    const rows = db
      .prepare("SELECT * FROM journal ORDER BY created_at DESC LIMIT ?")
      .all(limit) as (Omit<JournalEntry, "tags" | "mentions"> & { tags: string; mentions: string })[];
    return rows.map((r) => ({
      ...r,
      tags: json(r.tags),
      mentions: json(r.mentions),
      anchors: anchorsFor(r.id),
    }));
  },

  get(entryId: string): JournalEntryView | undefined {
    const r = db.prepare("SELECT * FROM journal WHERE id = ?").get(entryId) as
      | (Omit<JournalEntry, "tags" | "mentions"> & { tags: string; mentions: string })
      | undefined;
    if (!r) return undefined;
    return { ...r, tags: json(r.tags), mentions: json(r.mentions), anchors: anchorsFor(r.id) };
  },

  /**
   * The one write path. Persist immutable prose, then materialise each anchored
   * span into a structured entity and fan out inbox notifications.
   */
  append(input: NewJournalEntry, actorOverride?: string): JournalEntryView {
    return tx(() => {
      const author = actorOverride ?? input.author;
      const mentions = parseMentions(input.body);
      const entry: JournalEntry = {
        id: id("jrnl"),
        author,
        body: input.body,
        tags: input.tags ?? [],
        mentions,
        created_at: now(),
      };
      db.prepare(
        "INSERT INTO journal (id, author, body, tags, mentions, created_at) VALUES (@id, @author, @body, @tags, @mentions, @created_at)",
      ).run({ ...entry, tags: JSON.stringify(entry.tags), mentions: JSON.stringify(entry.mentions) });
      indexEntity("journal", entry.id, `${author}: ${snip(input.body, 50)}`, input.body, entry.tags);

      const assignedMentions = new Set<string>();
      for (const a of input.anchors ?? []) {
        materialiseAnchor(entry, a, author, assignedMentions);
      }

      // Anyone @mentioned but not already pulled into an anchor gets a plain
      // "mention" inbox item — humans and AIs alike.
      for (const m of mentions) {
        if (!assignedMentions.has(m)) {
          inbox.add(m, author, "mention", "journal", entry.id, entry.id, input.body);
        }
      }

      emit("journal.created", author, { id: entry.id, anchors: (input.anchors ?? []).length });
      return { ...entry, anchors: anchorsFor(entry.id) };
    });
  },
};

function materialiseAnchor(
  entry: JournalEntry,
  a: NewAnchor,
  author: string,
  assignedMentions: Set<string>,
): void {
  const text = entry.body.slice(a.start, a.end).trim();
  if (!text) return;
  const f: AnchorFields = a.fields ?? {};
  const spanMentions = parseMentions(text);
  const assignees = (f.assignees ?? spanMentions).filter((x) => x !== author);
  const title = (f.title ?? text.split(/[.\n]/)[0]).slice(0, 120).trim();

  let refId: string;
  let reason: InboxReason;
  if (a.kind === "task") {
    const t = tasks.create(
      {
        title,
        body: text,
        status: (f.status as TaskStatus) ?? "todo",
        priority: f.priority,
        tags: f.tags,
        assignees,
        project: f.project ?? null,
        origin_entry_id: entry.id,
        anchor_text: text,
      },
      author,
    );
    refId = t.id;
    reason = "assignment";
  } else if (a.kind === "decision") {
    const d = decisions.create(
      {
        title,
        context: f.context ?? "",
        decision: f.decision ?? text,
        consequences: f.consequences ?? "",
        status: (f.status as DecisionStatus) ?? "proposed",
        tags: f.tags,
        assignees,
        supersedes: f.supersedes ?? null,
        project: f.project ?? null,
        origin_entry_id: entry.id,
        anchor_text: text,
      },
      author,
    );
    refId = d.id;
    reason = "decision";
  } else {
    const e = events.create(
      {
        title,
        body: text,
        at: f.at ?? null,
        tags: f.tags,
        assignees,
        origin_entry_id: entry.id,
        anchor_text: text,
      },
      author,
    );
    refId = e.id;
    reason = "event";
  }

  db.prepare(
    'INSERT INTO anchors (id, entry_id, start, "end", text, kind, ref_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
  ).run(id("anc"), entry.id, a.start, a.end, text, a.kind, refId, now());
  links.create("journal", entry.id, a.kind, refId, "anchors", author);

  for (const who of assignees) {
    assignedMentions.add(who);
    inbox.add(who, author, reason, a.kind, refId, entry.id, text);
  }
}

// ---- notes (kept for compat; no longer the prose surface) ----

type NoteRow = Omit<Note, "tags"> & { tags: string };
export const notes = {
  list: (): Note[] =>
    (db.prepare("SELECT * FROM notes ORDER BY updated_at DESC").all() as NoteRow[]).map((r) => ({
      ...r,
      tags: json(r.tags),
    })),
  create(input: { title: string; body?: string; tags?: string[] }, actor = "system"): Note {
    const n: Note = {
      id: id("note"),
      title: input.title,
      body: input.body ?? "",
      tags: input.tags ?? [],
      created_at: now(),
      updated_at: now(),
    };
    db.prepare(
      "INSERT INTO notes (id, title, body, tags, created_at, updated_at) VALUES (@id, @title, @body, @tags, @created_at, @updated_at)",
    ).run({ ...n, tags: JSON.stringify(n.tags) });
    indexEntity("note", n.id, n.title, n.body, n.tags);
    emit("note.created", actor, { id: n.id });
    return n;
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
    return l;
  },

  forEntity: (refId: string): Link[] =>
    db
      .prepare("SELECT * FROM links WHERE source_id = ? OR target_id = ? ORDER BY created_at DESC")
      .all(refId, refId) as Link[],
};

// ---- search ----

export function search(query: string, limit = 25): SearchHit[] {
  if (!query.trim()) return [];
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

function toMatchQuery(q: string): string {
  return q
    .split(/\s+/)
    .filter(Boolean)
    .map((term) => `${term.replace(/[^\p{L}\p{N}]/gu, "")}*`)
    .filter((t) => t.length > 1)
    .join(" ");
}

// ---- dashboard ----

export function dashboard(): DashboardStats {
  const count = (sql: string, ...args: unknown[]) =>
    (db.prepare(sql).get(...args) as { n: number }).n;

  const taskStats = { total: count("SELECT count(*) n FROM tasks") } as DashboardStats["tasks"];
  for (const s of TASK_STATUSES) taskStats[s] = count("SELECT count(*) n FROM tasks WHERE status=?", s);

  const decStats = { total: count("SELECT count(*) n FROM decisions") } as DashboardStats["decisions"];
  for (const s of DECISION_STATUSES)
    decStats[s] = count("SELECT count(*) n FROM decisions WHERE status=?", s);

  const byAuthor = db
    .prepare("SELECT author, count(*) AS entries FROM journal GROUP BY author ORDER BY entries DESC")
    .all() as { author: string; entries: number }[];

  const inboxStats = ACTORS.map((a) => ({
    recipient: a.name,
    kind: a.kind,
    unread: count('SELECT count(*) n FROM inbox WHERE recipient=? AND read_at IS NULL', a.name),
    total: count('SELECT count(*) n FROM inbox WHERE recipient=?', a.name),
  }));

  return {
    entries: count("SELECT count(*) n FROM journal"),
    events: count("SELECT count(*) n FROM events"),
    tasks: taskStats,
    decisions: decStats,
    inbox: inboxStats,
    byAuthor,
    recent: wire(12),
  };
}
