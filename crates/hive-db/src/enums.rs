//! Closed-set enums validated at parse time.
//!
//! Postgres has no native enum constraint here (we store everything as
//! `TEXT` for forward-compat); the python toolchain validates at the CLI
//! boundary (`validate_owner`, `validate_ai`, etc.). Mirror that in rust
//! so the types layer can't hold an invalid value.
//!
//! For sqlx binding: use `.as_str()` when you need to `.bind(...)`. We
//! don't implement `sqlx::Encode<Postgres>` directly because the queries
//! crate prefers explicit `.as_str()` and the enum types are Copy.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $field:literal, { $( $variant:ident => $s:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $( $variant ),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            pub fn as_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $s ),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $s => Ok($name::$variant), )+
                    other => {
                        let mut valid: Vec<&'static str> = Self::ALL.iter().map(|v| v.as_str()).collect();
                        valid.sort();
                        Err(Error::InvalidEnum {
                            field: $field,
                            value: other.to_string(),
                            valid: valid.join(", "),
                        })
                    }
                }
            }
        }
    };
}

str_enum!(
    /// Task / note / journal owner.
    Owner, "owner", {
        Pia => "pia",
        Apis => "apis",
        Cera => "cera",
        Nate => "nate",
        Maggie => "maggie",
    }
);

str_enum!(
    /// Journal entry author.
    Ai, "ai", {
        Pia => "pia",
        Apis => "apis",
        Cera => "cera",
        Nate => "nate",
    }
);

str_enum!(
    /// Note author.
    Author, "author", {
        Pia => "pia",
        Apis => "apis",
        Cera => "cera",
        Nate => "nate",
        Maggie => "maggie",
    }
);

str_enum!(
    /// Wire-event severity.
    Severity, "severity", {
        Critical => "critical",
        High => "high",
        Medium => "medium",
        Low => "low",
        Info => "info",
    }
);

str_enum!(
    /// Project lifecycle status.
    ProjectStatus, "status", {
        Active => "active",
        Paused => "paused",
        Archived => "archived",
    }
);

str_enum!(
    /// Task lifecycle status.
    TaskStatus, "status", {
        Open => "open",
        InProgress => "in_progress",
        Blocked => "blocked",
        Done => "done",
        Dropped => "dropped",
    }
);

impl TaskStatus {
    pub fn is_active(self) -> bool {
        matches!(self, TaskStatus::Open | TaskStatus::InProgress)
    }

    pub fn is_closed(self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Dropped)
    }
}

/// The set of tables that can appear on either side of a `links` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkTable {
    Tasks,
    JournalEntries,
    Notes,
    WireEvents,
    Projects,
}

impl LinkTable {
    pub const ALL: &'static [LinkTable] = &[
        LinkTable::Tasks,
        LinkTable::JournalEntries,
        LinkTable::Notes,
        LinkTable::WireEvents,
        LinkTable::Projects,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            LinkTable::Tasks => "tasks",
            LinkTable::JournalEntries => "journal_entries",
            LinkTable::Notes => "notes",
            LinkTable::WireEvents => "wire_events",
            LinkTable::Projects => "projects",
        }
    }

    /// CLI shorthand: link specs accept `tasks` / `journal` / `notes` / `wire` / `projects`.
    pub fn parse_short(s: &str) -> Result<Self, Error> {
        match s {
            "tasks" => Ok(LinkTable::Tasks),
            "journal" | "journal_entries" => Ok(LinkTable::JournalEntries),
            "notes" => Ok(LinkTable::Notes),
            "wire" | "wire_events" => Ok(LinkTable::WireEvents),
            "projects" => Ok(LinkTable::Projects),
            other => {
                let mut valid: Vec<&'static str> = Self::ALL.iter().map(|t| t.as_str()).collect();
                valid.sort();
                Err(Error::InvalidEnum {
                    field: "table",
                    value: other.to_string(),
                    valid: valid.join(", "),
                })
            }
        }
    }
}

impl fmt::Display for LinkTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LinkTable {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_short(s)
    }
}
