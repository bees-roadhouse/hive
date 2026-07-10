// Login accounts + onboarding (store.ts `users` + `onboarding`). A user
// authenticates with email + password and writes as their `actor` (a
// people.slug) — the authenticated identity, not a spoofable header.

use anyhow::Result;
use hive_shared::{ActorKind, OnboardingStatus, SafeUser, User, UserRole, APP_VERSION};
use serde_json::json;
use sqlx::Row;

use crate::auth::{hash_password, verify_password};

use super::{new_id, now_iso, Store};

pub struct NewUser {
    pub name: String,
    pub email: String,
    pub password: String,
    pub role: Option<UserRole>,
    pub actor: Option<String>,
    pub kind: Option<ActorKind>,
}

impl Store {
    pub async fn users_count(&self) -> Result<i64> {
        Ok(crate::pgq::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(self.db())
            .await?)
    }

    pub async fn users_list(&self) -> Result<Vec<SafeUser>> {
        let rows =
            crate::pgq::query("SELECT id, actor, email, name, role FROM users ORDER BY created_at")
                .fetch_all(self.db())
                .await?;
        rows.iter()
            .map(|r| {
                Ok(SafeUser {
                    id: r.try_get("id")?,
                    actor: r.try_get("actor")?,
                    email: r.try_get("email")?,
                    name: r.try_get("name")?,
                    role: UserRole::from_str_lossy(r.try_get::<String, _>("role")?.as_str()),
                })
            })
            .collect()
    }

    pub async fn users_by_email(&self, email: &str) -> Result<Option<(User, String)>> {
        let row = crate::pgq::query("SELECT * FROM users WHERE email = ?")
            .bind(email.trim().to_lowercase())
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_user_with_hash).transpose()
    }

    pub async fn users_by_id(&self, uid: &str) -> Result<Option<User>> {
        let row = crate::pgq::query("SELECT * FROM users WHERE id = ?")
            .bind(uid)
            .fetch_optional(self.db())
            .await?;
        row.as_ref()
            .map(|r| row_to_user_with_hash(r).map(|(u, _)| u))
            .transpose()
    }

    pub async fn users_by_actor(&self, actor: &str) -> Result<Option<User>> {
        let row = crate::pgq::query("SELECT * FROM users WHERE actor = ?")
            .bind(actor)
            .fetch_optional(self.db())
            .await?;
        row.as_ref()
            .map(|r| row_to_user_with_hash(r).map(|(u, _)| u))
            .transpose()
    }

    pub async fn users_create(&self, input: NewUser, by: &str) -> Result<SafeUser> {
        // Tie the account to a person row (the actor it writes as).
        let person = self
            .people_ensure(
                input.actor.as_deref().unwrap_or(&input.name),
                input.kind.unwrap_or(ActorKind::Human),
            )
            .await?;
        let user = User {
            id: new_id("usr"),
            actor: person.slug,
            email: input.email.trim().to_lowercase(),
            name: input.name,
            role: input.role.unwrap_or(UserRole::Member),
            created_at: now_iso(),
            last_login_at: None,
        };
        let password_hash = hash_password(&input.password);
        crate::pgq::query(
            "INSERT INTO users (id, actor, email, name, role, password_hash, created_at, last_login_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(&user.id)
        .bind(&user.actor)
        .bind(&user.email)
        .bind(&user.name)
        .bind(user.role.as_str())
        .bind(&password_hash)
        .bind(&user.created_at)
        .execute(self.db())
        .await?;
        self.emit(
            "user.created",
            by,
            json!({"id": user.id, "actor": user.actor, "role": user.role.as_str()}),
        )
        .await?;
        Ok(SafeUser {
            id: user.id,
            actor: user.actor,
            email: user.email,
            name: user.name,
            role: user.role,
        })
    }

    /// Verify credentials; on success stamp last_login_at and return the row.
    pub async fn users_authenticate(&self, email: &str, password: &str) -> Result<Option<User>> {
        let Some((user, hash)) = self.users_by_email(email).await? else {
            return Ok(None);
        };
        if !verify_password(password, &hash) {
            return Ok(None);
        }
        crate::pgq::query("UPDATE users SET last_login_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&user.id)
            .execute(self.db())
            .await?;
        Ok(Some(user))
    }

    // ---- onboarding (first-run setup, v0.1.1) ----

    /// Setup is required for a fresh install (flag false) OR any instance with
    /// no login account yet — the latter keeps a pre-0.1.1 DB from bricking
    /// itself behind a login it can't satisfy.
    pub async fn onboarding_required(&self) -> Result<bool> {
        Ok(!self.config_bool("onboarding.completed").await? || self.users_count().await? == 0)
    }

    pub async fn onboarding_status(&self) -> Result<OnboardingStatus> {
        Ok(OnboardingStatus {
            completed: !self.onboarding_required().await?,
            instance_name: self.config_get("instance.name").await?,
            version: self
                .config_get("app.version")
                .await?
                .unwrap_or_else(|| APP_VERSION.to_string()),
        })
    }

    /// Create the first admin + name the instance, mark setup complete, and
    /// return a session so the wizard logs the admin straight in.
    pub async fn onboarding_complete(
        &self,
        instance_name: &str,
        admin_name: &str,
        admin_email: &str,
        password: &str,
    ) -> Result<(SafeUser, String)> {
        let admin = self
            .users_create(
                NewUser {
                    name: admin_name.to_string(),
                    email: admin_email.to_string(),
                    password: password.to_string(),
                    role: Some(UserRole::Admin),
                    actor: Some(admin_name.to_string()),
                    kind: Some(ActorKind::Human),
                },
                "onboarding",
            )
            .await?;
        self.config_set("instance.name", instance_name).await?;
        self.config_set("app.version", APP_VERSION).await?;
        self.config_set("onboarding.completed", "true").await?;
        let session = self.sessions_create(&admin.id).await?;
        self.emit(
            "onboarding.completed",
            &admin.actor.clone(),
            json!({"instance": instance_name}),
        )
        .await?;
        Ok((admin, session))
    }
}

fn row_to_user_with_hash(r: &sqlx::postgres::PgRow) -> Result<(User, String)> {
    Ok((
        User {
            id: r.try_get("id")?,
            actor: r.try_get("actor")?,
            email: r.try_get("email")?,
            name: r.try_get("name")?,
            role: UserRole::from_str_lossy(r.try_get::<String, _>("role")?.as_str()),
            created_at: r.try_get("created_at")?,
            last_login_at: r.try_get("last_login_at")?,
        },
        r.try_get("password_hash")?,
    ))
}
