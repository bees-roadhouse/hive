use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

mod parse;
mod rewrite;

pub use parse::parse;
pub use rewrite::{assign_block_ids, update_task};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedBody {
    pub tasks: Vec<ParsedTask>,
    pub person_refs: Vec<PersonRef>,
    pub tag_refs: Vec<TagRef>,
    /// Universal-mention pipeline: `@slug`, `[[type:slug]]`, `[[slug]]`.
    /// Code spans and fenced code blocks are skipped at the lexer level so
    /// shell commands quoted in journal/note prose don't pollute this list.
    pub entity_mentions: Vec<EntityMention>,
    /// Note spawn blocks: `[[[note title …]]]` … `[[[/note]]]`.
    pub note_spawns: Vec<ParsedNoteSpawn>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedNoteSpawn {
    pub line_index: usize,
    pub title: String,
    pub project: Option<String>,
    pub tags: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedTask {
    pub block_id: Option<String>,
    pub line_index: usize,
    pub text: String,
    pub checked: bool,
    /// Obsidian `- [-]` dropped checkbox (distinct from done).
    pub dropped: bool,
    pub owner: Option<String>,
    pub due: Option<NaiveDate>,
    pub raw_due: Option<String>,
    pub priority: Option<String>,
    pub tags: Vec<String>,
    pub persons: Vec<String>,
    /// Optional `proj:foo` token on the task line (journal projection).
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersonRef {
    pub slug: String,
    pub line_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagRef {
    pub tag: String,
    pub line_index: usize,
}

/// One mention extracted from prose. Locked grammar:
///
/// | Syntax                       | `MentionKind`            |
/// |------------------------------|--------------------------|
/// | `@slug`                      | `Person`                 |
/// | `[[type:identifier]]`        | `Typed(TypedKind)`       |
/// | `[[type:identifier\|alias]]` | `Typed(TypedKind)`       |
/// | `[[slug-or-title]]`          | `Fuzzy`                  |
/// | `[[slug-or-title\|alias]]`   | `Fuzzy`                  |
///
/// `identifier` is either a UUID (the canonical anchor the compose picker
/// writes) or a slug/title (legacy + hand-typed prose). `alias` is the
/// human-readable label captured at write time; the renderer prefers it
/// when present, then falls back to the resolved entity title.
///
/// `#tag` is intentionally NOT a mention ... existing `tag_refs` already
/// covers it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityMention {
    pub kind: MentionKind,
    /// The exact source token, e.g. `@pia`, `[[task:abc-1]]`, `[[home]]`,
    /// `[[task:<uuid>|Fix the build]]`. Retained for `links.note` so the UI
    /// can render the unresolved raw text if needed.
    pub raw: String,
    /// The slug derived from the identifier when the identifier is NOT a
    /// UUID. When the identifier IS a UUID, this still carries a slug-shaped
    /// fallback (we slugify the alias if present, else the UUID's hex form
    /// is unsuitable so we leave a sentinel slug ... see `slug` discussion
    /// in `parse.rs`). The resolver checks `target_id` first.
    pub slug: String,
    /// The UUID anchor when the identifier parsed as a UUID. `None` for
    /// hand-typed `[[type:slug]]` / `[[title]]` prose.
    pub target_id: Option<uuid::Uuid>,
    /// The pipe-delimited alias from the source token, if present. The
    /// renderer prefers this over the resolved entity title.
    pub alias: Option<String>,
    pub line_index: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", tag = "kind", content = "type")]
pub enum MentionKind {
    Person,
    Typed(TypedKind),
    Fuzzy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TypedKind {
    Task,
    Note,
    Event,
    Journal,
}

impl TypedKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TypedKind::Task => "task",
            TypedKind::Note => "note",
            TypedKind::Event => "event",
            TypedKind::Journal => "journal",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskPatch {
    pub checked: Option<bool>,
    pub text: Option<String>,
    pub owner: Option<Option<String>>,
    pub due: Option<Option<NaiveDate>>,
    pub priority: Option<Option<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("block id {0} not found in body")]
    BlockIdNotFound(String),
}
