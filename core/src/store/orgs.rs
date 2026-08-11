// Orgs, memberships, and provider identities — the tenancy plane.
//
// None of these tables is row-level-secured, because resolving "which org is
// this session acting in" has to happen before an acting org exists. They hold
// no content: an org is a name, a membership is an edge, an identity is an
// `(issuer, subject)` pair. Everything else in the schema is content and is
// behind a policy.

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use crate::db::{DEFAULT_ORG_ID, DEFAULT_ORG_SLUG};

use super::{new_id, now_iso, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Org {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
}

/// A user's place in one org: the org, and their role inside it. Role is flat
/// (`admin` | `member`) — `docs/WEB-APP.md` leaves per-resource roles open, and
/// flat is right until it very suddenly is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub org: Org,
    pub role: String,
}

impl Membership {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

impl Store {
    pub async fn orgs_create(&self, slug: &str, name: &str) -> Result<Org> {
        let org = Org {
            id: Uuid::new_v4(),
            slug: slug.trim().to_lowercase(),
            name: name.trim().to_string(),
        };
        crate::pgq::query("INSERT INTO orgs (id, slug, name) VALUES (?, ?, ?)")
            .bind(org.id)
            .bind(&org.slug)
            .bind(&org.name)
            .execute(self.db())
            .await?;
        Ok(org)
    }

    pub async fn orgs_by_slug(&self, slug: &str) -> Result<Option<Org>> {
        let row = crate::pgq::query("SELECT id, slug, name FROM orgs WHERE slug = ?")
            .bind(slug.trim().to_lowercase())
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_org).transpose()
    }

    /// The single-tenant org an upgraded install folded into, created by
    /// `apply_org_scoping` and never absent on a migrated database.
    pub async fn orgs_default(&self) -> Result<Org> {
        let row = crate::pgq::query("SELECT id, slug, name FROM orgs WHERE id = ?")
            .bind(DEFAULT_ORG_ID)
            .fetch_optional(self.db())
            .await?;
        match row.as_ref().map(row_to_org).transpose()? {
            Some(org) => Ok(org),
            None => Ok(Org {
                id: DEFAULT_ORG_ID,
                slug: DEFAULT_ORG_SLUG.to_string(),
                name: "Default".to_string(),
            }),
        }
    }

    pub async fn memberships_add(&self, user_id: &str, org: Uuid, role: &str) -> Result<()> {
        crate::pgq::query(
            "INSERT INTO memberships (user_id, org_id, role, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (user_id, org_id) DO UPDATE SET role = excluded.role",
        )
        .bind(user_id)
        .bind(org)
        .bind(role)
        .bind(now_iso())
        .execute(self.db())
        .await?;
        Ok(())
    }

    /// Add a membership in whatever org the caller is ACTING in, read from the
    /// same session variable every content row's `org_id` default reads. One
    /// source of truth for "which org am I in", so a caller cannot name one
    /// org while writing into another. No acting org means no membership: the
    /// insert fails on `org_id NOT NULL` rather than guessing.
    pub async fn memberships_add_acting(&self, user_id: &str, role: &str) -> Result<()> {
        crate::pgq::query(
            "INSERT INTO memberships (user_id, org_id, role, created_at) \
             VALUES (?, nullif(current_setting('hive.acting_org', true), '')::uuid, ?, ?) \
             ON CONFLICT (user_id, org_id) DO UPDATE SET role = excluded.role",
        )
        .bind(user_id)
        .bind(role)
        .bind(now_iso())
        .execute(self.db())
        .await?;
        Ok(())
    }

    /// Every org a user belongs to, oldest first. The list a human picks from
    /// when starting a session; never a set a single session acts across.
    pub async fn memberships_for(&self, user_id: &str) -> Result<Vec<Membership>> {
        let rows = crate::pgq::query(
            "SELECT o.id, o.slug, o.name, m.role FROM memberships m \
             JOIN orgs o ON o.id = m.org_id WHERE m.user_id = ? ORDER BY m.created_at, o.slug",
        )
        .bind(user_id)
        .fetch_all(self.db())
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Membership {
                    org: row_to_org(r)?,
                    role: r.try_get("role")?,
                })
            })
            .collect()
    }

    /// The membership a session may act under, or None if the user is not in
    /// that org. Checked ONCE, when the session is minted; RLS enforces it on
    /// every statement afterwards without the application re-deriving anything.
    pub async fn membership_of(&self, user_id: &str, org: Uuid) -> Result<Option<Membership>> {
        Ok(self
            .memberships_for(user_id)
            .await?
            .into_iter()
            .find(|m| m.org.id == org))
    }

    // ---- provider identities ----

    /// Resolve an OIDC login to a local user by `(issuer, subject)`.
    ///
    /// This is the whole join. Email is NOT a key here and must never become
    /// one: `email_verified` is an assertion by an IdP we do not control, and
    /// the classic takeover is registering the victim's address at a trusted
    /// provider and letting a match on it do the rest.
    pub async fn identity_user(&self, issuer: &str, subject: &str) -> Result<Option<String>> {
        Ok(crate::pgq::query_scalar::<String>(
            "SELECT user_id FROM user_identities WHERE issuer = ? AND subject = ?",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(self.db())
        .await?)
    }

    /// Bind a provider identity to a local user. `UNIQUE (issuer, subject)`
    /// makes a second binding of the same provider account fail rather than
    /// quietly move it: one provider account, one local identity, always.
    pub async fn identity_link(&self, user_id: &str, issuer: &str, subject: &str) -> Result<()> {
        crate::pgq::query(
            "INSERT INTO user_identities (id, user_id, issuer, subject, created_at) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (issuer, subject) DO NOTHING",
        )
        .bind(new_id("uid"))
        .bind(user_id)
        .bind(issuer)
        .bind(subject)
        .bind(now_iso())
        .execute(self.db())
        .await?;
        Ok(())
    }
}

fn row_to_org(r: &sqlx::postgres::PgRow) -> Result<Org> {
    Ok(Org {
        id: r.try_get("id")?,
        slug: r.try_get("slug")?,
        name: r.try_get("name")?,
    })
}
