import { nanoid } from "nanoid";
import {
  type Anchor,
  type AnchorFields,
  type AutocompleteItem,
  type DashboardStats,
  type Decision,
  type DecisionPatch,
  type DecisionStatus,
  type EmbeddingStats,
  type EntityKind,
  type EventItem,
  type GraphData,
  type GraphNode,
  type InboxItem,
  type InboxReason,
  type JournalEntry,
  type JournalEntryView,
  type JournalRef,
  type Link,
  type NewAnchor,
  type NewJournalEntry,
  type NewSource,
  type Note,
  type OutboxJob,
  type OutboxStatus,
  type Person,
  type Phase,
  type Project,
  type ResolvedAnchor,
  type SearchHit,
  type Severity,
  type Source,
  type SourcePatch,
  type Task,
  type TaskPatch,
  type TaskStatus,
  type Topic,
  type WireEvent,
  type WorkerStatus,
  ACTORS,
  parseMentions,
  TASK_STATUSES,
  DECISION_STATUSES,
} from "@hive/shared";
import { db, tx } from "./db.ts";
import {
  contentHash,
  cosine,
  embed,
  embedQuery,
  EMBED_MODEL,
  fromBlob,
  rerank,
  RERANK_AVAILABLE,
  toBlob,
} from "./embed.ts";

const now = () => new Date().toISOString();
const id = (prefix: string) => `${prefix}_${nanoid(12)}`;
const json = <T>(s: string): T => JSON.parse(s) as T;
const snip = (s: string, n = 140) => (s.length > n ? `${s.slice(0, n)}…` : s);

/** lowercase, spaces→'-', strip non [a-z0-9-] */
const slugify = (s: string) =>
  s
    .toLowerCase()
    .replace(/\s+/g, "-")
    .replace(/[^a-z0-9-]/g, "");

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

  get(projectId: string): Project | undefined {
    return db.prepare("SELECT * FROM projects WHERE id = ?").get(projectId) as Project | undefined;
  },

  bySlug(slug: string): Project | undefined {
    return db.prepare("SELECT * FROM projects WHERE slug = ?").get(slug) as Project | undefined;
  },

  ensure(name: string): Project {
    const slug = slugify(name);
    const existing = db.prepare("SELECT * FROM projects WHERE slug = ?").get(slug) as Project | undefined;
    if (existing) return existing;
    const p: Project = { id: id("proj"), name, slug, created_at: now() };
    db.prepare("INSERT INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)").run(
      p.id, p.name, p.slug, p.created_at,
    );
    return p;
  },

  withChildren(projectId: string): Project & { tasks: Task[]; phases: Phase[] } | undefined {
    const p = projects.get(projectId);
    if (!p) return undefined;
    return {
      ...p,
      tasks: tasks.list({ project: projectId }),
      phases: phases.list(projectId),
    };
  },
};

// ---- people ----

export const people = {
  list: (): Person[] => db.prepare("SELECT * FROM people ORDER BY name").all() as Person[],

  get(personId: string): Person | undefined {
    return db.prepare("SELECT * FROM people WHERE id = ?").get(personId) as Person | undefined;
  },

  bySlug(slug: string): Person | undefined {
    return db.prepare("SELECT * FROM people WHERE slug = ?").get(slug) as Person | undefined;
  },

  ensure(name: string, kind: "human" | "ai" = "human"): Person {
    const slug = slugify(name);
    const existing = db.prepare("SELECT * FROM people WHERE slug = ?").get(slug) as Person | undefined;
    if (existing) return existing;
    const p: Person = { id: id("per"), name, slug, kind, created_at: now() };
    db.prepare("INSERT INTO people (id, name, slug, kind, created_at) VALUES (?, ?, ?, ?, ?)").run(
      p.id, p.name, p.slug, p.kind, p.created_at,
    );
    return p;
  },

  create(input: { name: string; kind?: "human" | "ai" }, actor = "system"): Person {
    const p = people.ensure(input.name, input.kind ?? "human");
    emit("person.created", actor, { id: p.id, name: p.name, kind: p.kind });
    return p;
  },

  update(personId: string, patch: { name?: string; kind?: "human" | "ai" }, actor = "system"): Person | undefined {
    const cur = people.get(personId);
    if (!cur) return undefined;
    const name = patch.name ?? cur.name;
    const kind = patch.kind ?? cur.kind;
    const slug = patch.name ? slugify(name) : cur.slug;
    db.prepare("UPDATE people SET name = ?, slug = ?, kind = ? WHERE id = ?").run(name, slug, kind, personId);
    const next: Person = { ...cur, name, slug, kind };
    emit("person.updated", actor, { id: personId, name, kind });
    return next;
  },
};

// ---- topics ----

export const topics = {
  list: (): Topic[] => db.prepare("SELECT * FROM topics ORDER BY name").all() as Topic[],

  get(topicId: string): Topic | undefined {
    return db.prepare("SELECT * FROM topics WHERE id = ?").get(topicId) as Topic | undefined;
  },

  bySlug(slug: string): Topic | undefined {
    return db.prepare("SELECT * FROM topics WHERE slug = ?").get(slug) as Topic | undefined;
  },

  ensure(name: string): Topic {
    const slug = slugify(name);
    const existing = db.prepare("SELECT * FROM topics WHERE slug = ?").get(slug) as Topic | undefined;
    if (existing) return existing;
    const t: Topic = { id: id("top"), name, slug, created_at: now() };
    db.prepare("INSERT INTO topics (id, name, slug, created_at) VALUES (?, ?, ?, ?)").run(
      t.id, t.name, t.slug, t.created_at,
    );
    return t;
  },
};

// ---- phases ----

export const phases = {
  list(projectId?: string): Phase[] {
    if (projectId) {
      return db
        .prepare("SELECT * FROM phases WHERE project = ? ORDER BY position, created_at")
        .all(projectId) as Phase[];
    }
    return db.prepare("SELECT * FROM phases ORDER BY project, position, created_at").all() as Phase[];
  },

  get(phaseId: string): Phase | undefined {
    return db.prepare("SELECT * FROM phases WHERE id = ?").get(phaseId) as Phase | undefined;
  },

  bySlug(slug: string, projectId: string): Phase | undefined {
    return db
      .prepare("SELECT * FROM phases WHERE project = ? AND name = ? COLLATE NOCASE")
      .get(projectId, slug.replace(/-/g, " ")) as Phase | undefined ??
      db.prepare("SELECT * FROM phases WHERE project = ? AND LOWER(REPLACE(name,' ','-')) = ?")
        .get(projectId, slug) as Phase | undefined;
  },

  ensure(projectId: string, name: string): Phase {
    const existing = db
      .prepare("SELECT * FROM phases WHERE project = ? AND LOWER(name) = LOWER(?)")
      .get(projectId, name) as Phase | undefined;
    if (existing) return existing;
    const pos = (
      db.prepare("SELECT COALESCE(MAX(position)+1, 0) AS n FROM phases WHERE project = ?").get(projectId) as { n: number }
    ).n;
    const ph: Phase = { id: id("ph"), project: projectId, name, position: pos, created_at: now() };
    db.prepare("INSERT INTO phases (id, project, name, position, created_at) VALUES (?, ?, ?, ?, ?)").run(
      ph.id, ph.project, ph.name, ph.position, ph.created_at,
    );
    return ph;
  },
};

// ---- structured entities (created internally from journal anchors) ----

type TaskRow = Omit<Task, "tags" | "assignees"> & { tags: string; assignees: string };
const toTask = (r: TaskRow): Task => ({ ...r, tags: json(r.tags), assignees: json(r.assignees) });

export const tasks = {
  list(filter: { status?: string; assignee?: string; project?: string; phase?: string } = {}): Task[] {
    const rows = db
      .prepare(
        "SELECT * FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
      )
      .all() as TaskRow[];
    return rows
      .map(toTask)
      .filter((t) => !filter.status || t.status === filter.status)
      .filter((t) => !filter.project || t.project === filter.project)
      .filter((t) => !filter.phase || t.phase === filter.phase)
      .filter((t) => !filter.assignee || t.assignees.includes(filter.assignee));
  },

  get(taskId: string): Task | undefined {
    const r = db.prepare("SELECT * FROM tasks WHERE id = ?").get(taskId) as TaskRow | undefined;
    return r ? toTask(r) : undefined;
  },

  create(input: Partial<Task> & { title: string }, actor = "system"): Task {
    // Only ensure-by-name when the project value is not already a known project id.
    if (input.project && !projects.get(input.project)) projects.ensure(input.project);
    const t: Task = {
      id: id("task"),
      title: input.title,
      body: input.body ?? "",
      status: (input.status as TaskStatus) ?? "todo",
      priority: input.priority ?? "normal",
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      project: input.project ?? null,
      phase: input.phase ?? null,
      due: input.due ?? null,
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
      updated_at: now(),
    };
    db.prepare(
      `INSERT INTO tasks (id, project, phase, due, title, body, status, priority, tags, assignees, origin_entry_id, anchor_text, created_at, updated_at)
       VALUES (@id, @project, @phase, @due, @title, @body, @status, @priority, @tags, @assignees, @origin_entry_id, @anchor_text, @created_at, @updated_at)`,
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
    if (input.project && !projects.get(input.project)) projects.ensure(input.project);
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

/** Regex to find bracket tokens like [person: Maggie Bierly] */
const TOKEN_RE = /\[(person|topic|project|phase|task):([^\]]+)\]/g;

/** Resolve bracket tokens in a body string against the DB at read time. */
function refsFor(body: string): JournalRef[] {
  const refs: JournalRef[] = [];
  for (const m of body.matchAll(new RegExp(TOKEN_RE.source, "g"))) {
    const kind = m[1] as JournalRef["kind"];
    const rawName = m[2].trim();
    const start = m.index!;
    const end = start + m[0].length;
    let entity: { id: string; slug: string; name: string } | undefined;
    if (kind === "person") {
      entity = people.bySlug(slugify(rawName)) ?? undefined;
    } else if (kind === "topic") {
      entity = topics.bySlug(slugify(rawName)) ?? undefined;
    } else if (kind === "project") {
      entity = projects.bySlug(slugify(rawName)) ?? undefined;
    } else if (kind === "phase") {
      // phase resolution without a project context: find by name across all phases
      const ph = db
        .prepare("SELECT * FROM phases WHERE LOWER(name) = LOWER(?) LIMIT 1")
        .get(rawName) as Phase | undefined;
      if (ph) entity = { id: ph.id, slug: slugify(ph.name), name: ph.name };
    } else {
      // task — find the most recent task with matching title
      type TR = { id: string; title: string };
      const t = db
        .prepare("SELECT id, title FROM tasks WHERE LOWER(title) = LOWER(?) ORDER BY created_at DESC LIMIT 1")
        .get(rawName) as TR | undefined;
      if (t) entity = { id: t.id, slug: slugify(t.title), name: t.title };
    }
    if (entity) {
      refs.push({ kind, id: entity.id, slug: entity.slug, name: entity.name, start, end });
    }
  }
  return refs;
}

// ---- journal (write-only source of truth) ----

export const journal = {
  list(limit = 100, offset = 0): JournalEntryView[] {
    const rows = db
      .prepare("SELECT * FROM journal ORDER BY created_at DESC LIMIT ? OFFSET ?")
      .all(limit, offset) as (Omit<JournalEntry, "tags" | "mentions"> & { tags: string; mentions: string })[];
    return rows.map((r) => ({
      ...r,
      tags: json(r.tags),
      mentions: json(r.mentions),
      anchors: anchorsFor(r.id),
      refs: refsFor(r.body),
    }));
  },

  get(entryId: string): JournalEntryView | undefined {
    const r = db.prepare("SELECT * FROM journal WHERE id = ?").get(entryId) as
      | (Omit<JournalEntry, "tags" | "mentions"> & { tags: string; mentions: string })
      | undefined;
    if (!r) return undefined;
    return { ...r, tags: json(r.tags), mentions: json(r.mentions), anchors: anchorsFor(r.id), refs: refsFor(r.body) };
  },

  /**
   * The one write path. Persist immutable prose, then materialise each anchored
   * span into a structured entity and fan out inbox notifications.
   * Also parses inline [person:], [topic:], [project:], [phase:], [task:] tokens
   * to emerge/link entities and feed inboxes.
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

      // Parse bracket tokens: emerge/link entities, fan to inboxes.
      parseBracketTokens(entry, author, assignedMentions);

      // Anyone @mentioned but not already pulled into an anchor gets a plain
      // "mention" inbox item — humans and AIs alike.
      for (const m of mentions) {
        if (!assignedMentions.has(m)) {
          inbox.add(m, author, "mention", "journal", entry.id, entry.id, input.body);
        }
      }

      emit("journal.created", author, { id: entry.id, anchors: (input.anchors ?? []).length });
      return { ...entry, anchors: anchorsFor(entry.id), refs: refsFor(entry.body) };
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
  // Auto-assign to the entry author when no explicit assignees and no @mentions in the span.
  const rawAssignees = f.assignees ?? (spanMentions.length > 0 ? spanMentions : [author]);
  const assignees = rawAssignees.filter((x) => x !== author);
  const assigneesForTask = rawAssignees.length > 0 ? rawAssignees : [author];
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
        assignees: assigneesForTask,
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

  // For inbox delivery use the full assignee list (including author when auto-assigned).
  const inboxRecipients = a.kind === "task" ? assigneesForTask : assignees;
  for (const who of inboxRecipients) {
    assignedMentions.add(who);
    inbox.add(who, author, reason, a.kind, refId, entry.id, text);
  }
}

/**
 * Parse [person:], [topic:], [project:], [phase:], [task:] tokens from an entry body.
 * Find-or-create each entity, create a links row, and fan to inboxes where relevant.
 * Context tracking: if the entry mentions a [project:] and/or [phase:], any [task:]
 * that emerges is related to that project/phase.
 */
function parseBracketTokens(
  entry: JournalEntry,
  author: string,
  assignedMentions: Set<string>,
): void {
  // First pass: collect context (project + phase referenced in this entry)
  let contextProjectId: string | null = null;
  let contextPhaseId: string | null = null;

  for (const m of entry.body.matchAll(new RegExp(TOKEN_RE.source, "g"))) {
    const kind = m[1] as JournalRef["kind"];
    const rawName = m[2].trim();
    if (kind === "project") {
      const p = projects.ensure(rawName);
      contextProjectId = p.id;
    } else if (kind === "phase" && contextProjectId) {
      const ph = phases.ensure(contextProjectId, rawName);
      contextPhaseId = ph.id;
    }
  }

  // Second pass: process all tokens
  for (const m of entry.body.matchAll(new RegExp(TOKEN_RE.source, "g"))) {
    const kind = m[1] as JournalRef["kind"];
    const rawName = m[2].trim();

    if (kind === "person") {
      // Resolve against ACTORS first (known actors), then ensure as a people row.
      const slug = slugify(rawName);
      const actorMatch = ACTORS.find((a) => a.name === slug || slugify(a.name) === slug);
      const person = actorMatch
        ? people.ensure(actorMatch.name.charAt(0).toUpperCase() + actorMatch.name.slice(1), actorMatch.kind)
        : people.ensure(rawName);
      links.create("journal", entry.id, "person", person.id, "mentions", author);
      // Fan to inbox if this person is a known actor (same as @mention)
      if (actorMatch) {
        assignedMentions.add(actorMatch.name);
        inbox.add(actorMatch.name, author, "mention", "journal", entry.id, entry.id, entry.body);
      }

    } else if (kind === "topic") {
      const topic = topics.ensure(rawName);
      links.create("journal", entry.id, "topic", topic.id, "tagged", author);

    } else if (kind === "project") {
      const proj = projects.ensure(rawName);
      links.create("journal", entry.id, "project", proj.id, "about", author);

    } else if (kind === "phase") {
      const projId = contextProjectId;
      if (projId) {
        const ph = phases.ensure(projId, rawName);
        links.create("journal", entry.id, "phase", ph.id, "about", author);
      }

    } else if (kind === "task") {
      // Emerge a task anchored to this entry, auto-assigned to the author.
      const t = tasks.create(
        {
          title: rawName,
          body: "",
          assignees: [author],
          project: contextProjectId,
          phase: contextPhaseId,
          origin_entry_id: entry.id,
          anchor_text: rawName,
        },
        author,
      );
      links.create("journal", entry.id, "task", t.id, "anchors", author);
      // author is assigned; inbox.add silently skips self-notification (recipient===from)
      inbox.add(author, author, "assignment", "task", t.id, entry.id, rawName);
    }
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

/** The whole knowledge graph: every linked entity as a node, every link as an
 * edge. Node titles are resolved from the entities themselves; an endpoint with
 * no resolvable title falls back to its id.
 *
 * Derived edges (computed at query time, not stored):
 * - chain: per-author consecutive journal entry pairs (chronological within author)
 * - project→task, project→phase, phase→task from column relationships
 */
export function graph(): GraphData {
  const rows = db
    .prepare("SELECT source_kind, source_id, target_kind, target_id, rel FROM links ORDER BY created_at")
    .all() as {
    source_kind: EntityKind;
    source_id: string;
    target_kind: EntityKind;
    target_id: string;
    rel: string;
  }[];
  const titleOf = new Map(embeddableItems().map((i) => [`${i.kind}:${i.id}`, i.title]));
  for (const n of notes.list()) titleOf.set(`note:${n.id}`, n.title);
  for (const p of people.list()) titleOf.set(`person:${p.id}`, p.name);
  for (const t of topics.list()) titleOf.set(`topic:${t.id}`, t.name);
  for (const p of projects.list()) titleOf.set(`project:${p.id}`, p.name);
  for (const ph of phases.list()) titleOf.set(`phase:${ph.id}`, ph.name);

  const nodes = new Map<string, GraphNode>();
  const addNode = (kind: EntityKind, refId: string) => {
    const key = `${kind}:${refId}`;
    if (!nodes.has(key)) nodes.set(key, { id: key, kind, title: titleOf.get(key) ?? refId });
  };
  const edges: { source: string; target: string; rel: string }[] = rows.map((r) => {
    addNode(r.source_kind, r.source_id);
    addNode(r.target_kind, r.target_id);
    return { source: `${r.source_kind}:${r.source_id}`, target: `${r.target_kind}:${r.target_id}`, rel: r.rel };
  });

  // Derived: per-author journal chain edges
  const journalRows = db
    .prepare("SELECT id, author FROM journal ORDER BY author, created_at ASC")
    .all() as { id: string; author: string }[];
  let prevAuthor: string | null = null;
  let prevId: string | null = null;
  for (const jr of journalRows) {
    if (jr.author === prevAuthor && prevId) {
      addNode("journal", prevId);
      addNode("journal", jr.id);
      edges.push({ source: `journal:${prevId}`, target: `journal:${jr.id}`, rel: "chain" });
    } else if (jr.author !== prevAuthor) {
      prevAuthor = jr.author;
    }
    prevId = jr.id;
  }

  // Derived: project→task and project→phase edges from column values
  for (const t of tasks.list()) {
    if (t.project) {
      addNode("project", t.project);
      addNode("task", t.id);
      edges.push({ source: `project:${t.project}`, target: `task:${t.id}`, rel: "has_task" });
    }
    if (t.phase) {
      addNode("phase", t.phase);
      addNode("task", t.id);
      edges.push({ source: `phase:${t.phase}`, target: `task:${t.id}`, rel: "has_task" });
    }
  }
  for (const ph of phases.list()) {
    addNode("project", ph.project);
    addNode("phase", ph.id);
    edges.push({ source: `project:${ph.project}`, target: `phase:${ph.id}`, rel: "has_phase" });
  }

  return { nodes: [...nodes.values()], edges };
}

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

/** Typeahead autocomplete: matching people, open tasks, projects, topics, phases. */
export function autocomplete(q: string, kinds?: string[]): AutocompleteItem[] {
  const lower = q.toLowerCase();
  const want = kinds ?? ["person", "task", "project", "topic", "phase"];
  const results: AutocompleteItem[] = [];

  if (want.includes("person")) {
    for (const p of people.list()) {
      if (p.name.toLowerCase().includes(lower)) {
        results.push({ kind: "person", id: p.id, slug: p.slug, label: p.name });
      }
    }
  }
  if (want.includes("project")) {
    for (const p of projects.list()) {
      if (p.name.toLowerCase().includes(lower)) {
        results.push({ kind: "project", id: p.id, slug: p.slug, label: p.name });
      }
    }
  }
  if (want.includes("topic")) {
    for (const t of topics.list()) {
      if (t.name.toLowerCase().includes(lower)) {
        results.push({ kind: "topic", id: t.id, slug: t.slug, label: t.name });
      }
    }
  }
  if (want.includes("phase")) {
    for (const ph of phases.list()) {
      if (ph.name.toLowerCase().includes(lower)) {
        results.push({ kind: "phase", id: ph.id, slug: slugify(ph.name), label: ph.name });
      }
    }
  }
  if (want.includes("task")) {
    for (const t of tasks.list({ status: "todo" }).concat(tasks.list({ status: "doing" }))) {
      if (t.title.toLowerCase().includes(lower)) {
        results.push({ kind: "task", id: t.id, slug: slugify(t.title), label: t.title });
      }
    }
  }

  return results.slice(0, 8);
}

/** Ensure the 5 known actors exist as people rows. Safe to call multiple times. */
export function seedActors(): void {
  const FULL_NAMES: Record<string, string> = {
    nate: "Nate",
    maggie: "Maggie",
    pia: "Pia",
    apis: "Apis",
    cera: "Cera",
  };
  for (const a of ACTORS) {
    people.ensure(FULL_NAMES[a.name] ?? a.name, a.kind);
  }
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

/** Every item worth embedding. `text` is the clean body (for reranking +
 * display); `embedText` carries a `[kind] title` context prefix the way
 * bookstack-mcp prepends `[shelf > book > chapter > page]` before embedding. */
export function embeddableItems(): {
  kind: string;
  id: string;
  title: string;
  text: string;
  embedText: string;
  hash: string;
}[] {
  const out: { kind: string; id: string; title: string; text: string; embedText: string; hash: string }[] = [];
  const push = (kind: string, id: string, title: string, text: string) => {
    const embedText = `[${kind}] ${title}\n\n${text}`;
    out.push({ kind, id, title, text, embedText, hash: contentHash(embedText) });
  };
  for (const e of journal.list(1000)) push("journal", e.id, `${e.author}: ${e.body.slice(0, 40)}`, e.body);
  for (const t of tasks.list()) push("task", t.id, t.title, `${t.title} ${t.body}`);
  for (const d of decisions.list())
    push("decision", d.id, d.title, `${d.title} ${d.context} ${d.decision} ${d.consequences}`);
  for (const ev of events.list()) push("event", ev.id, ev.title, `${ev.title} ${ev.body}`);
  return out;
}

export const embeddings = {
  count: () => (db.prepare("SELECT count(*) n FROM embeddings").get() as { n: number }).n,

  async upsert(ref_kind: string, ref_id: string, embedText: string): Promise<boolean> {
    const hash = contentHash(embedText);
    const existing = db
      .prepare("SELECT hash, model FROM embeddings WHERE ref_kind = ? AND ref_id = ?")
      .get(ref_kind, ref_id) as { hash: string; model: string } | undefined;
    // Re-embed when the text changed OR the active model changed — so flipping
    // $HIVE_EMBED makes the next backfill recompute even unchanged rows.
    if (existing?.hash === hash && existing.model === EMBED_MODEL) return false;
    const vec = await embed(embedText);
    // Vector is stored as a packed little-endian f32 BLOB (see embed.toBlob).
    db.prepare(
      `INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(ref_kind, ref_id) DO UPDATE SET model=excluded.model, dim=excluded.dim, vec=excluded.vec, hash=excluded.hash, created_at=excluded.created_at`,
    ).run(ref_kind, ref_id, EMBED_MODEL, vec.length, toBlob(vec), hash, now());
    return true;
  },

  /** Backfill any missing/stale embeddings; returns how many were (re)computed. */
  async backfill(): Promise<number> {
    let n = 0;
    for (const it of embeddableItems()) if (await embeddings.upsert(it.kind, it.id, it.embedText)) n++;
    return n;
  },
};

/** Admin view of the embedding corpus: coverage + per-kind/per-model breakdown. */
export function embeddingStats(): EmbeddingStats {
  const items = embeddableItems();
  const stored = new Map(
    (
      db.prepare("SELECT ref_kind, ref_id, hash FROM embeddings").all() as {
        ref_kind: string;
        ref_id: string;
        hash: string;
      }[]
    ).map((r) => [`${r.ref_kind}:${r.ref_id}`, r.hash]),
  );
  let pending = 0;
  for (const it of items) if (stored.get(`${it.kind}:${it.id}`) !== it.hash) pending++;
  return {
    total: embeddings.count(),
    model: EMBED_MODEL,
    embeddable: items.length,
    pending,
    byKind: db
      .prepare("SELECT ref_kind AS kind, count(*) AS count FROM embeddings GROUP BY ref_kind ORDER BY count DESC")
      .all() as { kind: string; count: number }[],
    byModel: db
      .prepare("SELECT model, dim, count(*) AS count FROM embeddings GROUP BY model, dim ORDER BY count DESC")
      .all() as { model: string; dim: number; count: number }[],
  };
}

export interface SemanticOptions {
  limit?: number;
  /** Drop vector matches scoring below this cosine value (default 0). */
  threshold?: number;
  /** Blend FTS keyword ranks into the score (default true). */
  hybrid?: boolean;
  /** Re-order the top-N with the cross-encoder, when one is available. */
  rerank?: boolean;
}

const refKey = (kind: string, id: string) => `${kind}:${id}`;
function splitKey(k: string): [string, string] {
  const ix = k.indexOf(":");
  return [k.slice(0, ix), k.slice(ix + 1)];
}

/** Neighbors of an entity in the links graph (either direction) — the Markov
 * blanket bookstack-mcp uses to boost results whose neighbors also surfaced. */
function blanketNeighbors(kind: string, id: string): string[] {
  const rows = db
    .prepare(
      `SELECT target_kind AS k, target_id AS i FROM links WHERE source_kind = ? AND source_id = ?
       UNION
       SELECT source_kind AS k, source_id AS i FROM links WHERE target_kind = ? AND target_id = ?`,
    )
    .all(kind, id, kind, id) as { k: string; i: string }[];
  return rows.map((r) => refKey(r.k, r.i));
}

/**
 * Semantic search, mirroring bookstack-mcp's hybrid pipeline: a brute-force
 * cosine vector pass, an optional FTS keyword blend (0.7 vector / 0.2 keyword),
 * a Markov-blanket boost from the links graph, and an optional cross-encoder
 * rerank of the top-N. Falls back to top-k vector hits so a non-empty corpus
 * never returns nothing.
 */
export async function semanticSearch(query: string, opts: SemanticOptions = {}): Promise<SearchHit[]> {
  const limit = opts.limit ?? 10;
  const threshold = opts.threshold ?? 0;
  const hybrid = opts.hybrid ?? true;
  const useRerank = (opts.rerank ?? false) && RERANK_AVAILABLE;
  if (!query.trim()) return [];

  const items = embeddableItems();
  const titleOf = new Map(items.map((i) => [refKey(i.kind, i.id), i.title]));
  const textOf = new Map(items.map((i) => [refKey(i.kind, i.id), i.text]));

  // 1. Vector pass — full cosine over model-matched blobs. The model+dim filter
  // means a partial backfill (mixed models) never compares across dimensions.
  const q = await embedQuery(query);
  const rows = db
    .prepare("SELECT ref_kind, ref_id, vec FROM embeddings WHERE model = ? AND dim = ?")
    .all(EMBED_MODEL, q.length) as { ref_kind: string; ref_id: string; vec: Buffer }[];
  const scoredAll = rows
    .map((r) => ({ k: refKey(r.ref_kind, r.ref_id), score: cosine(q, fromBlob(r.vec)) }))
    .sort((a, b) => b.score - a.score);
  const passing = scoredAll.filter((h) => h.score >= threshold);
  const rawHitKeys = new Set(passing.map((h) => h.k));
  const vhits = passing.slice(0, Math.max(limit * 2, limit));

  type Score = { vector: number; keyword: number; blanket: number };
  const scores = new Map<string, Score>();
  for (const h of vhits) scores.set(h.k, { vector: h.score, keyword: 0, blanket: 0 });

  // 2. Keyword pass (FTS) — rank-based score, decaying from the top.
  if (hybrid) {
    const kw = search(query, limit * 2);
    const total = kw.length || 1;
    kw.forEach((r, i) => {
      const kk = refKey(r.kind, r.id);
      const s = scores.get(kk) ?? { vector: 0, keyword: 0, blanket: 0 };
      s.keyword = 1 - i / total;
      scores.set(kk, s);
    });
  }

  // 3. Markov-blanket boost: neighbor in the final set (+0.05, cap 0.15),
  // neighbor that had a vector hit but didn't make the cut (+0.02, cap 0.06).
  const scoredKeys = new Set(scores.keys());
  for (const [kk, s] of scores) {
    const [k, id] = splitKey(kk);
    let strong = 0;
    let weak = 0;
    for (const nk of blanketNeighbors(k, id)) {
      if (scoredKeys.has(nk)) strong++;
      else if (rawHitKeys.has(nk)) weak++;
    }
    if (strong || weak) s.blanket = Math.min(strong * 0.05, 0.15) + Math.min(weak * 0.02, 0.06);
  }

  // Drop keyword-only noise — a keyword hit with zero semantic relevance.
  if (hybrid) for (const [kk, s] of [...scores]) if (s.vector === 0 && s.keyword > 0) scores.delete(kk);

  // 4. Blended sort.
  let ranked = [...scores.entries()]
    .map(([k, s]) => ({
      k,
      score: s.keyword > 0 && s.vector > 0 ? s.vector * 0.7 + s.keyword * 0.2 + s.blanket : s.vector + s.blanket,
    }))
    .sort((a, b) => b.score - a.score)
    .slice(0, limit);

  // 5. Cross-encoder rerank of the top-N (re-orders only; candidate set stays).
  if (useRerank && ranked.length) {
    const rr = await rerank(query, ranked.map((r) => textOf.get(r.k) ?? ""));
    if (rr) ranked = ranked.map((r, i) => ({ k: r.k, score: rr[i] })).sort((a, b) => b.score - a.score);
  }

  // 6. Fallback — never return empty when vectors exist.
  if (ranked.length === 0 && scoredAll.length) {
    ranked = scoredAll.slice(0, limit).map((h) => ({ k: h.k, score: h.score }));
  }

  return ranked.map((r) => {
    const [kind, id] = splitKey(r.k);
    return {
      kind: kind as SearchHit["kind"],
      id,
      title: titleOf.get(r.k) ?? id,
      snippet: "",
      score: Math.round(r.score * 1000) / 1000,
    };
  });
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
