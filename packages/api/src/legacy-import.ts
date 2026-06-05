// Reads a legacy hive.db (the old Python/Rust hive: independent journal/tasks/
// projects/links/messages tables) and maps it onto this instance's import payload.
// Opened READ-ONLY; each table is read defensively so a missing column or a single
// corrupt table (the old files have a known-bad wire_events page) can't abort the rest.
import Database from "better-sqlite3";
import type { LegacyImport } from "@hive/shared";

const slugify = (s: string): string =>
  s.toLowerCase().trim().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");

/** Comma/whitespace-separated legacy tag string → clean array. */
const parseTags = (raw: unknown): string[] =>
  typeof raw === "string" ? raw.split(/[,\s]+/).map((t) => t.trim()).filter(Boolean) : [];

/** Legacy table name → this instance's link entity kind. */
const KIND: Record<string, string> = {
  journal_entries: "journal",
  tasks: "task",
  projects: "project",
  notes: "note",
  messages: "message",
  wire_events: "wire",
};

function tableExists(db: Database.Database, name: string): boolean {
  return !!db.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?").get(name);
}

/** Run a reader, swallowing errors (corrupt/absent table) so the rest still imports. */
function safe<T>(label: string, fn: () => T[], warnings: string[]): T[] {
  try {
    return fn();
  } catch (e) {
    warnings.push(`${label}: ${(e as Error).message}`);
    return [];
  }
}

export interface LegacyReadResult {
  payload: LegacyImport;
  warnings: string[];
}

export function readLegacyDb(path: string): LegacyReadResult {
  const db = new Database(path, { readonly: true, fileMustExist: true });
  const warnings: string[] = [];
  try {
    type Row = Record<string, unknown>;
    const str = (v: unknown): string => (v == null ? "" : String(v));

    // --- journal_entries → journal (title folds into the markdown body) ---
    const journal = tableExists(db, "journal_entries")
      ? safe(
          "journal_entries",
          () =>
            (db.prepare("SELECT * FROM journal_entries").all() as Row[]).map((r) => {
              const title = str(r.title).trim();
              const body = str(r.body);
              return {
                id: str(r.id),
                author: slugify(str(r.ai) || "unknown"),
                body: title ? `# ${title}\n\n${body}` : body,
                tags: parseTags(r.tags),
                created_at: str(r.created_at) || str(r.entry_date) || new Date(0).toISOString(),
              };
            }),
          warnings,
        )
      : [];

    // --- messages → journal (no node equivalent; preserve as sender-authored notes) ---
    const messages = tableExists(db, "messages")
      ? safe(
          "messages",
          () =>
            (db.prepare("SELECT * FROM messages").all() as Row[]).map((r) => ({
              id: str(r.id),
              author: slugify(str(r.sender_ai) || "unknown"),
              body: `**Message → @${str(r.recipient_ai)} (${str(r.kind)})**\n\n${str(r.body)}`,
              tags: ["legacy-message"],
              created_at: str(r.sent_at) || new Date(0).toISOString(),
            })),
          warnings,
        )
      : [];

    // --- projects ---
    const projects = tableExists(db, "projects")
      ? safe(
          "projects",
          () =>
            (db.prepare("SELECT * FROM projects").all() as Row[]).map((r) => ({
              id: str(r.id),
              name: str(r.name),
              slug: slugify(str(r.name)),
              created_at: str(r.created_at) || new Date(0).toISOString(),
            })),
          warnings,
        )
      : [];

    // --- tasks (due/block_reason/closed_at have no node column → footnote into the body) ---
    const tasks = tableExists(db, "tasks")
      ? safe(
          "tasks",
          () =>
            (db.prepare("SELECT * FROM tasks").all() as Row[]).map((r) => {
              const notes: string[] = [];
              if (r.block_reason) notes.push(`blocked: ${str(r.block_reason)}`);
              if (r.closed_at) notes.push(`closed: ${str(r.closed_at)}`);
              const body = [str(r.body), notes.length ? `\n\n_${notes.join(" · ")}_` : ""].join("");
              const owner = str(r.owner).trim();
              return {
                id: str(r.id),
                project: r.project ? str(r.project) : null,
                title: str(r.title),
                body,
                status: str(r.status) || "todo",
                priority: str(r.priority) || "normal",
                tags: [],
                assignees: owner ? [slugify(owner)] : [],
                due: r.due ? str(r.due) : null,
                created_at: str(r.created_at) || new Date(0).toISOString(),
                updated_at: str(r.updated_at) || str(r.created_at) || new Date(0).toISOString(),
              };
            }),
          warnings,
        )
      : [];

    // --- links ---
    const links = tableExists(db, "links")
      ? safe(
          "links",
          () =>
            (db.prepare("SELECT * FROM links").all() as Row[]).map((r) => ({
              id: str(r.id),
              source_kind: KIND[str(r.source_table)] ?? str(r.source_table),
              source_id: str(r.source_id),
              target_kind: KIND[str(r.target_table)] ?? str(r.target_table),
              target_id: str(r.target_id),
              rel: str(r.link_type) || "relates",
              created_at: str(r.created_at) || new Date(0).toISOString(),
            })),
          warnings,
        )
      : [];

    return { payload: { journal: [...journal, ...messages], projects, tasks, links }, warnings };
  } finally {
    db.close();
  }
}
