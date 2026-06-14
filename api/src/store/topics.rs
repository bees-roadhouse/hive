// Topics taxonomy (store.ts `topics`). Owned by core-stores.

use anyhow::Result;
use hive_shared::{slugify, Topic};
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    pub async fn topics_list(&self) -> Result<Vec<Topic>> {
        let rows = sqlx::query("SELECT * FROM topics ORDER BY name")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_topic).collect()
    }

    pub async fn topics_get(&self, topic_id: &str) -> Result<Option<Topic>> {
        let row = sqlx::query("SELECT * FROM topics WHERE id = ?")
            .bind(topic_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_topic).transpose()
    }

    pub async fn topics_by_slug(&self, slug: &str) -> Result<Option<Topic>> {
        let row = sqlx::query("SELECT * FROM topics WHERE slug = ?")
            .bind(slug)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_topic).transpose()
    }

    pub async fn topics_ensure(&self, name: &str) -> Result<Topic> {
        let slug = slugify(name);
        if let Some(existing) = self.topics_by_slug(&slug).await? {
            return Ok(existing);
        }
        let t = Topic {
            id: new_id("top"),
            name: name.to_string(),
            slug,
            created_at: now_iso(),
        };
        sqlx::query("INSERT INTO topics (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
            .bind(&t.id)
            .bind(&t.name)
            .bind(&t.slug)
            .bind(&t.created_at)
            .execute(self.db())
            .await?;
        Ok(t)
    }
}

pub(crate) fn row_to_topic(r: &sqlx::sqlite::SqliteRow) -> Result<Topic> {
    Ok(Topic {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        slug: r.try_get("slug")?,
        created_at: r.try_get("created_at")?,
    })
}
