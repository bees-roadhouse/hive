// Conversations: full-transcript session logs ingested by an external app
// (claude-code / claude-desktop / openclaude). Namespace-aware exactly like the
// journal — `user_scope` is the owning user (NULL = global), and non-admin reads
// gate on `(user_scope IS NULL OR user_scope = viewer)`; admins (Visibility::All)
// see everything. Reflection maintains the rolling summary and drains the
// `reflected_at IS NULL` queue.

use anyhow::Result;
use hive_shared::{Conversation, ConversationMessage, ConversationView, NewConversationMessage};
use serde_json::json;
use sqlx::Row;

use crate::middleware::Visibility;

use super::{new_id, now_iso, Store};

/// Inputs for the upsert path. The route fills `actor` + `user_scope` from the
/// authenticated ctx (never client params).
#[derive(Debug, Clone, Default)]
pub struct ConversationUpsert {
    pub app: String,
    pub instance: Option<String>,
    pub name: Option<String>,
    pub actor: String,
    pub external_id: Option<String>,
}

impl Store {
    /// Upsert a conversation by (app, external_id). On insert the writer's
    /// namespace is stamped onto `user_scope`; on conflict the existing row's
    /// scope/actor/started_at are preserved (only name/instance refresh).
    /// Returns the conversation id. When `external_id` is None there is no
    /// conflict key, so every call inserts a fresh row.
    pub async fn conversations_upsert(
        &self,
        input: ConversationUpsert,
        user_scope: Option<&str>,
    ) -> Result<String> {
        let ts = now_iso();
        let id = new_id("conv");
        let name = input.name.unwrap_or_default();

        // No external_id → nothing to conflict on; plain insert.
        if input.external_id.is_none() {
            crate::pgq::query(
                "INSERT INTO conversations \
                 (id, app, instance, name, actor, external_id, status, summary, user_scope, \
                  reflected_at, started_at, last_message_at, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, NULL, 'open', '', ?, NULL, ?, NULL, ?, ?)",
            )
            .bind(&id)
            .bind(&input.app)
            .bind(&input.instance)
            .bind(&name)
            .bind(&input.actor)
            .bind(user_scope)
            .bind(&ts)
            .bind(&ts)
            .bind(&ts)
            .execute(self.db())
            .await?;
            self.emit("conversation.created", &input.actor, json!({"id": id}))
                .await?;
            return Ok(id);
        }

        // Idempotent ingest: ON CONFLICT (app, external_id) refresh name/instance
        // and bump updated_at; the original id/user_scope/actor/started_at stick.
        let row = crate::pgq::query(
            "INSERT INTO conversations \
             (id, app, instance, name, actor, external_id, status, summary, user_scope, \
              reflected_at, started_at, last_message_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'open', '', ?, NULL, ?, NULL, ?, ?) \
             ON CONFLICT (app, external_id) DO UPDATE SET \
               name = CASE WHEN excluded.name = '' THEN conversations.name ELSE excluded.name END, \
               instance = COALESCE(excluded.instance, conversations.instance), \
               updated_at = excluded.updated_at \
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(&id)
        .bind(&input.app)
        .bind(&input.instance)
        .bind(&name)
        .bind(&input.actor)
        .bind(&input.external_id)
        .bind(user_scope)
        .bind(&ts)
        .bind(&ts)
        .bind(&ts)
        .fetch_one(self.db())
        .await?;
        let out_id: String = row.try_get("id")?;
        let inserted: bool = row.try_get("inserted")?;
        if inserted {
            self.emit("conversation.created", &input.actor, json!({"id": out_id}))
                .await?;
        }
        Ok(out_id)
    }

    /// Bulk-append turns to a conversation with monotonically increasing seq
    /// (continuing from MAX(seq)), then bump last_message_at + updated_at.
    /// Returns the number appended.
    pub async fn conversation_append_messages(
        &self,
        conversation_id: &str,
        msgs: &[NewConversationMessage],
    ) -> Result<u64> {
        if msgs.is_empty() {
            return Ok(0);
        }
        // Next seq continues from the current max (NULL → start at 0).
        let max_seq: Option<i64> = crate::pgq::query_scalar(
            "SELECT MAX(seq) FROM conversation_messages WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_one(self.db())
        .await?;
        let mut seq = max_seq.map(|s| s + 1).unwrap_or(0);
        let ts = now_iso();
        for m in msgs {
            crate::pgq::query(
                "INSERT INTO conversation_messages (id, conversation_id, seq, role, content, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(new_id("cmsg"))
            .bind(conversation_id)
            .bind(seq)
            .bind(&m.role)
            .bind(&m.content)
            .bind(&ts)
            .execute(self.db())
            .await?;
            seq += 1;
        }
        crate::pgq::query(
            "UPDATE conversations SET last_message_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&ts)
        .bind(&ts)
        .bind(conversation_id)
        .execute(self.db())
        .await?;
        Ok(msgs.len() as u64)
    }

    /// Namespace-scoped list, newest by last_message_at (NULLs last → fall back
    /// to created_at). Admins see all; non-admins see global + own namespace.
    pub async fn conversations_list(
        &self,
        vis: &Visibility,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Conversation>> {
        let gate = scope_gate(vis);
        let sql = format!(
            "SELECT * FROM conversations WHERE {gate} \
             ORDER BY COALESCE(last_message_at, created_at) DESC, created_at DESC \
             LIMIT ? OFFSET ?"
        );
        let mut q = crate::pgq::query(&sql);
        q = bind_gate(q, vis);
        let rows = q.bind(limit).bind(offset).fetch_all(self.db()).await?;
        rows.iter().map(row_to_conversation).collect()
    }

    /// A conversation + its full transcript, namespace-checked (hidden as 404,
    /// mirroring `journal_get`).
    pub async fn conversation_get(
        &self,
        id: &str,
        vis: &Visibility,
    ) -> Result<Option<ConversationView>> {
        let row = crate::pgq::query("SELECT * FROM conversations WHERE id = ?")
            .bind(id)
            .fetch_optional(self.db())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if let Visibility::Namespace(u) = vis {
            let scope: Option<String> = row.try_get("user_scope")?;
            let own_or_global = scope.as_deref().map(|s| s == u).unwrap_or(true);
            if !own_or_global {
                return Ok(None);
            }
        }
        let conversation = row_to_conversation(&row)?;
        let messages = self.conversation_messages(id).await?;
        Ok(Some(ConversationView {
            conversation,
            messages,
        }))
    }

    /// Transcript for a conversation, in seq order.
    async fn conversation_messages(&self, id: &str) -> Result<Vec<ConversationMessage>> {
        let rows = crate::pgq::query(
            "SELECT * FROM conversation_messages WHERE conversation_id = ? ORDER BY seq",
        )
        .bind(id)
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_message).collect()
    }

    /// The reflection queue: conversations not yet reflected, namespace-scoped,
    /// oldest-first (started_at) so reflection drains the backlog in order.
    pub async fn conversations_pending(
        &self,
        vis: &Visibility,
        limit: i64,
    ) -> Result<Vec<Conversation>> {
        let gate = scope_gate(vis);
        let sql = format!(
            "SELECT * FROM conversations WHERE reflected_at IS NULL AND {gate} \
             ORDER BY started_at ASC LIMIT ?"
        );
        let mut q = crate::pgq::query(&sql);
        q = bind_gate(q, vis);
        let rows = q.bind(limit).fetch_all(self.db()).await?;
        rows.iter().map(row_to_conversation).collect()
    }

    /// Mark a conversation reflected: stamp reflected_at = now and store the
    /// rolling summary. Returns the updated conversation (None if absent).
    pub async fn conversation_mark_reflected(
        &self,
        id: &str,
        summary: &str,
    ) -> Result<Option<Conversation>> {
        let ts = now_iso();
        let row = crate::pgq::query(
            "UPDATE conversations SET reflected_at = ?, summary = ?, updated_at = ? \
             WHERE id = ? RETURNING *",
        )
        .bind(&ts)
        .bind(summary)
        .bind(&ts)
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        row.as_ref().map(row_to_conversation).transpose()
    }

    /// Rename a conversation (human-editable friendly name). Returns the updated
    /// conversation (None if absent).
    pub async fn conversation_rename(&self, id: &str, name: &str) -> Result<Option<Conversation>> {
        let ts = now_iso();
        let row = crate::pgq::query(
            "UPDATE conversations SET name = ?, updated_at = ? WHERE id = ? RETURNING *",
        )
        .bind(name)
        .bind(&ts)
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        row.as_ref().map(row_to_conversation).transpose()
    }
}

/// The namespace WHERE fragment: admins (All) see everything; a namespaced
/// viewer sees global (NULL) + own-namespace rows. Mirrors the journal gate.
fn scope_gate(vis: &Visibility) -> &'static str {
    match vis {
        Visibility::All => "TRUE",
        Visibility::Namespace(_) => "(user_scope IS NULL OR user_scope = ?)",
    }
}

/// Bind the placeholder used by `scope_gate` (a no-op for an admin viewer).
fn bind_gate<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    vis: &'q Visibility,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match vis {
        Visibility::All => q,
        Visibility::Namespace(u) => q.bind(u.as_str()),
    }
}

fn row_to_conversation(r: &sqlx::postgres::PgRow) -> Result<Conversation> {
    Ok(Conversation {
        id: r.try_get("id")?,
        app: r.try_get("app")?,
        instance: r.try_get("instance")?,
        name: r.try_get("name")?,
        actor: r.try_get("actor")?,
        external_id: r.try_get("external_id")?,
        status: r.try_get("status")?,
        summary: r.try_get("summary")?,
        user_scope: r.try_get("user_scope")?,
        reflected_at: r.try_get("reflected_at")?,
        started_at: r.try_get("started_at")?,
        last_message_at: r.try_get("last_message_at")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

fn row_to_message(r: &sqlx::postgres::PgRow) -> Result<ConversationMessage> {
    Ok(ConversationMessage {
        id: r.try_get("id")?,
        conversation_id: r.try_get("conversation_id")?,
        seq: r.try_get("seq")?,
        role: r.try_get("role")?,
        content: r.try_get("content")?,
        created_at: r.try_get("created_at")?,
    })
}
