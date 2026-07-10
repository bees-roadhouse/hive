// Hosted Claude Code workspaces — the control-plane data layer. A "workspace" is
// one Claude Code session hive spins up and drives in an isolated sandbox; the
// cc_messages rows are its complete transcript (the full chat history). Both are
// scoped per `owner` (the human whose workspace it is) — see middleware Visibility.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{new_id, now_iso, Store};
use crate::Visibility;

const SESSION_COLS: &str =
    "id, owner, created_by, title, workdir, claude_session_id, runtime, status, \
    model, usage, meta, repo_url, repo_ref, created_at, updated_at, last_activity_at";
const MESSAGE_COLS: &str =
    "id, session_id, seq, role, kind, content, raw, tokens_in, tokens_out, created_at";

/// A hosted Claude Code session/workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcSession {
    pub id: String,
    pub owner: String,
    pub created_by: String,
    pub title: String,
    pub workdir: String,
    pub claude_session_id: Option<String>,
    pub runtime: String,
    pub status: String,
    pub model: Option<String>,
    pub usage: Value,
    pub meta: Value,
    pub repo_url: Option<String>,
    pub repo_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_activity_at: Option<String>,
}

/// One transcript message — part of the complete chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcMessage {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub role: String,
    pub kind: String,
    pub content: Value,
    pub raw: Value,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub created_at: String,
}

/// Create-a-workspace request.
#[derive(Debug, Clone, Deserialize)]
pub struct NewCcSession {
    /// Runtime backend: claude_code (default), codex, or opencode.
    pub runtime: Option<String>,
    /// Provider hint for runtimes that multiplex providers (notably OpenCode).
    pub provider: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    /// System-wide tags for grouping and search.
    pub tags: Option<Vec<String>>,
    /// Optional project grouping slug/name.
    pub project: Option<String>,
    /// Optional typed entities to relate to this conversation.
    pub linked_entities: Option<Vec<LinkedEntity>>,
    /// Optional first prompt to kick the session off.
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkedEntity {
    pub kind: String,
    pub id: String,
    pub rel: Option<String>,
}

/// Append-a-message request (runner → API ingest).
#[derive(Debug, Clone, Deserialize)]
pub struct NewCcMessage {
    pub role: String,
    pub kind: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub raw: Value,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// Runner reports Claude Code's own session id once known (for resume).
    pub claude_session_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    owner: String,
    created_by: String,
    title: String,
    workdir: String,
    claude_session_id: Option<String>,
    runtime: String,
    status: String,
    model: Option<String>,
    usage: String,
    meta: String,
    repo_url: Option<String>,
    repo_ref: Option<String>,
    created_at: String,
    updated_at: String,
    last_activity_at: Option<String>,
}

impl SessionRow {
    fn into_view(self) -> CcSession {
        CcSession {
            id: self.id,
            owner: self.owner,
            created_by: self.created_by,
            title: self.title,
            workdir: self.workdir,
            claude_session_id: self.claude_session_id,
            runtime: normalize_runtime(Some(&self.runtime)),
            status: self.status,
            model: self.model,
            usage: serde_json::from_str(&self.usage).unwrap_or(Value::Null),
            meta: serde_json::from_str(&self.meta).unwrap_or(Value::Null),
            repo_url: self.repo_url,
            repo_ref: self.repo_ref,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_activity_at: self.last_activity_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: String,
    session_id: String,
    seq: i64,
    role: String,
    kind: String,
    content: String,
    raw: String,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    created_at: String,
}

impl MessageRow {
    fn into_view(self) -> CcMessage {
        CcMessage {
            id: self.id,
            session_id: self.session_id,
            seq: self.seq,
            role: self.role,
            kind: self.kind,
            content: serde_json::from_str(&self.content).unwrap_or(Value::Null),
            raw: serde_json::from_str(&self.raw).unwrap_or(Value::Null),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            created_at: self.created_at,
        }
    }
}

/// Root under which per-session sandboxes live; the runner creates the dirs.
fn workspaces_root() -> String {
    std::env::var("HIVE_WORKSPACES_ROOT").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{home}/.hive/workspaces")
    })
}

fn visible(vis: &Visibility, owner: &str) -> bool {
    match vis {
        Visibility::All => true,
        Visibility::Namespace(v) => v == owner,
    }
}

pub fn normalize_runtime(runtime: Option<&str>) -> String {
    match runtime.unwrap_or("claude_code").trim() {
        "" | "claude" | "claude_code" => "claude_code".to_string(),
        "codex" => "codex".to_string(),
        "opencode" => "opencode".to_string(),
        other => other.to_string(),
    }
}

fn normalize_tags(tags: Option<Vec<String>>) -> Vec<String> {
    let mut out = Vec::new();
    for tag in tags.unwrap_or_default() {
        let t = tag.trim().trim_start_matches('#').to_ascii_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

fn normalize_project(project: Option<String>) -> Option<String> {
    project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Store {
    pub async fn workspace_create(
        &self,
        owner: &str,
        created_by: &str,
        input: NewCcSession,
    ) -> Result<CcSession> {
        let id = new_id("ccs");
        let workdir = format!("{}/{}/{}", workspaces_root(), owner, id);
        let ts = now_iso();
        let title = input.title.unwrap_or_default();
        let runtime = normalize_runtime(input.runtime.as_deref());
        let provider = input
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let tags = normalize_tags(input.tags);
        let project = normalize_project(input.project);
        let meta = json!({
            "kind": "conversation",
            "runtime": &runtime,
            "provider": &provider,
            "tags": &tags,
            "project": &project,
        })
        .to_string();
        crate::pgq::query(
            "INSERT INTO cc_sessions \
             (id, owner, created_by, title, workdir, runtime, status, model, usage, meta, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'provisioning', ?, '{}', ?, ?, ?)",
        )
        .bind(&id)
        .bind(owner)
        .bind(created_by)
        .bind(&title)
        .bind(&workdir)
        .bind(&runtime)
        .bind(&input.model)
        .bind(&meta)
        .bind(&ts)
        .bind(&ts)
        .execute(self.db())
        .await?;
        self.emit(
            "workspace.created",
            created_by,
            json!({"id": id, "owner": owner, "title": title, "runtime": runtime}),
        )
        .await?;
        self.workspace_get_internal(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workspace {id} vanished after insert"))
    }

    /// Get by id WITHOUT a visibility check (post-create, runner, internal paths).
    pub async fn workspace_get_internal(&self, id: &str) -> Result<Option<CcSession>> {
        let row = crate::pgq::query_as::<SessionRow>(&format!(
            "SELECT {SESSION_COLS} FROM cc_sessions WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        Ok(row.map(SessionRow::into_view))
    }

    /// Get by id, gated by the caller's namespace visibility.
    pub async fn workspace_get(&self, vis: &Visibility, id: &str) -> Result<Option<CcSession>> {
        Ok(self
            .workspace_get_internal(id)
            .await?
            .filter(|s| visible(vis, &s.owner)))
    }

    pub async fn workspace_list(&self, vis: &Visibility, limit: i64) -> Result<Vec<CcSession>> {
        let rows = match vis {
            Visibility::All => {
                crate::pgq::query_as::<SessionRow>(&format!(
                    "SELECT {SESSION_COLS} FROM cc_sessions ORDER BY created_at DESC LIMIT ?"
                ))
                .bind(limit)
                .fetch_all(self.db())
                .await?
            }
            Visibility::Namespace(viewer) => {
                crate::pgq::query_as::<SessionRow>(&format!(
                    "SELECT {SESSION_COLS} FROM cc_sessions WHERE owner = ? ORDER BY created_at DESC LIMIT ?"
                ))
                .bind(viewer)
                .bind(limit)
                .fetch_all(self.db())
                .await?
            }
        };
        Ok(rows.into_iter().map(SessionRow::into_view).collect())
    }

    /// Transcript for a session, in order, after `after_seq` (0 = from the start).
    pub async fn workspace_transcript(
        &self,
        session_id: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<CcMessage>> {
        let rows = crate::pgq::query_as::<MessageRow>(&format!(
            "SELECT {MESSAGE_COLS} FROM cc_messages \
             WHERE session_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?"
        ))
        .bind(session_id)
        .bind(after_seq)
        .bind(limit)
        .fetch_all(self.db())
        .await?;
        Ok(rows.into_iter().map(MessageRow::into_view).collect())
    }

    /// Append one transcript message (assigns the next monotonic seq) and bump the
    /// session's activity. Broadcasts `workspace.message` for live UI streaming.
    pub async fn workspace_append_message(
        &self,
        session_id: &str,
        input: NewCcMessage,
    ) -> Result<CcMessage> {
        let next_seq: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COALESCE(MAX(seq), 0) FROM cc_messages WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(self.db())
        .await?
            + 1;
        let id = new_id("ccm");
        let ts = now_iso();
        let content = if input.content.is_null() {
            "{}".to_string()
        } else {
            input.content.to_string()
        };
        let raw = if input.raw.is_null() {
            "{}".to_string()
        } else {
            input.raw.to_string()
        };
        crate::pgq::query(
            "INSERT INTO cc_messages \
             (id, session_id, seq, role, kind, content, raw, tokens_in, tokens_out, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(session_id)
        .bind(next_seq)
        .bind(&input.role)
        .bind(&input.kind)
        .bind(&content)
        .bind(&raw)
        .bind(input.tokens_in)
        .bind(input.tokens_out)
        .bind(&ts)
        .execute(self.db())
        .await?;

        // Bump activity; record Claude Code's session id the first time we see it.
        if let Some(csid) = input.claude_session_id.as_deref().filter(|s| !s.is_empty()) {
            crate::pgq::query(
                "UPDATE cc_sessions SET claude_session_id = ?, last_activity_at = ?, updated_at = ? WHERE id = ?",
            )
            .bind(csid)
            .bind(&ts)
            .bind(&ts)
            .bind(session_id)
            .execute(self.db())
            .await?;
        } else {
            crate::pgq::query(
                "UPDATE cc_sessions SET last_activity_at = ?, updated_at = ? WHERE id = ?",
            )
            .bind(&ts)
            .bind(&ts)
            .bind(session_id)
            .execute(self.db())
            .await?;
        }

        self.emit(
            "workspace.message",
            session_id,
            json!({"session_id": session_id, "seq": next_seq, "role": input.role, "kind": input.kind}),
        )
        .await?;

        Ok(CcMessage {
            id,
            session_id: session_id.to_string(),
            seq: next_seq,
            role: input.role,
            kind: input.kind,
            content: serde_json::from_str(&content).unwrap_or(Value::Null),
            raw: serde_json::from_str(&raw).unwrap_or(Value::Null),
            tokens_in: input.tokens_in,
            tokens_out: input.tokens_out,
            created_at: ts,
        })
    }

    pub async fn workspace_set_status(&self, id: &str, status: &str) -> Result<()> {
        let ts = now_iso();
        crate::pgq::query("UPDATE cc_sessions SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(&ts)
            .bind(id)
            .execute(self.db())
            .await?;
        self.emit(
            "workspace.status",
            "system",
            json!({"id": id, "status": status}),
        )
        .await?;
        Ok(())
    }

    pub async fn workspace_archive(&self, id: &str) -> Result<()> {
        self.workspace_set_status(id, "archived").await
    }

    /// Hard-delete a session: its transcript, then the graph links stamped with
    /// kind `conversation` on its id (both directions), then the row itself.
    /// Journal mirror entries are history and deliberately stay. Returns whether
    /// a session row was actually removed.
    pub async fn workspace_delete(&self, id: &str) -> Result<bool> {
        crate::pgq::query("DELETE FROM cc_messages WHERE session_id = ?")
            .bind(id)
            .execute(self.db())
            .await?;
        crate::pgq::query(
            "DELETE FROM links WHERE (source_kind = 'conversation' AND source_id = ?) \
             OR (target_kind = 'conversation' AND target_id = ?)",
        )
        .bind(id)
        .bind(id)
        .execute(self.db())
        .await?;
        let deleted = crate::pgq::query("DELETE FROM cc_sessions WHERE id = ?")
            .bind(id)
            .execute(self.db())
            .await?
            .rows_affected()
            > 0;
        if deleted {
            self.emit("workspace.deleted", "system", json!({ "id": id }))
                .await?;
        }
        Ok(deleted)
    }
}
