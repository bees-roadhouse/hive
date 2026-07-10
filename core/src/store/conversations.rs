// Conversation capture — SessionEnd ingest of LOCAL agent sessions onto the
// SAME cc_sessions/cc_messages tables the hosted workspaces use (main already
// presents cc_sessions as "Conversations"). origin='captured' marks these
// rows and every function here is scoped to them:
//
// - status is 'captured', never 'provisioning' — the runner claim loop polls
//   for provisioning workspaces, so captured rows can't be claimed/driven.
// - (runtime, claude_session_id) is the idempotent capture key (partial
//   unique index cc_sessions_captured_ext): a resumed local session re-fires
//   SessionEnd with the same session id and the FULL transcript, hence the
//   replace write mode.
// - reflected_at is the reflection cursor: NULL = queued; any transcript
//   write clears it (re-queues).
//
// Unlike hosted ingest (routes/workspaces.rs), NOTHING here journal-mirrors:
// reflection summarizes captured transcripts into the journal later, and
// double-writing would flood it with every turn of every local session.

use anyhow::Result;
use hive_shared::{
    Conversation, ConversationMessageFlat, ConversationView, NewCapturedConversation,
    NewConversationMessage,
};
use serde_json::{json, Value};

use super::{new_id, now_iso, Store};
use crate::Visibility;

const CONV_COLS: &str = "id, owner, created_by, title, runtime, origin, status, \
    claude_session_id, summary, reflected_at, created_at, updated_at, last_activity_at";

#[derive(sqlx::FromRow)]
struct ConvRow {
    id: String,
    owner: String,
    created_by: String,
    title: String,
    runtime: String,
    origin: String,
    status: String,
    claude_session_id: Option<String>,
    summary: String,
    reflected_at: Option<String>,
    created_at: String,
    updated_at: String,
    last_activity_at: Option<String>,
}

impl ConvRow {
    fn into_view(self) -> Conversation {
        Conversation {
            id: self.id,
            owner: self.owner,
            created_by: self.created_by,
            title: self.title,
            runtime: self.runtime,
            origin: self.origin,
            status: self.status,
            claude_session_id: self.claude_session_id,
            summary: self.summary,
            reflected_at: self.reflected_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_activity_at: self.last_activity_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct FlatMsgRow {
    id: String,
    seq: i64,
    role: String,
    kind: String,
    content: String,
    created_at: String,
}

impl FlatMsgRow {
    fn into_view(self) -> ConversationMessageFlat {
        let content = serde_json::from_str::<Value>(&self.content)
            .map(|v| flatten_content(&v))
            .unwrap_or(self.content);
        ConversationMessageFlat {
            id: self.id,
            seq: self.seq,
            role: self.role,
            kind: self.kind,
            content,
            created_at: self.created_at,
        }
    }
}

fn visible(vis: &Visibility, owner: &str) -> bool {
    match vis {
        Visibility::All => true,
        Visibility::Namespace(v) => v == owner,
    }
}

/// Flatten a stored message payload to plain text for the reflector: hosted-
/// shape objects yield their text-ish field, bare strings pass through, and
/// anything else serializes compactly (empty object → empty string).
fn flatten_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Object(m) => {
            for key in ["text", "result", "note", "error"] {
                if let Some(s) = m.get(key).and_then(Value::as_str) {
                    return s.to_string();
                }
            }
            if m.is_empty() {
                String::new()
            } else {
                content.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Normalize an incoming content payload to the stored hosted shape: a bare
/// string becomes {"text": …} (so captured and hosted transcripts render the
/// same), null becomes {}.
fn content_json(content: &Value) -> String {
    match content {
        Value::Null => "{}".to_string(),
        Value::String(s) => json!({ "text": s }).to_string(),
        other => other.to_string(),
    }
}

fn raw_json(raw: &Value) -> String {
    if raw.is_null() {
        "{}".to_string()
    } else {
        raw.to_string()
    }
}

impl Store {
    /// Idempotent capture upsert keyed on (runtime, claude_session_id) for
    /// origin='captured'. Inserts with status='captured' (the runner claim
    /// loop only picks up 'provisioning'; a captured row must never be
    /// claimed). On conflict the original owner/created_at stick and only a
    /// non-empty title/summary refresh. Returns Some(id); None when the
    /// capture key already belongs to a DIFFERENT owner — a session id is not
    /// a bearer credential for someone else's transcript (caller answers
    /// forbidden).
    pub async fn conversation_upsert_captured(
        &self,
        owner: &str,
        created_by: &str,
        input: NewCapturedConversation,
    ) -> Result<Option<String>> {
        anyhow::ensure!(!input.external_id.trim().is_empty(), "external_id required");
        let runtime = super::workspaces::normalize_runtime(input.runtime.as_deref());
        let id = new_id("ccs");
        let ts = now_iso();
        let title = input.title.unwrap_or_default();
        let summary = input.summary.unwrap_or_default();
        let meta = json!({
            "kind": "conversation",
            "origin": "captured",
            "runtime": &runtime,
        })
        .to_string();
        // The ON CONFLICT arbiter names the partial index's predicate; the DO
        // UPDATE is additionally gated to the same owner, so a foreign-owner
        // collision updates zero rows → no RETURNING row → Ok(None).
        let row = crate::pgq::query_as::<ConvRow>(&format!(
            "INSERT INTO cc_sessions \
             (id, owner, created_by, title, workdir, claude_session_id, runtime, origin, \
              status, usage, meta, summary, created_at, updated_at) \
             VALUES (?, ?, ?, ?, '', ?, ?, 'captured', 'captured', '{{}}', ?, ?, ?, ?) \
             ON CONFLICT (runtime, claude_session_id) WHERE origin = 'captured' DO UPDATE SET \
               title = CASE WHEN excluded.title = '' THEN cc_sessions.title ELSE excluded.title END, \
               summary = CASE WHEN excluded.summary = '' THEN cc_sessions.summary ELSE excluded.summary END, \
               updated_at = excluded.updated_at \
             WHERE cc_sessions.owner = excluded.owner \
             RETURNING {CONV_COLS}"
        ))
        .bind(&id)
        .bind(owner)
        .bind(created_by)
        .bind(&title)
        .bind(input.external_id.trim())
        .bind(&runtime)
        .bind(&meta)
        .bind(&summary)
        .bind(&ts)
        .bind(&ts)
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        // Fresh insert (the generated id survived) → announce the capture.
        if row.id == id {
            self.emit(
                "conversation.captured",
                created_by,
                json!({"id": row.id, "owner": owner, "runtime": runtime}),
            )
            .await?;
        }
        Ok(Some(row.id))
    }

    /// Get a captured conversation by id WITHOUT a visibility check (the
    /// routes' owner-or-admin write gate reads `owner` off this). Hosted rows
    /// are not part of the conversations ingest surface and return None.
    pub async fn conversation_get_captured(&self, id: &str) -> Result<Option<Conversation>> {
        let row = crate::pgq::query_as::<ConvRow>(&format!(
            "SELECT {CONV_COLS} FROM cc_sessions WHERE id = ? AND origin = 'captured'"
        ))
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        Ok(row.map(ConvRow::into_view))
    }

    /// Write transcript turns for a captured conversation. `replace` swaps the
    /// whole stored transcript transactionally (delete + reinsert — a resumed
    /// local session re-fires SessionEnd with the FULL transcript; appending
    /// would duplicate every turn); otherwise turns append after the current
    /// max seq. Either way the write clears reflected_at (re-queues the
    /// conversation for reflection) and bumps activity. Deliberately NO
    /// journal mirroring — see the module doc.
    pub async fn conversation_replace_messages(
        &self,
        session_id: &str,
        msgs: &[NewConversationMessage],
        replace: bool,
    ) -> Result<u64> {
        let ts = now_iso();
        let mut tx = self.db().begin().await?;
        let mut seq: i64 = if replace {
            crate::pgq::query("DELETE FROM cc_messages WHERE session_id = ?")
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            0
        } else {
            crate::pgq::query_scalar::<i64>(
                "SELECT COALESCE(MAX(seq), 0) FROM cc_messages WHERE session_id = ?",
            )
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await?
        };
        for m in msgs {
            seq += 1;
            crate::pgq::query(
                "INSERT INTO cc_messages \
                 (id, session_id, seq, role, kind, content, raw, tokens_in, tokens_out, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(new_id("ccm"))
            .bind(session_id)
            .bind(seq)
            .bind(&m.role)
            .bind(&m.kind)
            .bind(content_json(&m.content))
            .bind(raw_json(&m.raw))
            .bind(m.tokens_in)
            .bind(m.tokens_out)
            .bind(&ts)
            .execute(&mut *tx)
            .await?;
        }
        crate::pgq::query(
            "UPDATE cc_sessions SET reflected_at = NULL, last_activity_at = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(&ts)
        .bind(&ts)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(msgs.len() as u64)
    }

    /// The reflection queue: captured conversations not yet reflected, oldest
    /// first so reflection drains the backlog in order. Namespace viewers see
    /// their own; admins see all.
    pub async fn conversations_pending(
        &self,
        vis: &Visibility,
        limit: i64,
    ) -> Result<Vec<Conversation>> {
        let rows = match vis {
            Visibility::All => {
                crate::pgq::query_as::<ConvRow>(&format!(
                    "SELECT {CONV_COLS} FROM cc_sessions \
                     WHERE origin = 'captured' AND reflected_at IS NULL \
                     ORDER BY created_at ASC LIMIT ?"
                ))
                .bind(limit)
                .fetch_all(self.db())
                .await?
            }
            Visibility::Namespace(viewer) => {
                crate::pgq::query_as::<ConvRow>(&format!(
                    "SELECT {CONV_COLS} FROM cc_sessions \
                     WHERE origin = 'captured' AND reflected_at IS NULL AND owner = ? \
                     ORDER BY created_at ASC LIMIT ?"
                ))
                .bind(viewer)
                .bind(limit)
                .fetch_all(self.db())
                .await?
            }
        };
        Ok(rows.into_iter().map(ConvRow::into_view).collect())
    }

    /// A captured conversation + its transcript with content flattened to
    /// plain text (the reflector consumes content as a string). Visibility-
    /// gated (owner-or-admin); hidden rows answer None (a 404 upstream).
    pub async fn conversation_get_flat(
        &self,
        vis: &Visibility,
        id: &str,
    ) -> Result<Option<ConversationView>> {
        let Some(conversation) = self.conversation_get_captured(id).await? else {
            return Ok(None);
        };
        if !visible(vis, &conversation.owner) {
            return Ok(None);
        }
        let rows = crate::pgq::query_as::<FlatMsgRow>(
            "SELECT id, seq, role, kind, content, created_at FROM cc_messages \
             WHERE session_id = ? ORDER BY seq ASC",
        )
        .bind(id)
        .fetch_all(self.db())
        .await?;
        Ok(Some(ConversationView {
            conversation,
            messages: rows.into_iter().map(FlatMsgRow::into_view).collect(),
        }))
    }

    /// Stamp the reflection cursor (reflected_at = now) and, when supplied,
    /// the rolling summary. Returns the updated conversation (None if absent
    /// or not a captured row).
    pub async fn conversation_mark_reflected(
        &self,
        id: &str,
        summary: Option<&str>,
    ) -> Result<Option<Conversation>> {
        let ts = now_iso();
        let row = crate::pgq::query_as::<ConvRow>(&format!(
            "UPDATE cc_sessions SET reflected_at = ?, summary = COALESCE(?, summary), updated_at = ? \
             WHERE id = ? AND origin = 'captured' RETURNING {CONV_COLS}"
        ))
        .bind(&ts)
        .bind(summary)
        .bind(&ts)
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        Ok(row.map(ConvRow::into_view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_hosted_and_bare_content_shapes() {
        assert_eq!(flatten_content(&json!({"text": "hi"})), "hi");
        assert_eq!(flatten_content(&json!("plain")), "plain");
        assert_eq!(flatten_content(&json!({"error": "boom"})), "boom");
        assert_eq!(flatten_content(&json!({})), "");
        assert_eq!(flatten_content(&Value::Null), "");
        assert_eq!(
            flatten_content(&json!({"tool": "Bash"})),
            "{\"tool\":\"Bash\"}"
        );
    }

    #[test]
    fn stores_bare_strings_as_hosted_text_shape() {
        assert_eq!(content_json(&json!("hi")), "{\"text\":\"hi\"}");
        assert_eq!(content_json(&Value::Null), "{}");
        assert_eq!(content_json(&json!({"text": "hi"})), "{\"text\":\"hi\"}");
    }
}
