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
