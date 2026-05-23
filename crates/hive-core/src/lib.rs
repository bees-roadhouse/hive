//! hive-core: shared DTOs for the hive HTTP API.
//!
//! Wire-format types that travel between hive-api and any client
//! (hive-ui leptos canvas, iPad/Swift via uniffi later). Pure data,
//! no database access, no platform code. Add types here as endpoints
//! get wired.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: Uuid,
    pub ai: String,
    pub entry_date: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub created_at: String,
}
