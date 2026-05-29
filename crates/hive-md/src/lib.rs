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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParsedTask {
    pub block_id: Option<String>,
    pub line_index: usize,
    pub text: String,
    pub checked: bool,
    pub owner: Option<String>,
    pub due: Option<NaiveDate>,
    pub raw_due: Option<String>,
    pub priority: Option<String>,
    pub tags: Vec<String>,
    pub persons: Vec<String>,
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
/// | Syntax           | `MentionKind`            |
/// |------------------|--------------------------|
/// | `@slug`          | `Person`                 |
/// | `[[type:slug]]`  | `Typed(TypedKind)`       |
/// | `[[slug]]`       | `Fuzzy`                  |
///
/// `#tag` is intentionally NOT a mention ... existing `tag_refs` already
/// covers it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityMention {
    pub kind: MentionKind,
    /// The exact source token, e.g. `@pia`, `[[task:abc-1]]`, `[[home]]`.
    /// Retained for `links.note` so the UI can render the unresolved raw text
    /// if needed.
    pub raw: String,
    pub slug: String,
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
