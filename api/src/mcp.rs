// MCP tool surface — parity port of packages/api/src/mcp.ts (every tool name,
// title, description, inputSchema, and result shape as the Node SDK serves
// them; ground truth captured from @modelcontextprotocol/sdk 1.29.0), plus the
// Rust-branch identity_* tools (cross-platform identity mapping — an
// intentional addition, marked in their descriptions; not in the Node toolset).
//
// The JSON-RPC / HTTP layer lives in routes/mcp.rs; this module owns
// tools/list and tools/call dispatch. The authenticated actor pins authorship:
// journal_append writes as the token's actor, identity_update edits the
// caller's own card, admin tools gate on the actor's user role.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use hive_shared::{
    actor_names, ActorKind, NewIdentity, NewJournalEntry, NewShare, NewSource, ProfilePatch,
    Severity, ShareScope, SourcePatch, TaskPatch, TaskStatus, UserRole,
};
use serde_json::{json, Map, Value};

use crate::middleware::AuthCtx;
use crate::store::recall::RecallOptions;
use crate::store::semantic::SemanticOptions;
use crate::store::tasks::TaskFilter;
use crate::store::Store;

// ---- protocol constants (SDK 1.29.0 types.js) ----

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

pub const SERVER_NAME: &str = "hive";
pub const SERVER_VERSION: &str = "0.1.0";

/// The McpServer `instructions` string (mcp.ts buildMcpServer).
pub fn instructions() -> String {
    format!(
        "hive is journal-first. Write prose with journal_append; attach `anchors` \
         (char-offset spans of the body) to emerge tasks/decisions/events anchored \
         to the exact text. @mention actors ({}) to notify their inbox. \
         Read with the *_list / *_get / search / dashboard tools. Household record kinds beyond the built-ins are the custom entity registry: entity_types_list shows what exists, entity_create / entities_list write and read typed instances (admins define types with entity_type_create). \
         For relevance retrieval prefer semantic_search with `mode: \"precision\"` (the \
         four-stage cross-encoder cascade) — it's the recommended high-quality path; drop \
         to `mode: \"standard\"` only for a broader sweep.",
        actor_names().join(", ")
    )
}

const TASK_STATUSES: &[&str] = &["todo", "doing", "blocked", "done"];
const SEVERITIES: &[&str] = &["critical", "high", "medium", "low", "info"];

// ---- tools/list ----

/// The full tool array — verbatim what the Node SDK serializes for tools/list
/// (zod schemas → draft-07 JSON Schema), with the Rust-branch identity_* tools
/// appended at the end.
pub fn tools_list() -> &'static Value {
    static TOOLS: LazyLock<Value> = LazyLock::new(build_tools);
    &TOOLS
}

const FORBIDDEN: &str = "forbidden";

fn build_tools() -> Value {
    let actors = actor_names();
    let empty_schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {}
    });
    let anchor_schema = json!({
        "type": "object",
        "properties": {
            "start": {"type": "integer", "description": "start offset (chars) of the span in `body`"},
            "end": {"type": "integer", "description": "end offset (chars) of the span in `body`"},
            "kind": {"type": "string", "enum": ["task", "decision", "event"]},
            "fields": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "status": {"type": "string"},
                    "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent"]},
                    "assignees": {"type": "array", "items": {"type": "string"}},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "project": {"type": ["string", "null"]},
                    "context": {"type": "string"},
                    "decision": {"type": "string"},
                    "consequences": {"type": "string"},
                    "supersedes": {"type": ["string", "null"]},
                    "at": {"type": ["string", "null"]}
                },
                "additionalProperties": false
            }
        },
        "required": ["start", "end", "kind"],
        "additionalProperties": false,
        "description": "a span of `body` that becomes a structured task/decision/event"
    });

    // One json! literal per tool (a single array literal would blow the
    // default macro recursion limit, and lib.rs isn't this workstream's file).
    let mut tools: Vec<Value> = Vec::new();
    tools.push(json!(
        {
            "name": "journal_append",
            "title": "Append a journal entry",
            "description": "Write an immutable prose entry. Optionally attach anchors: each is a {start,end} char span of `body` that materialises a task/decision/event anchored to that text. @mentions notify inboxes. Inline bracket tokens also emerge entities: [person: Name], [topic: Name], [project: Name], [phase: Name], [task: Title]. A [task: Title] in the entry auto-assigns to the author. A [person: X] that matches a known actor also fans to their inbox. You write as your authenticated identity — authorship is taken from your token, not a parameter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "body": {"type": "string", "description": "the prose (Markdown supported); this is the source of truth"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "anchors": {"type": "array", "items": anchor_schema}
                },
                "required": ["body"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"journal_list",
            "title": "List journal entries",
            "description": "Recent entries (newest first) with their resolved anchors.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 200}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"journal_get",
            "title": "Get a journal entry",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"identity_update",
            "title": "Update your identity (bio/role)",
            "description": "Keep your own identity current — set your bio and/or role. Writes sections.bio / sections.role on your own profile card (your authenticated identity; you can't edit anyone else's).",
            "inputSchema": {
                "type": "object",
                "properties": {"bio": {"type": "string"}, "role": {"type": "string"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"tasks_list",
            "title": "List tasks",
            "description": "Tasks that emerged from the journal. Filter by status/assignee.",
            "inputSchema": {
                "type": "object",
                "properties": {"status": {"type": "string"}, "assignee": {"type": "string"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"task_set_status",
            "title": "Advance a task",
            "description": "Workflow update on a task (status is not journal-write).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "status": {"type": "string", "enum": TASK_STATUSES}
                },
                "required": ["id", "status"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"decisions_list",
            "title": "List decisions",
            "inputSchema": {
                "type": "object",
                "properties": {"status": {"type": "string"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"events_list",
            "title": "List events",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"inbox_list",
            "title": "List an actor's inbox",
            "description": "Unread-by-default notifications for a recipient (human or AI). Viewer-gated: your own inbox (admins: any; sessions: also AIs you own).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "recipient": {"type": "string", "enum": actors.clone()},
                    "unread_only": {"type": "boolean"}
                },
                "required": ["recipient"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"inbox_mark_read",
            "title": "Mark inbox item(s) read",
            "description": "Pass an item `id`, or a `recipient` to clear all their unread. Same viewer gate as inbox_list.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}, "recipient": {"type": "string"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"search",
            "title": "Full-text search",
            "description": "Search across journal, tasks, decisions, events.",
            "inputSchema": {
                "type": "object",
                "properties": {"q": {"type": "string"}, "limit": {"type": "integer"}},
                "required": ["q"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"mail_search",
            "title": "Search mail archive",
            "description": "Search stored mail messages. Viewer-gated to the authenticated namespace; admins see all stored mail.",
            "inputSchema": {
                "type": "object",
                "properties": {"q": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}},
                "required": ["q"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"mail_thread_get",
            "title": "Get a mail thread",
            "description": "Return stored messages for a mail thread, viewer-gated from day one.",
            "inputSchema": {
                "type": "object",
                "properties": {"thread_id": {"type": "string"}},
                "required": ["thread_id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"mail_accounts_list",
            "title": "List mail accounts",
            "description": "List stored mail accounts visible to the authenticated viewer.",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"dashboard",
            "title": "Cross-board stats",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"semantic_search",
            "title": "Semantic search",
            "description": "Semantic search across journal/tasks/decisions/events. **Default to `mode: \"precision\"`** — it runs the four-stage cascade (semantic → keyword → Markov-blanket → cross-encoder rerank) and picks the most-relevant item more accurately than the standard heuristic blend. Use `mode: \"standard\"` for a wider sweep (blanket-adjacent material); both modes return the same shape, so A/B is a single `mode` swap. `precision` is recommended for \"find the right one\"; `standard` for \"find everything relevant.\" When no cross-encoder is configured, `precision` falls back to `standard` on a widened candidate pool (it never errors). The Markov-blanket link-graph boost is on by default; pass `blanket: false` to disable it. On `standard`, pass `rerank: true` to layer the cross-encoder on top of the top-N.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "q": {"type": "string"},
                    "limit": {"type": "integer"},
                    "mode": {
                        "type": "string",
                        "enum": ["standard", "precision"],
                        "description": "ranking strategy; 'precision' (recommended) runs the 4-stage cascade, 'standard' is the wider heuristic blend"
                    },
                    "hybrid": {"type": "boolean"},
                    "rerank": {
                        "type": "boolean",
                        "description": "standard-mode only: layer the cross-encoder on the top-N (always on for precision)"
                    },
                    "blanket": {
                        "type": "boolean",
                        "description": "apply the Markov-blanket link-graph boost (default true)"
                    },
                    "threshold": {"type": "number"}
                },
                "required": ["q"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"profile_get",
            "title": "Get an actor's profile card",
            "description": "The mutable 'who they are' card for an actor (human or AI): identity, preferences, working style, relationships.",
            "inputSchema": {
                "type": "object",
                "properties": {"actor": {"type": "string"}},
                "required": ["actor"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"profile_update",
            "title": "Update an actor's profile card",
            "description": "Write durable identity facts. `sections` deep-merges into the card's sections (replace per key); pass display_name/kind to set them. This is the memory-WRITE target for durable identity (episodic facts go to journal_append).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "actor": {"type": "string"},
                    "display_name": {"type": "string"},
                    "kind": {"type": "string", "enum": ["human", "ai"]},
                    "sections": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["actor"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"recall",
            "title": "Recall memory for a session",
            "description": "One-call session-start memory: composes profile cards (identity + optional peer), open tasks, unread inbox, recent relevant journal, recent events, and touched projects into a ready-to-inject markdown brief (trimmed to `budget` tokens) plus the structured data behind it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "identity": {"type": "string", "description": "the AI/actor recalling (whose tasks/inbox to pull)"},
                    "peer": {"type": "string", "description": "optional focus actor, e.g. the human in the session"},
                    "query": {"type": "string", "description": "optional topic; defaults to recent + open threads"},
                    "budget": {"type": "integer", "description": "approx token budget for the brief"},
                    "threshold": {"type": "number", "description": "optional minimum semantic score for journal hits"}
                },
                "required": ["identity"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"sources_list",
            "title": "List ingest sources",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"sources_add",
            "title": "Add an ingest source",
            "description": "Register a feed (RSS) or page monitor (scrape) for the worker to poll into wire events. Set owner to an actor name for a personal source, or omit for global.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "url": {"type": "string", "format": "uri"},
                    "kind": {"type": "string", "enum": ["rss", "scrape"]},
                    "category": {"type": "string"},
                    "severity": {"type": "string", "enum": SEVERITIES},
                    "interval_secs": {"type": "integer", "minimum": 30},
                    "notify": {"type": "string", "enum": actors.clone()},
                    "owner": {"anyOf": [{"type": "string", "enum": actors.clone()}, {"type": "null"}]}
                },
                "required": ["name", "url"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"sources_update",
            "title": "Update an ingest source",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "enabled": {"type": "boolean"},
                    "interval_secs": {"type": "integer", "minimum": 30},
                    "severity": {"type": "string", "enum": SEVERITIES},
                    "category": {"type": "string"},
                    "notify": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"sources_remove",
            "title": "Remove an ingest source",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"outbox_list",
            "title": "List outbound jobs",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"worker_status",
            "title": "Worker heartbeat + last-run stats",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"people_list",
            "title": "List writers",
            "description": "All known writers (humans + AIs) with their ownership.",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"topics_list",
            "title": "List topics",
            "description": "Topics that have been tagged in journal entries.",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"projects_list",
            "title": "List projects",
            "description": "Projects with their tasks and phases.",
            "inputSchema": empty_schema.clone(),
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"phases_list",
            "title": "List phases",
            "description": "Phases within a project. Pass project_id to filter.",
            "inputSchema": {
                "type": "object",
                "properties": {"project_id": {"type": "string"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"share_entry",
            "title": "Share a journal entry or author's journal",
            "description": "Grant a viewer visibility into a specific entry (scope='entry', ref=entry_id) or an author's entire journal stream (scope='journal', ref=author_slug). Idempotent — safe to call multiple times.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["entry", "journal"],
                        "description": "'entry' for a single entry; 'journal' for all entries by an author"
                    },
                    "ref": {"type": "string", "description": "journal entry id (scope=entry) or author slug (scope=journal)"},
                    "viewer": {"type": "string", "enum": actors.clone(), "description": "the actor who gains visibility"}
                },
                "required": ["scope", "ref", "viewer"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"actor_delete",
            "title": "Delete an actor and cascade all their data",
            "description": "DESTRUCTIVE, admin-only. Removes the actor (people/users/sessions/tokens/profile) and cascades everything they authored: journal entries AND the tasks/decisions/events anchored to those entries, plus embeddings/search/links/inbox/shares so nothing is orphaned. Pass dry_run:true to preview per-table counts without mutating.",
            "inputSchema": {
                "type": "object",
                "properties": {"slug": {"type": "string"}, "dry_run": {"type": "boolean"}},
                "required": ["slug"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"actor_merge",
            "title": "Merge one actor into another",
            "description": "DESTRUCTIVE, admin-only. Folds `from` into `into`: reassigns journal authorship/mentions, task/decision/event assignees, inbox, shares, tokens, oauth grants, wire, sources, people.owner pointers, profile + login, then removes the `from` people row. Use to consolidate duplicate actors. Pass dry_run:true to preview counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": {"type": "string"},
                    "into": {"type": "string"},
                    "dry_run": {"type": "boolean"}
                },
                "required": ["from", "into"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    // ---- Rust-branch additions (cross-platform identity mapping) ----
    // Not in the Node toolset; marked in their descriptions so clients can
    // tell them apart from the parity surface.
    tools.push(json!(
        {
            "name": "identity_link",
            "description": "Link a platform identity to an actor (Rust-branch addition; not in the Node toolset)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "platform": {"type": "string"},
                    "platform_id": {"type": "string"},
                    "actor": {"type": "string"}
                },
                "required": ["platform", "platform_id", "actor"]
            }
        }
    ));
    tools.push(json!(
        {
            "name":"identity_resolve",
            "description": "Resolve a platform ID to an actor (Rust-branch addition; not in the Node toolset)",
            "inputSchema": {
                "type": "object",
                "properties": {"platform": {"type": "string"}, "platform_id": {"type": "string"}},
                "required": ["platform", "platform_id"]
            }
        }
    ));
    tools.push(json!(
        {
            "name":"identity_list",
            "description": "List linked identities (Rust-branch addition; not in the Node toolset)",
            "inputSchema": {
                "type": "object",
                "properties": {"actor": {"type": "string"}}
            }
        }
    ));
    tools.push(json!(
        {
            "name":"identity_unlink",
            "description": "Unlink a platform identity (Rust-branch addition; not in the Node toolset)",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }
        }
    ));
    tools.push(json!(
        {
            "name": "workspace_list",
            "description": "List hosted Claude Code workspaces (sessions) visible to you. Each is a sandboxed Claude Code session hive runs; use workspace_transcript to read one's full chat history.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 500}}
            }
        }
    ));
    tools.push(json!(
        {
            "name": "workspace_get",
            "description": "Get one hosted Claude Code workspace by id (status, owner, sandbox dir, claude_session_id).",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }
        }
    ));
    tools.push(json!(
        {
            "name": "workspace_transcript",
            "description": "Read the complete transcript (chat history) of a hosted Claude Code workspace — every message and tool call. Use this to dream over a session and append/enrich journal memory based on what happened.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "after": {"type": "integer", "minimum": 0, "description": "only messages with seq greater than this"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 5000}
                },
                "required": ["id"]
            }
        }
    ));
    tools.push(json!(
        {
            "name":"entity_types_list",
            "title": "List custom entity types",
            "description": "The user-defined entity type registry (kind-config: fields, board grouping, presentation).",
            "inputSchema": {
                "type": "object",
                "properties": {"include_archived": {"type": "boolean"}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entity_type_create",
            "title": "Define a custom entity type",
            "description": "Admin only. Creates a type with typed fields (text|number|bool|date|choice|ref). Slugs are permanent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "slug": {"type": "string", "description": "lowercase, permanent; defaults from name"},
                    "name_plural": {"type": "string"},
                    "description": {"type": "string"},
                    "icon": {"type": "string"},
                    "color": {"type": "string"},
                    "board_field": {"type": "string", "description": "choice-field slug the board groups by"},
                    "fields": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "slug": {"type": "string"},
                            "label": {"type": "string"},
                            "field_type": {"type": "string", "enum": ["text", "number", "bool", "date", "choice", "ref"]},
                            "required": {"type": "boolean"},
                            "position": {"type": "integer"},
                            "options": {"type": "array", "items": {"type": "string"}},
                            "ref_kind": {"type": "string", "description": "person|topic|project|task or a custom slug"}
                        },
                        "required": ["label", "field_type"],
                        "additionalProperties": false
                    }}
                },
                "required": ["name"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entity_type_update",
            "title": "Evolve a custom entity type",
            "description": "Admin only. Rename/describe/archive a type, add fields, relabel/reorder/archive fields. Slugs and field types never change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "type id or slug"},
                    "name": {"type": "string"},
                    "name_plural": {"type": "string"},
                    "description": {"type": "string"},
                    "icon": {"type": "string"},
                    "color": {"type": "string"},
                    "board_field": {"type": ["string", "null"]},
                    "archived": {"type": "boolean"},
                    "add_fields": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "slug": {"type": "string"},
                            "label": {"type": "string"},
                            "field_type": {"type": "string", "enum": ["text", "number", "bool", "date", "choice", "ref"]},
                            "required": {"type": "boolean"},
                            "position": {"type": "integer"},
                            "options": {"type": "array", "items": {"type": "string"}},
                            "ref_kind": {"type": "string"}
                        },
                        "required": ["label", "field_type"],
                        "additionalProperties": false
                    }},
                    "update_fields": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "slug": {"type": "string"},
                            "label": {"type": "string"},
                            "position": {"type": "integer"},
                            "required": {"type": "boolean"},
                            "options": {"type": "array", "items": {"type": "string"}},
                            "archived": {"type": "boolean"}
                        },
                        "required": ["slug"],
                        "additionalProperties": false
                    }}
                },
                "required": ["type"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entities_list",
            "title": "List custom entities",
            "description": "Instances of a custom type; equality filters on field slugs, sort by field/title/created_at.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "type slug"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500},
                    "offset": {"type": "integer", "minimum": 0},
                    "sort": {"type": "string"},
                    "dir": {"type": "string", "enum": ["asc", "desc"]},
                    "filters": {"type": "object", "description": "field slug -> required value", "additionalProperties": true}
                },
                "required": ["type"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entity_get",
            "title": "Get a custom entity",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entity_create",
            "title": "Create a custom entity",
            "description": "Fields are validated against the type's registry; scope 'me' keeps it in your namespace (default global).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "type slug"},
                    "title": {"type": "string"},
                    "fields": {"type": "object", "additionalProperties": true},
                    "scope": {"type": "string", "enum": ["global", "me"]}
                },
                "required": ["type", "title"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entity_update",
            "title": "Update a custom entity",
            "description": "Shallow-merges fields; a JSON null clears a key. Validated against the registry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "title": {"type": "string"},
                    "fields": {"type": "object", "additionalProperties": true},
                    "scope": {"type": "string", "enum": ["global", "me"]}
                },
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"entity_delete",
            "title": "Delete a custom entity",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            },
            "execution": {"taskSupport": FORBIDDEN}
        }
    ));
    tools.push(json!(
        {
            "name":"artifacts_list",
            "description": "List your Claude Code artifacts (skills, agents, slash-commands) — scoped to the authenticated identity",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ));
    tools.push(json!(
        {
            "name":"artifacts_get",
            "description": "Get one Claude Code artifact by id",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }
        }
    ));
    // ---- Rust-branch additions (conversation capture + reflection queue) ----
    // Local agent sessions captured at SessionEnd onto cc_sessions
    // (origin='captured'); reflection drains the reflected_at IS NULL queue.
    let conversation_message_schema = json!({
        "type": "object",
        "properties": {
            "role": {"type": "string", "description": "user | assistant | tool | system"},
            "kind": {"type": "string", "description": "message kind (defaults to 'text')"},
            "content": {"description": "message payload; a bare string is stored as {text}"},
            "raw": {"description": "lossless original payload (optional)"},
            "tokens_in": {"type": "integer"},
            "tokens_out": {"type": "integer"}
        },
        "required": ["role"],
        "additionalProperties": false
    });
    tools.push(json!(
        {
            "name": "conversation_log",
            "title": "Capture a session transcript",
            "description": "Capture a local agent session into hive (Rust-branch addition; not in the Node toolset). Upserts the conversation by (runtime, external_id) — idempotent re-ingest — and writes the supplied turns; pass replace:true to swap the stored transcript (a resumed session re-fires with the FULL transcript). Owner/namespace come from your token. Captured conversations queue for reflection; nothing is journaled directly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "external_id": {"type": "string", "description": "the app's own session id (idempotent capture key)"},
                    "runtime": {"type": "string", "description": "claude_code (default) | codex | opencode | …"},
                    "title": {"type": "string"},
                    "summary": {"type": "string"},
                    "replace": {"type": "boolean", "description": "replace the stored transcript instead of appending"},
                    "messages": {"type": "array", "items": conversation_message_schema}
                },
                "required": ["external_id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            }
        }
    ));
    tools.push(json!(
        {
            "name": "conversation_list_pending",
            "title": "List conversations pending reflection",
            "description": "The reflection queue: captured conversations not yet reflected (reflected_at IS NULL), namespace-scoped, oldest first (Rust-branch addition; not in the Node toolset).",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 200}},
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            }
        }
    ));
    tools.push(json!(
        {
            "name": "conversation_get",
            "title": "Get a conversation transcript",
            "description": "A captured conversation plus its transcript with content flattened to plain text, namespace-checked (Rust-branch addition; not in the Node toolset).",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            }
        }
    ));
    tools.push(json!(
        {
            "name": "conversation_mark_reflected",
            "title": "Mark a conversation reflected",
            "description": "Stamp a captured conversation's reflection cursor and optionally store the rolling summary, draining it from the reflection queue (Rust-branch addition; not in the Node toolset).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "summary": {"type": "string", "description": "the rolling summary reflection produced"}
                },
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            }
        }
    ));
    Value::Array(tools)
}

// ---- tool results ----

/// mcp.ts `ok(data)` — the result content block.
fn ok_content<T: serde::Serialize>(data: &T) -> Value {
    let text = serde_json::to_string_pretty(data).unwrap_or_else(|_| "null".to_string());
    json!({"content": [{"type": "text", "text": text}]})
}

/// The SDK's createToolError — a thrown handler error becomes isError content.
fn tool_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

/// Registry/instance validation issues rendered for tool consumers.
fn issues_text(issues: &[crate::store::entity_validation::FieldIssue]) -> String {
    let lines: Vec<String> = issues
        .iter()
        .map(|i| format!("{}: {} ({})", i.field, i.message, i.code))
        .collect();
    format!("validation failed\n{}", lines.join("\n"))
}

/// EntityWriteError → CallToolResult (store errors keep propagating).
fn entity_write_result(e: crate::store::custom_entities::EntityWriteError) -> ToolResult {
    use crate::store::custom_entities::EntityWriteError as E;
    match e {
        E::Issues(issues) => Ok(tool_error(&issues_text(&issues))),
        E::UnknownType => Ok(tool_error("unknown entity type")),
        E::ArchivedType => Ok(tool_error(
            "type is archived; unarchive it to add instances",
        )),
        E::Other(err) => Err(err.into()),
    }
}

enum ToolFailure {
    /// A validation failure — already rendered as a CallToolResult.
    Invalid(Value),
    Store(anyhow::Error),
}

impl From<anyhow::Error> for ToolFailure {
    fn from(e: anyhow::Error) -> Self {
        ToolFailure::Store(e)
    }
}

type ToolResult = Result<Value, ToolFailure>;

// ---- zod-style validation (matches the Node SDK's wrapped zod v3 messages
// for the common cases; deeper/nested failures fall back to serde's message
// inside the same "Input validation error" wrapper) ----

fn received_kind(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn render_path(path: &[String]) -> String {
    if path.is_empty() {
        return "\"path\": []".to_string();
    }
    let segs = path
        .iter()
        .map(|p| format!("      {p}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("\"path\": [\n{segs}\n    ]")
}

/// One zod v3 invalid_type issue, rendered as JSON.stringify(_, null, 2) would.
fn issue_invalid_type(expected: &str, received: &str, path: &[String], message: &str) -> String {
    format!(
        "  {{\n    \"code\": \"invalid_type\",\n    \"expected\": {},\n    \"received\": {},\n    {},\n    \"message\": {}\n  }}",
        json_str(expected),
        json_str(received),
        render_path(path),
        json_str(message),
    )
}

fn issue_invalid_enum(received: &Value, options: &[&str], path: &[String]) -> String {
    let opts = options
        .iter()
        .map(|o| format!("      {}", json_str(o)))
        .collect::<Vec<_>>()
        .join(",\n");
    let expected = options
        .iter()
        .map(|o| format!("'{o}'"))
        .collect::<Vec<_>>()
        .join(" | ");
    let received_str = received
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| received.to_string());
    format!(
        "  {{\n    \"received\": {},\n    \"code\": \"invalid_enum_value\",\n    \"options\": [\n{}\n    ],\n    {},\n    \"message\": {}\n  }}",
        json_str(&received_str),
        opts,
        render_path(path),
        json_str(&format!(
            "Invalid enum value. Expected {expected}, received '{received_str}'"
        )),
    )
}

fn issue_number_bound(too_small: bool, bound: i64, path: &[String]) -> String {
    let (code, key, message) = if too_small {
        (
            "too_small",
            "minimum",
            format!("Number must be greater than or equal to {bound}"),
        )
    } else {
        (
            "too_big",
            "maximum",
            format!("Number must be less than or equal to {bound}"),
        )
    };
    format!(
        "  {{\n    \"code\": \"{code}\",\n    \"{key}\": {bound},\n    \"type\": \"number\",\n    \"inclusive\": true,\n    \"exact\": false,\n    {},\n    \"message\": {}\n  }}",
        render_path(path),
        json_str(&message),
    )
}

fn issue_invalid_url(path: &[String]) -> String {
    format!(
        "  {{\n    \"validation\": \"url\",\n    \"code\": \"invalid_string\",\n    {},\n    \"message\": \"Invalid url\"\n  }}",
        render_path(path),
    )
}

fn invalid_args(tool: &str, detail: &str) -> ToolFailure {
    ToolFailure::Invalid(tool_error(&format!(
        "MCP error -32602: Input validation error: Invalid arguments for tool {tool}: {detail}"
    )))
}

/// Per-tool argument reader. Collects zod-style issues so multiple failures
/// report together (as zod does), then `finish()` renders them.
struct Args<'a> {
    tool: &'static str,
    map: &'a Map<String, Value>,
    issues: Vec<String>,
}

impl<'a> Args<'a> {
    fn new(tool: &'static str, map: &'a Map<String, Value>) -> Self {
        Self {
            tool,
            map,
            issues: Vec::new(),
        }
    }

    fn key_path(key: &str) -> Vec<String> {
        vec![json_str(key)]
    }

    fn req_str(&mut self, key: &str) -> Option<&'a str> {
        match self.map.get(key) {
            Some(Value::String(s)) => Some(s.as_str()),
            other => {
                let message = if other.is_none() { "Required" } else { "" };
                let received = received_kind(other);
                let msg = if message.is_empty() {
                    format!("Expected string, received {received}")
                } else {
                    message.to_string()
                };
                self.issues.push(issue_invalid_type(
                    "string",
                    received,
                    &Self::key_path(key),
                    &msg,
                ));
                None
            }
        }
    }

    fn opt_str(&mut self, key: &str) -> Option<&'a str> {
        match self.map.get(key) {
            None => None,
            Some(Value::String(s)) => Some(s.as_str()),
            other => {
                self.issues.push(issue_invalid_type(
                    "string",
                    received_kind(other),
                    &Self::key_path(key),
                    &format!("Expected string, received {}", received_kind(other)),
                ));
                None
            }
        }
    }

    fn opt_bool(&mut self, key: &str) -> Option<bool> {
        match self.map.get(key) {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            other => {
                self.issues.push(issue_invalid_type(
                    "boolean",
                    received_kind(other),
                    &Self::key_path(key),
                    &format!("Expected boolean, received {}", received_kind(other)),
                ));
                None
            }
        }
    }

    fn opt_int(&mut self, key: &str, min: Option<i64>, max: Option<i64>) -> Option<i64> {
        let v = self.map.get(key)?;
        let path = Self::key_path(key);
        let Some(n) = v.as_i64() else {
            if v.is_number() {
                self.issues.push(issue_invalid_type(
                    "integer",
                    "float",
                    &path,
                    "Expected integer, received float",
                ));
            } else {
                self.issues.push(issue_invalid_type(
                    "number",
                    received_kind(Some(v)),
                    &path,
                    &format!("Expected number, received {}", received_kind(Some(v))),
                ));
            }
            return None;
        };
        if let Some(min) = min {
            if n < min {
                self.issues.push(issue_number_bound(true, min, &path));
                return None;
            }
        }
        if let Some(max) = max {
            if n > max {
                self.issues.push(issue_number_bound(false, max, &path));
                return None;
            }
        }
        Some(n)
    }

    fn opt_f64(&mut self, key: &str) -> Option<f64> {
        let v = self.map.get(key)?;
        match v.as_f64() {
            Some(n) => Some(n),
            None => {
                self.issues.push(issue_invalid_type(
                    "number",
                    received_kind(Some(v)),
                    &Self::key_path(key),
                    &format!("Expected number, received {}", received_kind(Some(v))),
                ));
                None
            }
        }
    }

    fn check_enum(&mut self, key: &str, value: &'a str, options: &[&str]) -> Option<&'a str> {
        if options.contains(&value) {
            Some(value)
        } else {
            self.issues.push(issue_invalid_enum(
                &Value::String(value.to_string()),
                options,
                &Self::key_path(key),
            ));
            None
        }
    }

    fn req_enum(&mut self, key: &str, options: &[&str]) -> Option<&'a str> {
        match self.map.get(key) {
            Some(Value::String(s)) => self.check_enum(key, s.as_str(), options),
            other => {
                let expected = options
                    .iter()
                    .map(|o| format!("'{o}'"))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let received = received_kind(other);
                let msg = if other.is_none() {
                    "Required".to_string()
                } else {
                    format!("Expected {expected}, received {received}")
                };
                self.issues.push(issue_invalid_type(
                    &expected,
                    received,
                    &Self::key_path(key),
                    &msg,
                ));
                None
            }
        }
    }

    fn opt_enum(&mut self, key: &str, options: &[&str]) -> Option<&'a str> {
        match self.map.get(key) {
            None => None,
            Some(Value::String(s)) => self.check_enum(key, s.as_str(), options),
            other => {
                let expected = options
                    .iter()
                    .map(|o| format!("'{o}'"))
                    .collect::<Vec<_>>()
                    .join(" | ");
                self.issues.push(issue_invalid_type(
                    &expected,
                    received_kind(other),
                    &Self::key_path(key),
                    &format!("Expected {expected}, received {}", received_kind(other)),
                ));
                None
            }
        }
    }

    fn url_format(&mut self, key: &str, value: &str) {
        if reqwest::Url::parse(value).is_err() {
            self.issues.push(issue_invalid_url(&Self::key_path(key)));
        }
    }

    fn finish(self) -> Result<(), ToolFailure> {
        if self.issues.is_empty() {
            return Ok(());
        }
        let body = self.issues.join(",\n");
        Err(invalid_args(self.tool, &format!("[\n{body}\n]")))
    }
}

// ---- dispatch ----

/// tools/call dispatch — Node parity: handler results are content blocks,
/// thrown handler errors become isError content, an unknown tool is the SDK's
/// "MCP error -32602: Tool X not found" isError result. `actor` is the
/// authenticated identity (authorship pin).
pub async fn call_tool(
    store: &Store,
    ctx: &AuthCtx,
    name: &str,
    args: &Map<String, Value>,
) -> Value {
    match dispatch(store, ctx, name, args).await {
        Ok(v) => v,
        Err(ToolFailure::Invalid(v)) => v,
        Err(ToolFailure::Store(e)) => tool_error(&e.to_string()),
    }
}

async fn dispatch(
    store: &Store,
    ctx: &AuthCtx,
    name: &str,
    args: &Map<String, Value>,
) -> ToolResult {
    // Authorship pin is the authenticated identity; reads/writes are scoped to
    // its per-user namespace (admins are unscoped).
    let actor = ctx.actor();
    let viewer: Option<String> = if ctx.is_admin() {
        None
    } else {
        Some(ctx.namespace_user().to_string())
    };
    match name {
        "workspace_list" => {
            let mut a = Args::new("workspace_list", args);
            let limit = a.opt_int("limit", Some(1), Some(500));
            a.finish()?;
            Ok(ok_content(
                &store
                    .workspace_list(&ctx.visibility(), limit.unwrap_or(50))
                    .await?,
            ))
        }
        "workspace_get" => {
            let mut a = Args::new("workspace_get", args);
            let id = a.req_str("id");
            a.finish()?;
            match store.workspace_get(&ctx.visibility(), id.unwrap()).await? {
                Some(ws) => Ok(ok_content(&ws)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "workspace_transcript" => {
            let mut a = Args::new("workspace_transcript", args);
            let id = a.req_str("id");
            let after = a.opt_int("after", Some(0), None);
            let limit = a.opt_int("limit", Some(1), Some(5000));
            a.finish()?;
            let id = id.unwrap();
            if store.workspace_get(&ctx.visibility(), id).await?.is_none() {
                return Ok(ok_content(&json!({"error": "not found"})));
            }
            Ok(ok_content(
                &store
                    .workspace_transcript(id, after.unwrap_or(0), limit.unwrap_or(2000))
                    .await?,
            ))
        }
        "journal_append" => journal_append(store, ctx, args).await,
        "journal_list" => {
            let mut a = Args::new("journal_list", args);
            let limit = a.opt_int("limit", Some(1), Some(200));
            a.finish()?;
            Ok(ok_content(
                &store
                    .visible_journal(&ctx.visibility(), None, None, limit.unwrap_or(30), 0)
                    .await?,
            ))
        }
        "journal_get" => {
            let mut a = Args::new("journal_get", args);
            let id = a.req_str("id");
            a.finish()?;
            match store.journal_get(id.unwrap(), &ctx.visibility()).await? {
                Some(e) => Ok(ok_content(&e)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "identity_update" => {
            let mut a = Args::new("identity_update", args);
            let bio = a.opt_str("bio").map(String::from);
            let role = a.opt_str("role").map(String::from);
            a.finish()?;
            let mut sections: BTreeMap<String, String> = BTreeMap::new();
            if let Some(bio) = bio {
                sections.insert("bio".to_string(), bio);
            }
            if let Some(role) = role {
                sections.insert("role".to_string(), role);
            }
            let patch = ProfilePatch {
                sections: Some(sections),
                ..Default::default()
            };
            Ok(ok_content(
                &store.profile_update(actor, patch, actor).await?,
            ))
        }
        "tasks_list" => {
            let mut a = Args::new("tasks_list", args);
            let status = a.opt_str("status").map(String::from);
            let assignee = a.opt_str("assignee").map(String::from);
            a.finish()?;
            let filter = TaskFilter {
                status,
                assignee,
                ..Default::default()
            };
            Ok(ok_content(&store.tasks_list(filter).await?))
        }
        "task_set_status" => {
            let mut a = Args::new("task_set_status", args);
            let id = a.req_str("id");
            let status = a.req_enum("status", TASK_STATUSES);
            a.finish()?;
            let patch = TaskPatch {
                status: TaskStatus::parse(status.unwrap()),
                ..Default::default()
            };
            match store.tasks_update(id.unwrap(), patch, actor).await? {
                Some(t) => Ok(ok_content(&t)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "decisions_list" => {
            let mut a = Args::new("decisions_list", args);
            let status = a.opt_str("status").map(String::from);
            a.finish()?;
            Ok(ok_content(&store.decisions_list(status.as_deref()).await?))
        }
        "events_list" => Ok(ok_content(&store.events_list().await?)),
        "inbox_list" => {
            let mut a = Args::new("inbox_list", args);
            let recipient = a.req_enum("recipient", &actor_names());
            let unread_only = a.opt_bool("unread_only");
            a.finish()?;
            let recipient = recipient.unwrap();
            // Viewer gate: an inbox is private to its recipient — snippets
            // quote entries other viewers may not see (DIRECTION.md Phase 0).
            if !can_act_for_identity(store, ctx, recipient).await? {
                return Ok(tool_error("forbidden"));
            }
            Ok(ok_content(
                &store
                    .inbox_list(recipient, unread_only.unwrap_or(true))
                    .await?,
            ))
        }
        "inbox_mark_read" => {
            let mut a = Args::new("inbox_mark_read", args);
            let id = a.opt_str("id").map(String::from);
            let recipient = a.opt_str("recipient").map(String::from);
            a.finish()?;
            if let Some(id) = id {
                // Gate on the item's recipient (kind-agnostic — the row must
                // stay markable even when its ref_kind postdates this build).
                // Missing and foreign ids answer the same {"marked": false} so
                // the tool doesn't oracle which ids exist in others' inboxes.
                let allowed = match store.inbox_recipient(&id).await? {
                    Some(recipient) => can_act_for_identity(store, ctx, &recipient).await?,
                    None => false,
                };
                if !allowed {
                    return Ok(ok_content(&json!({"marked": false})));
                }
                let marked = store.inbox_mark_read(&id).await? > 0;
                return Ok(ok_content(&json!({"marked": marked})));
            }
            if let Some(recipient) = recipient {
                if !can_act_for_identity(store, ctx, &recipient).await? {
                    return Ok(tool_error("forbidden"));
                }
                let marked = store.inbox_mark_all_read(&recipient).await?;
                return Ok(ok_content(&json!({"marked": marked})));
            }
            Ok(ok_content(&json!({"error": "provide id or recipient"})))
        }
        "search" => {
            let mut a = Args::new("search", args);
            let q = a.req_str("q").map(String::from);
            let limit = a.opt_int("limit", None, None);
            a.finish()?;
            let limit = limit.unwrap_or(25).max(0) as usize;
            Ok(ok_content(
                &store.search(&q.unwrap(), limit, viewer.as_deref()).await?,
            ))
        }
        "mail_search" => {
            let mut a = Args::new("mail_search", args);
            let q = a.req_str("q").map(String::from);
            let limit = a.opt_int("limit", Some(1), Some(200));
            a.finish()?;
            Ok(ok_content(
                &store
                    .mail_search(&q.unwrap(), viewer.as_deref(), limit.unwrap_or(50))
                    .await?,
            ))
        }
        "mail_thread_get" => {
            let mut a = Args::new("mail_thread_get", args);
            let thread_id = a.req_str("thread_id").map(String::from);
            a.finish()?;
            Ok(ok_content(
                &store
                    .mail_thread_get(&thread_id.unwrap(), viewer.as_deref())
                    .await?,
            ))
        }
        "mail_accounts_list" => {
            let a = Args::new("mail_accounts_list", args);
            a.finish()?;
            Ok(ok_content(
                &store.mail_accounts_list(viewer.as_deref()).await?,
            ))
        }
        "dashboard" => {
            if !ctx.is_admin() {
                return Ok(ok_content(&json!({"error": "admin only"})));
            }
            Ok(ok_content(&store.dashboard().await?))
        }
        "semantic_search" => {
            let mut a = Args::new("semantic_search", args);
            let q = a.req_str("q").map(String::from);
            let limit = a.opt_int("limit", None, None);
            let mode = a
                .opt_enum("mode", &["standard", "precision"])
                .map(String::from);
            let hybrid = a.opt_bool("hybrid");
            let rerank = a.opt_bool("rerank");
            let blanket = a.opt_bool("blanket");
            let threshold = a.opt_f64("threshold");
            a.finish()?;
            let opts = SemanticOptions {
                limit: Some(limit.unwrap_or(10).max(0) as usize),
                mode,
                hybrid,
                rerank,
                blanket,
                threshold,
                viewer: viewer.clone(),
                ..Default::default()
            };
            Ok(ok_content(&store.semantic_search(&q.unwrap(), opts).await?))
        }
        "profile_get" => {
            let mut a = Args::new("profile_get", args);
            let target = a.req_str("actor");
            a.finish()?;
            match store.profile_get(target.unwrap()).await? {
                Some(p) => Ok(ok_content(&p)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "profile_update" => {
            let mut a = Args::new("profile_update", args);
            let target = a.req_str("actor").map(String::from);
            let display_name = a.opt_str("display_name").map(String::from);
            let kind = a.opt_enum("kind", &["human", "ai"]).map(|k| {
                if k == "ai" {
                    ActorKind::Ai
                } else {
                    ActorKind::Human
                }
            });
            let sections = match args.get("sections") {
                None => None,
                Some(Value::Object(m)) => {
                    let mut out: BTreeMap<String, String> = BTreeMap::new();
                    for (k, v) in m {
                        match v.as_str() {
                            Some(s) => {
                                out.insert(k.clone(), s.to_string());
                            }
                            None => a.issues.push(issue_invalid_type(
                                "string",
                                received_kind(Some(v)),
                                &[json_str("sections"), json_str(k)],
                                &format!("Expected string, received {}", received_kind(Some(v))),
                            )),
                        }
                    }
                    Some(out)
                }
                other => {
                    a.issues.push(issue_invalid_type(
                        "object",
                        received_kind(other),
                        &[json_str("sections")],
                        &format!("Expected object, received {}", received_kind(other)),
                    ));
                    None
                }
            };
            a.finish()?;
            let patch = ProfilePatch {
                display_name,
                kind,
                sections,
            };
            let target = target.unwrap();
            if !can_edit_actor_profile(store, ctx, &target).await? {
                return Ok(tool_error("forbidden"));
            }
            // Node passes "mcp" as the acting principal here (not the token actor).
            Ok(ok_content(
                &store.profile_update(&target, patch, "mcp").await?,
            ))
        }
        "recall" => {
            let mut a = Args::new("recall", args);
            let identity = a.req_str("identity").map(String::from);
            let peer = a.opt_str("peer").map(String::from);
            let query = a.opt_str("query").map(String::from);
            let budget = a.opt_int("budget", None, None);
            let threshold = a.opt_f64("threshold");
            a.finish()?;
            let identity = identity.unwrap();
            if !can_act_for_identity(store, ctx, &identity).await? {
                return Ok(tool_error("not_your_identity"));
            }
            let opts = RecallOptions {
                peer,
                query,
                budget: budget.map(|b| b.max(0) as usize),
                threshold,
                viewer: viewer.clone(),
            };
            Ok(ok_content(&store.recall(&identity, opts).await?))
        }
        "sources_list" => Ok(ok_content(&store.sources_list(None).await?)),
        "sources_add" => {
            let mut a = Args::new("sources_add", args);
            a.req_str("name");
            if let Some(url) = a.req_str("url") {
                let url = url.to_string();
                a.url_format("url", &url);
            }
            a.opt_enum("kind", &["rss", "scrape"]);
            a.opt_str("category");
            a.opt_enum("severity", SEVERITIES);
            a.opt_int("interval_secs", Some(30), None);
            a.opt_enum("notify", &actor_names());
            if let Some(owner) = args.get("owner") {
                if !owner.is_null() {
                    if let Some(s) = owner.as_str() {
                        a.check_enum("owner", s, &actor_names());
                    } else {
                        a.issues.push(issue_invalid_type(
                            "string",
                            received_kind(Some(owner)),
                            &[json_str("owner")],
                            &format!("Expected string, received {}", received_kind(Some(owner))),
                        ));
                    }
                }
            }
            a.finish()?;
            let input: NewSource = serde_json::from_value(Value::Object(args.clone()))
                .map_err(|e| invalid_args("sources_add", &e.to_string()))?;
            Ok(ok_content(&store.sources_create(input, actor).await?))
        }
        "sources_update" => {
            let mut a = Args::new("sources_update", args);
            let id = a.req_str("id").map(String::from);
            let enabled = a.opt_bool("enabled");
            let interval_secs = a.opt_int("interval_secs", Some(30), None);
            let severity = a.opt_enum("severity", SEVERITIES).map(|s| {
                serde_json::from_value::<Severity>(Value::String(s.to_string()))
                    .expect("validated severity")
            });
            let category = a.opt_str("category").map(String::from);
            let notify = a.opt_str("notify").map(String::from);
            a.finish()?;
            let patch = SourcePatch {
                enabled,
                interval_secs,
                severity,
                category: category.map(Some),
                notify: notify.map(Some),
                ..Default::default()
            };
            match store.sources_update(&id.unwrap(), patch, actor).await? {
                Some(s) => Ok(ok_content(&s)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "sources_remove" => {
            let mut a = Args::new("sources_remove", args);
            let id = a.req_str("id");
            a.finish()?;
            let removed = store.sources_remove(id.unwrap(), actor).await?;
            Ok(ok_content(&json!({"removed": removed})))
        }
        "outbox_list" => {
            let mut a = Args::new("outbox_list", args);
            let limit = a.opt_int("limit", None, None);
            a.finish()?;
            Ok(ok_content(&store.outbox_list(limit.unwrap_or(50)).await?))
        }
        "worker_status" => Ok(ok_content(&store.worker_status().await?)),
        "people_list" => Ok(ok_content(&store.people_list().await?)),
        "topics_list" => Ok(ok_content(&store.topics_list().await?)),
        "projects_list" => Ok(ok_content(&store.projects_list().await?)),
        "phases_list" => {
            let mut a = Args::new("phases_list", args);
            let project_id = a.opt_str("project_id").map(String::from);
            a.finish()?;
            Ok(ok_content(&store.phases_list(project_id.as_deref()).await?))
        }
        "share_entry" => {
            let mut a = Args::new("share_entry", args);
            let scope = a.req_enum("scope", &["entry", "journal"]);
            let ref_ = a.req_str("ref").map(String::from);
            let viewer = a.req_enum("viewer", &actor_names()).map(String::from);
            a.finish()?;
            let input = NewShare {
                scope: ShareScope::from_str_lossy(scope.unwrap()),
                ref_: ref_.unwrap(),
                viewer: viewer.unwrap(),
            };
            Ok(ok_content(&store.shares_create(input).await?))
        }
        "actor_delete" => {
            if !is_admin(store, actor).await? {
                return Ok(forbidden());
            }
            let mut a = Args::new("actor_delete", args);
            let slug = a.req_str("slug").map(String::from);
            let dry_run = a.opt_bool("dry_run");
            a.finish()?;
            let slug = slug.unwrap();
            if store.people_get(&slug).await?.is_none() {
                return Ok(ok_content(&json!({"error": "not found"})));
            }
            if dry_run.unwrap_or(false) {
                Ok(ok_content(&store.actors_remove_preview(&slug).await?))
            } else {
                Ok(ok_content(&store.actors_remove(&slug).await?))
            }
        }
        "actor_merge" => {
            if !is_admin(store, actor).await? {
                return Ok(forbidden());
            }
            let mut a = Args::new("actor_merge", args);
            let from = a.req_str("from").map(String::from);
            let into = a.req_str("into").map(String::from);
            let dry_run = a.opt_bool("dry_run");
            a.finish()?;
            let (from, into) = (from.unwrap(), into.unwrap());
            if from == into {
                return Ok(ok_content(
                    &json!({"error": "cannot merge an actor into itself"}),
                ));
            }
            if store.people_get(&from).await?.is_none() {
                return Ok(ok_content(
                    &json!({"error": format!("from actor '{from}' not found")}),
                ));
            }
            if store.people_get(&into).await?.is_none() {
                return Ok(ok_content(
                    &json!({"error": format!("into actor '{into}' not found")}),
                ));
            }
            if dry_run.unwrap_or(false) {
                Ok(ok_content(&store.actors_merge_preview(&from, &into).await?))
            } else {
                Ok(ok_content(&store.actors_merge(&from, &into).await?))
            }
        }
        // ---- Rust-branch identity tools ----
        "identity_link" => {
            let mut a = Args::new("identity_link", args);
            let platform = a.req_str("platform").map(String::from);
            let platform_id = a.req_str("platform_id").map(String::from);
            let target = a.req_str("actor").map(String::from);
            a.finish()?;
            let identity = store
                .identities_create(
                    NewIdentity {
                        platform: platform.unwrap(),
                        platform_id: platform_id.unwrap(),
                        actor: target.unwrap(),
                    },
                    actor,
                )
                .await?;
            Ok(ok_content(&json!({"linked": true, "identity": identity})))
        }
        "identity_resolve" => {
            let mut a = Args::new("identity_resolve", args);
            let platform = a.req_str("platform").map(String::from);
            let platform_id = a.req_str("platform_id").map(String::from);
            a.finish()?;
            let resolved = store
                .identities_resolve(&platform.unwrap(), &platform_id.unwrap())
                .await?;
            Ok(ok_content(&json!({"actor": resolved})))
        }
        "identity_list" => {
            let mut a = Args::new("identity_list", args);
            let target = a.opt_str("actor").map(String::from);
            a.finish()?;
            let items = match target {
                Some(t) => store.identities_for_actor(&t).await?,
                None => store.identities_list().await?,
            };
            Ok(ok_content(
                &json!({"count": items.len(), "identities": items}),
            ))
        }
        "identity_unlink" => {
            let mut a = Args::new("identity_unlink", args);
            let id = a.req_str("id");
            a.finish()?;
            let removed = store.identities_remove(id.unwrap(), actor).await?;
            Ok(ok_content(&json!({"removed": removed})))
        }
        "entity_types_list" => {
            let mut a = Args::new("entity_types_list", args);
            let include = a.opt_bool("include_archived");
            a.finish()?;
            Ok(ok_content(
                &store.entity_types_list(include.unwrap_or(false)).await?,
            ))
        }
        "entity_type_create" => {
            if !is_admin(store, actor).await? {
                return Ok(forbidden());
            }
            let input: hive_shared::NewEntityType =
                serde_json::from_value(Value::Object(args.clone()))
                    .map_err(|e| invalid_args("entity_type_create", &e.to_string()))?;
            match store.entity_types_create(input, actor).await {
                Ok(view) => Ok(ok_content(&view)),
                Err(crate::store::entity_types::TypeWriteError::Issues(issues)) => {
                    Ok(tool_error(&issues_text(&issues)))
                }
                Err(crate::store::entity_types::TypeWriteError::Other(e)) => Err(e.into()),
            }
        }
        "entity_type_update" => {
            if !is_admin(store, actor).await? {
                return Ok(forbidden());
            }
            let mut a = Args::new("entity_type_update", args);
            let target = a.req_str("type").map(String::from);
            a.finish()?;
            let mut body = args.clone();
            body.remove("type");
            let patch: hive_shared::EntityTypePatch =
                serde_json::from_value(Value::Object(body))
                    .map_err(|e| invalid_args("entity_type_update", &e.to_string()))?;
            match store
                .entity_types_update(&target.unwrap(), patch, actor)
                .await
            {
                Ok(Some(view)) => Ok(ok_content(&view)),
                Ok(None) => Ok(ok_content(&json!({"error": "not found"}))),
                Err(crate::store::entity_types::TypeWriteError::Issues(issues)) => {
                    Ok(tool_error(&issues_text(&issues)))
                }
                Err(crate::store::entity_types::TypeWriteError::Other(e)) => Err(e.into()),
            }
        }
        "entities_list" => {
            let mut a = Args::new("entities_list", args);
            let type_slug = a.req_str("type").map(String::from);
            let limit = a.opt_int("limit", Some(1), Some(500));
            let offset = a.opt_int("offset", Some(0), None);
            let sort = a.opt_str("sort").map(String::from);
            let dir = a.opt_enum("dir", &["asc", "desc"]).map(String::from);
            a.finish()?;
            let fields = args
                .get("filters")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| {
                            let vs = match v {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            (k.clone(), vs)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let filter = crate::store::custom_entities::EntityFilter {
                type_slug: type_slug.unwrap(),
                limit: limit.unwrap_or(100),
                offset: offset.unwrap_or(0),
                sort,
                desc: dir.as_deref() != Some("asc"),
                fields,
            };
            match store.custom_entities_list(&filter, &ctx.visibility()).await {
                Ok(items) => Ok(ok_content(&items)),
                Err(e) => entity_write_result(e),
            }
        }
        "entity_get" => {
            let mut a = Args::new("entity_get", args);
            let id = a.req_str("id");
            a.finish()?;
            match store
                .custom_entities_get(id.unwrap(), &ctx.visibility())
                .await?
            {
                Some(e) => Ok(ok_content(&e)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "entity_create" => {
            let input: hive_shared::NewCustomEntity =
                serde_json::from_value(Value::Object(args.clone()))
                    .map_err(|e| invalid_args("entity_create", &e.to_string()))?;
            match store
                .custom_entities_create(input, actor, ctx.namespace_owner())
                .await
            {
                Ok(e) => Ok(ok_content(&e)),
                Err(e) => entity_write_result(e),
            }
        }
        "entity_update" => {
            let mut a = Args::new("entity_update", args);
            let id = a.req_str("id").map(String::from);
            a.finish()?;
            let mut body = args.clone();
            body.remove("id");
            let patch: hive_shared::CustomEntityPatch = serde_json::from_value(Value::Object(body))
                .map_err(|e| invalid_args("entity_update", &e.to_string()))?;
            match store
                .custom_entities_update(
                    &id.unwrap(),
                    patch,
                    actor,
                    &ctx.visibility(),
                    ctx.namespace_owner(),
                )
                .await
            {
                Ok(Some(e)) => Ok(ok_content(&e)),
                Ok(None) => Ok(ok_content(&json!({"error": "not found"}))),
                Err(e) => entity_write_result(e),
            }
        }
        "entity_delete" => {
            let mut a = Args::new("entity_delete", args);
            let id = a.req_str("id");
            a.finish()?;
            match store
                .custom_entities_delete(id.unwrap(), actor, &ctx.visibility())
                .await?
            {
                Some(()) => Ok(ok_content(&json!({"deleted": true}))),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        // Claude Code artifacts for the authenticated identity (its own skills /
        // agents / commands) — same keying as the REST sync endpoint.
        "artifacts_list" => {
            let a = Args::new("artifacts_list", args);
            a.finish()?;
            let items = store.artifacts_list(actor).await?;
            Ok(ok_content(
                &json!({"count": items.len(), "artifacts": items}),
            ))
        }
        "artifacts_get" => {
            let mut a = Args::new("artifacts_get", args);
            let id = a.req_str("id");
            a.finish()?;
            // Identity gate on the row's actor; a foreign id answers exactly
            // like a missing one so the tool doesn't oracle other identities'
            // artifact ids (same posture as inbox_mark_read).
            match store.artifacts_get(id.unwrap()).await? {
                Some(art) if can_act_for_identity(store, ctx, &art.actor).await? => {
                    Ok(ok_content(&art))
                }
                _ => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        // ---- conversation capture + reflection queue ----
        "conversation_log" => conversation_log(store, ctx, args).await,
        "conversation_list_pending" => {
            let mut a = Args::new("conversation_list_pending", args);
            let limit = a.opt_int("limit", Some(1), Some(200));
            a.finish()?;
            Ok(ok_content(
                &store
                    .conversations_pending(&ctx.visibility(), limit.unwrap_or(50))
                    .await?,
            ))
        }
        "conversation_get" => {
            let mut a = Args::new("conversation_get", args);
            let id = a.req_str("id");
            a.finish()?;
            match store
                .conversation_get_flat(&ctx.visibility(), id.unwrap())
                .await?
            {
                Some(view) => Ok(ok_content(&view)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        "conversation_mark_reflected" => {
            let mut a = Args::new("conversation_mark_reflected", args);
            let id = a.req_str("id").map(String::from);
            let summary = a.opt_str("summary").map(String::from);
            a.finish()?;
            let id = id.unwrap();
            // Owner-or-admin, the same write gate as the REST route.
            let Some(conv) = store.conversation_get_captured(&id).await? else {
                return Ok(ok_content(&json!({"error": "not found"})));
            };
            if !ctx.is_admin() && ctx.namespace_user() != conv.owner {
                return Ok(tool_error("forbidden"));
            }
            match store
                .conversation_mark_reflected(&id, summary.as_deref())
                .await?
            {
                Some(c) => Ok(ok_content(&c)),
                None => Ok(ok_content(&json!({"error": "not found"}))),
            }
        }
        _ => Ok(tool_error(&format!(
            "MCP error -32602: Tool {name} not found"
        ))),
    }
}

async fn journal_append(store: &Store, ctx: &AuthCtx, args: &Map<String, Value>) -> ToolResult {
    let mut a = Args::new("journal_append", args);
    a.req_str("body");
    a.finish()?;
    // tags/anchors structure via serde — nested anchor failures report the
    // serde message inside the SDK's "Input validation error" wrapper.
    let mut input: NewJournalEntry = serde_json::from_value(Value::Object(args.clone()))
        .map_err(|e| invalid_args("journal_append", &e.to_string()))?;
    // Author is the token's actor — a client cannot write as someone else. The
    // entry lands in the writing principal's namespace (its granting user).
    let actor = ctx.actor().to_string();
    input.author = Some(actor.clone());
    Ok(ok_content(
        &store
            .journal_append(input, Some(&actor), ctx.namespace_owner())
            .await?,
    ))
}

/// conversation_log: capture upsert (by runtime/external_id) + transcript
/// write in one call — what an agent's SessionEnd hook fires. Owner/namespace
/// come from the token's ctx. Deliberately NO journal mirroring (reflection
/// summarizes into the journal later).
async fn conversation_log(store: &Store, ctx: &AuthCtx, args: &Map<String, Value>) -> ToolResult {
    let mut a = Args::new("conversation_log", args);
    let external_id = a.req_str("external_id").map(String::from);
    let runtime = a.opt_str("runtime").map(String::from);
    let title = a.opt_str("title").map(String::from);
    let summary = a.opt_str("summary").map(String::from);
    let replace = a.opt_bool("replace").unwrap_or(false);
    a.finish()?;
    // messages via serde — nested failures report the serde message inside
    // the SDK's "Input validation error" wrapper (journal_append precedent).
    let messages: Vec<hive_shared::NewConversationMessage> = match args.get("messages") {
        None => Vec::new(),
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| invalid_args("conversation_log", &e.to_string()))?,
    };
    let external_id = external_id.unwrap();
    if external_id.trim().is_empty() {
        return Ok(tool_error("external_id required"));
    }
    let owner = ctx.namespace_user().to_string();
    let input = hive_shared::NewCapturedConversation {
        runtime,
        external_id,
        title,
        summary,
    };
    let Some(id) = store
        .conversation_upsert_captured(&owner, ctx.actor(), input)
        .await?
    else {
        // The capture key belongs to a different owner's session.
        return Ok(tool_error("forbidden"));
    };
    // A metadata-only refresh (no turns, no replace) must not dirty the
    // reflection queue, so skip the transcript write entirely.
    let appended = if messages.is_empty() && !replace {
        0
    } else {
        store
            .conversation_replace_messages(&id, &messages, replace)
            .await?
    };
    Ok(ok_content(&json!({"id": id, "appended": appended})))
}

/// One shared identity gate for MCP and HTTP: crate::middleware::can_act_for_identity.
async fn can_act_for_identity(
    store: &Store,
    ctx: &AuthCtx,
    identity: &str,
) -> Result<bool, ToolFailure> {
    Ok(crate::middleware::can_act_for_identity(store, ctx, identity).await?)
}

async fn can_edit_actor_profile(
    store: &Store,
    ctx: &AuthCtx,
    actor: &str,
) -> Result<bool, ToolFailure> {
    if ctx.is_admin() || actor == ctx.actor() {
        return Ok(true);
    }
    if ctx.principal == Some("session") {
        let Some(target) = store.people_get(actor).await? else {
            return Ok(false);
        };
        return Ok(target.owner.as_deref() == Some(ctx.actor()));
    }
    Ok(false)
}

/// mcp.ts isAdmin(): the token's actor maps to a user with role 'admin'.
async fn is_admin(store: &Store, actor: &str) -> Result<bool, ToolFailure> {
    let users = store.users_list().await?;
    Ok(users
        .iter()
        .any(|u| u.actor == actor && u.role == UserRole::Admin))
}

fn forbidden() -> Value {
    ok_content(&json!({"error": "forbidden — admin only"}))
}
