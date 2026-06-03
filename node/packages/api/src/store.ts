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
  type NewSource,
  type Note,
  type OutboxJob,
  type OutboxStatus,
  type Project,
  type ResolvedAnchor,
  type SearchHit,
  type Severity,
  type Source,
  type SourcePatch,
  type Task,
  type TaskPatch,
  type TaskStatus,
  type WireEvent,
  type WorkerStatus,
  ACTORS,
  parseMentions,
  TASK_STATUSES,
  DECISION_STATUSES,
} from "@hive/shared";
import { db, tx } from "./db.ts";
import { contentHash, cosine, embed, EMBED_DIM, EMBED_MODEL } from "./embed.ts";

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

// ============================================================================
// Worker surface: sources, outbox, embeddings, ingestion, status.
// ============================================================================

type SourceRow = Omit<Source, "enabled"> & { enabled: number };
const toSource = (r: SourceRow): Source => ({ ...r, enabled: !!r.enabled });

export const sources = {
  /**
   * List sources. If `owner` is provided, returns global (owner=null) + that actor's.
   * Omit to get all sources (worker uses this path).
   */
  list(owner?: string): Source[] {
    const all = (db.prepare("SELECT * FROM sources ORDER BY created_at").all() as SourceRow[]).map(toSource);
    if (owner === undefined) return all;
    return all.filter((s) => s.owner === null || s.owner === owner);
  },

  get(sourceId: string): Source | undefined {
    const r = db.prepare("SELECT * FROM sources WHERE id = ?").get(sourceId) as SourceRow | undefined;
    return r ? toSource(r) : undefined;
  },

  create(input: NewSource, actor = "system"): Source {
    const s: Source = {
      id: id("src"),
      name: input.name,
      url: input.url,
      kind: input.kind ?? "rss",
      category: input.category ?? null,
      severity: input.severity ?? "info",
      interval_secs: input.interval_secs ?? 900,
      notify: input.notify ?? null,
      enabled: input.enabled ?? true,
      owner: input.owner ?? null,
      last_polled_at: null,
      last_status: null,
      created_at: now(),
    };
    db.prepare(
      `INSERT INTO sources (id, name, url, kind, category, severity, interval_secs, notify, enabled, owner, last_polled_at, last_status, created_at)
       VALUES (@id, @name, @url, @kind, @category, @severity, @interval_secs, @notify, @enabled, @owner, @last_polled_at, @last_status, @created_at)`,
    ).run({ ...s, enabled: s.enabled ? 1 : 0 });
    emit("source.added", actor, { id: s.id, name: s.name, url: s.url });
    return s;
  },

  update(sourceId: string, patch: SourcePatch, actor = "system"): Source | undefined {
    const cur = sources.get(sourceId);
    if (!cur) return undefined;
    const next: Source = { ...cur, ...patch, id: cur.id };
    db.prepare(
      `UPDATE sources SET name=@name, url=@url, kind=@kind, category=@category, severity=@severity,
       interval_secs=@interval_secs, notify=@notify, enabled=@enabled, owner=@owner WHERE id=@id`,
    ).run({ ...next, enabled: next.enabled ? 1 : 0 });
    emit("source.updated", actor, { id: next.id });
    return next;
  },

  remove(sourceId: string, actor = "system"): boolean {
    const ok = db.prepare("DELETE FROM sources WHERE id = ?").run(sourceId).changes > 0;
    if (ok) emit("source.removed", actor, { id: sourceId });
    return ok;
  },

  /** Enabled sources whose poll interval has elapsed. */
  due(): Source[] {
    const t = Date.now();
    return sources
      .list()
      .filter((s) => s.enabled)
      .filter((s) => !s.last_polled_at || t - new Date(s.last_polled_at).getTime() >= s.interval_secs * 1000);
  },

  markPolled(sourceId: string, status: string): void {
    db.prepare("UPDATE sources SET last_polled_at = ?, last_status = ? WHERE id = ?").run(
      now(),
      status,
      sourceId,
    );
  },
};

/** Ingest fetched feed items into wire events (deduped by guid). */
export function ingest(
  source: Source,
  items: { guid: string; title: string; url?: string; body?: string }[],
): number {
  let added = 0;
  for (const it of items) {
    const dupe = db
      .prepare("SELECT 1 FROM wire WHERE kind = 'feed.item' AND payload LIKE ? LIMIT 1")
      .get(`%${JSON.stringify(it.guid).slice(1, -1)}%`);
    if (dupe) continue;
    emit("feed.item", source.name, {
      guid: it.guid,
      title: it.title,
      url: it.url ?? null,
      body: it.body ?? "",
      source: source.name,
      category: source.category,
      severity: source.severity,
    });
    if (source.notify) {
      inbox.add(source.notify, source.name, "mention", "journal", source.id, null, `${source.name}: ${it.title}`);
    }
    added++;
  }
  return added;
}

/** Ingest scraped page items into wire events (deduped by guid = resolved URL). */
export function ingestScrape(
  source: Source,
  items: { guid: string; title: string; url: string }[],
): number {
  let added = 0;
  for (const it of items) {
    const dupe = db
      .prepare("SELECT 1 FROM wire WHERE kind = 'scrape.item' AND payload LIKE ? LIMIT 1")
      .get(`%${JSON.stringify(it.guid).slice(1, -1)}%`);
    if (dupe) continue;
    emit("scrape.item", source.name, {
      guid: it.guid,
      title: it.title,
      url: it.url,
      source: source.name,
      category: source.category,
      severity: source.severity,
    });
    if (source.notify) {
      inbox.add(source.notify, source.name, "mention", "journal", source.id, null, `${source.name}: ${it.title}`);
    }
    added++;
  }
  return added;
}

// ---- outbox ----

const toJob = (r: Omit<OutboxJob, "payload"> & { payload: string }): OutboxJob => ({
  ...r,
  payload: json(r.payload),
});

export const outbox = {
  enqueue(kind: string, payload: unknown, runAfter = now(), actor = "system"): OutboxJob {
    const job: OutboxJob = {
      id: id("out"),
      kind,
      payload,
      status: "pending",
      attempts: 0,
      last_error: null,
      run_after: runAfter,
      created_at: now(),
      completed_at: null,
    };
    db.prepare(
      `INSERT INTO outbox (id, kind, payload, status, attempts, last_error, run_after, created_at, completed_at)
       VALUES (@id, @kind, @payload, @status, @attempts, @last_error, @run_after, @created_at, @completed_at)`,
    ).run({ ...job, payload: JSON.stringify(job.payload) });
    emit("outbox.enqueued", actor, { id: job.id, kind });
    return job;
  },

  list: (limit = 50): OutboxJob[] =>
    (db.prepare("SELECT * FROM outbox ORDER BY created_at DESC LIMIT ?").all(limit) as any[]).map(toJob),

  claim(limit = 10): OutboxJob[] {
    const rows = db
      .prepare("SELECT * FROM outbox WHERE status = 'pending' AND run_after <= ? ORDER BY run_after LIMIT ?")
      .all(now(), limit) as any[];
    return rows.map(toJob);
  },

  complete(jobId: string): void {
    db.prepare("UPDATE outbox SET status='done', completed_at=? WHERE id=?").run(now(), jobId);
  },

  fail(jobId: string, error: string, attempts: number): void {
    const backoffSecs = Math.min(3600, 2 ** attempts * 30);
    const runAfter = new Date(Date.now() + backoffSecs * 1000).toISOString();
    const status: OutboxStatus = attempts >= 5 ? "failed" : "pending";
    db.prepare("UPDATE outbox SET status=?, attempts=?, last_error=?, run_after=? WHERE id=?").run(
      status,
      attempts,
      error,
      runAfter,
      jobId,
    );
  },

  counts: () =>
    ["pending", "done", "failed"].reduce(
      (acc, s) => {
        acc[s as OutboxStatus] = (
          db.prepare("SELECT count(*) n FROM outbox WHERE status = ?").get(s) as { n: number }
        ).n;
        return acc;
      },
      {} as Record<OutboxStatus, number>,
    ),
};

// ---- embeddings + semantic search ----

/** Every text chunk worth embedding, with a change-detection hash. */
export function embeddableItems(): { kind: string; id: string; title: string; text: string; hash: string }[] {
  const out: { kind: string; id: string; title: string; text: string; hash: string }[] = [];
  for (const e of journal.list(1000))
    out.push({ kind: "journal", id: e.id, title: `${e.author}: ${e.body.slice(0, 40)}`, text: e.body, hash: contentHash(e.body) });
  for (const t of tasks.list()) {
    const text = `${t.title} ${t.body}`;
    out.push({ kind: "task", id: t.id, title: t.title, text, hash: contentHash(text) });
  }
  for (const d of decisions.list()) {
    const text = `${d.title} ${d.context} ${d.decision} ${d.consequences}`;
    out.push({ kind: "decision", id: d.id, title: d.title, text, hash: contentHash(text) });
  }
  for (const ev of events.list()) {
    const text = `${ev.title} ${ev.body}`;
    out.push({ kind: "event", id: ev.id, title: ev.title, text, hash: contentHash(text) });
  }
  return out;
}

export const embeddings = {
  count: () => (db.prepare("SELECT count(*) n FROM embeddings").get() as { n: number }).n,

  upsert(ref_kind: string, ref_id: string, text: string): boolean {
    const hash = contentHash(text);
    const existing = db
      .prepare("SELECT hash FROM embeddings WHERE ref_kind = ? AND ref_id = ?")
      .get(ref_kind, ref_id) as { hash: string } | undefined;
    if (existing?.hash === hash) return false; // unchanged
    const vec = embed(text);
    db.prepare(
      `INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(ref_kind, ref_id) DO UPDATE SET model=excluded.model, dim=excluded.dim, vec=excluded.vec, hash=excluded.hash, created_at=excluded.created_at`,
    ).run(ref_kind, ref_id, EMBED_MODEL, EMBED_DIM, JSON.stringify(vec), hash, now());
    return true;
  },

  /** Backfill any missing/stale embeddings; returns how many were (re)computed. */
  backfill(): number {
    let n = 0;
    for (const it of embeddableItems()) if (embeddings.upsert(it.kind, it.id, it.text)) n++;
    return n;
  },
};

/** Rank stored embeddings by cosine similarity to the query. */
export function semanticSearch(query: string, limit = 10): SearchHit[] {
  if (!query.trim()) return [];
  const q = embed(query);
  const titleOf = new Map(embeddableItems().map((i) => [`${i.kind}:${i.id}`, i.title]));
  const rows = db.prepare("SELECT ref_kind, ref_id, vec FROM embeddings").all() as {
    ref_kind: string;
    ref_id: string;
    vec: string;
  }[];
  return rows
    .map((r) => ({
      kind: r.ref_kind as SearchHit["kind"],
      id: r.ref_id,
      title: titleOf.get(`${r.ref_kind}:${r.ref_id}`) ?? r.ref_id,
      snippet: "",
      score: Math.round(cosine(q, json<number[]>(r.vec)) * 1000) / 1000,
    }))
    .filter((h) => h.score > 0.01)
    .sort((a, b) => b.score - a.score)
    .slice(0, limit);
}

// ---- worker status ----

export function setHeartbeat(): void {
  db.prepare(
    "INSERT INTO worker_status (id, heartbeat) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET heartbeat = excluded.heartbeat",
  ).run(now());
}

export function setLastRun(stats: NonNullable<WorkerStatus["last_run"]>): void {
  db.prepare(
    "INSERT INTO worker_status (id, last_run) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET last_run = excluded.last_run",
  ).run(JSON.stringify(stats));
}

export function workerStatus(): WorkerStatus {
  const row = db.prepare("SELECT heartbeat, last_run FROM worker_status WHERE id = 1").get() as
    | { heartbeat: string | null; last_run: string | null }
    | undefined;
  const all = sources.list();
  return {
    heartbeat: row?.heartbeat ?? null,
    last_run: row?.last_run ? json(row.last_run) : null,
    sources: { total: all.length, enabled: all.filter((s) => s.enabled).length },
    outbox: outbox.counts(),
    embeddings: { count: embeddings.count(), model: EMBED_MODEL },
  };
}
