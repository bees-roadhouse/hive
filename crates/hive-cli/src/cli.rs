//! Clap structure mirroring `~/.hive/hive.py`'s argparse grammar.
//!
//! Names, defaults, and help text are kept close to the python so callers
//! that read `--help` see the same shape. Closed-set fields (owner, ai,
//! author, severity, statuses) are validated CLI-side against the same valid
//! sets python uses, so a typo fails fast before any HTTP round-trip; the API
//! is still the source of truth and re-validates server-side.
//!
//! Entity ids are taken as `String` and passed through to the API path
//! verbatim. The canonical schema uses UUIDv7, but a deployed server may still
//! be on legacy integer PKs ... not parsing the id here keeps the CLI working
//! against both, and the server validates/404s an unknown id anyway.

use clap::{Args, Parser, Subcommand};

// Valid sets ... mirror hive_db::enums (kept in lockstep with the API).
const OWNERS: &[&str] = &["pia", "apis", "cera", "nate", "maggie"];
const AIS: &[&str] = &["pia", "apis", "cera", "nate"];
const AUTHORS: &[&str] = &["pia", "apis", "cera", "nate", "maggie"];
const SEVERITIES: &[&str] = &["critical", "high", "medium", "low", "info"];
const PROJECT_STATUSES: &[&str] = &["active", "paused", "archived"];
const TASK_STATUSES: &[&str] = &["open", "in_progress", "blocked", "done", "dropped"];

#[derive(Debug, Parser)]
#[command(
    name = "hive",
    about = "Hive shared-state helper (HTTP client for hive-api) for Pia / Apis / Cera",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Top,
}

#[derive(Debug, Subcommand)]
pub enum Top {
    /// Check connectivity to hive-api (GET /healthz)
    Init,

    /// Authenticate to hive-api and cache an access token (password flow)
    Login(LoginArgs),

    /// Clear the cached access token
    Logout,

    /// Task tracking
    Tasks {
        #[command(subcommand)]
        cmd: TasksCmd,
    },

    /// Chronological per-AI memory
    Journal {
        #[command(subcommand)]
        cmd: JournalCmd,
    },

    /// Free-form notes
    Notes {
        #[command(subcommand)]
        cmd: NotesCmd,
    },

    /// watch-the-wire event cache
    Wire {
        #[command(subcommand)]
        cmd: WireCmd,
    },

    /// Cross-domain relations between hive entities
    Links {
        #[command(subcommand)]
        cmd: LinksCmd,
    },

    /// Dump a tag-hub knowledge graph as JSON (consumed by hive-ui /graph)
    Graph(GraphArgs),

    /// Full-text search across journal + notes (add --hybrid for vector + rerank)
    Search(SearchArgs),
}

// ---------- login ----------

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Username to authenticate as (password read from HIVE_PASSWORD, or
    /// prompted interactively when a TTY is available).
    #[arg(long)]
    pub username: Option<String>,
    /// Scopes to request (space-delimited). The AS intersects these with what
    /// the user is granted; omit to let the server decide.
    #[arg(long)]
    pub scope: Option<String>,
    /// Use the RFC 8628 device-authorization grant instead of the password
    /// flow. NOT YET IMPLEMENTED (Phase 5) — the flag reserves the surface.
    #[arg(long)]
    pub device: bool,
}

// ---------- tasks ----------

#[derive(Debug, Subcommand)]
pub enum TasksCmd {
    /// Project management
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Add a task
    Add(TaskAddArgs),
    /// List tasks (default: open + in_progress)
    List(TaskListArgs),
    /// Show one task
    Show { id: String },
    /// Update task fields
    Update(TaskUpdateArgs),
    /// Mark task done
    Done { id: String },
    /// Mark task blocked
    Block {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Mark task dropped (cancelled)
    Drop { id: String },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    /// Add a project
    Add(ProjectAddArgs),
    /// List projects
    List {
        #[arg(long, value_parser = parse_project_status)]
        status: Option<String>,
    },
    /// Archive a project
    Archive { name: String },
}

#[derive(Debug, Args)]
pub struct ProjectAddArgs {
    pub name: String,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, value_parser = parse_owner)]
    pub owner: String,
}

#[derive(Debug, Args)]
pub struct TaskAddArgs {
    #[arg(long)]
    pub project: String,
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long, value_parser = parse_owner)]
    pub owner: String,
    #[arg(long)]
    pub priority: Option<String>,
    #[arg(long)]
    pub due: Option<String>,
    /// Link to another entity: <table>:<id>[:<link_type>] (repeatable)
    #[arg(long, action = clap::ArgAction::Append)]
    pub link: Vec<String>,
}

#[derive(Debug, Args)]
pub struct TaskListArgs {
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long, value_parser = parse_owner)]
    pub owner: Option<String>,
    #[arg(long, value_parser = parse_task_status)]
    pub status: Option<String>,
    /// Include closed/dropped
    #[arg(long)]
    pub all: bool,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TaskUpdateArgs {
    pub id: String,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub priority: Option<String>,
    #[arg(long)]
    pub owner: Option<String>,
    #[arg(long)]
    pub due: Option<String>,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
}

// ---------- journal ----------

#[derive(Debug, Subcommand)]
pub enum JournalCmd {
    Add(JournalAddArgs),
    List(JournalListArgs),
    Show { id: String },
    Search(JournalSearchArgs),
}

#[derive(Debug, Args)]
pub struct JournalAddArgs {
    #[arg(long, value_parser = parse_ai)]
    pub ai: String,
    /// YYYY-MM-DD (default: today)
    #[arg(long)]
    pub date: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub body: String,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub link: Vec<String>,
}

#[derive(Debug, Args)]
pub struct JournalListArgs {
    #[arg(long, value_parser = parse_ai)]
    pub ai: Option<String>,
    #[arg(long = "from")]
    pub from_date: Option<String>,
    #[arg(long = "to")]
    pub to_date: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct JournalSearchArgs {
    pub query: String,
    #[arg(long, default_value_t = 20)]
    pub limit: i64,
    /// Filter to one writer (only with --hybrid)
    #[arg(long, value_parser = parse_ai)]
    pub ai: Option<String>,
    /// Run FTS + vector + cross-encoder rerank via hive-embed
    #[arg(long)]
    pub hybrid: bool,
}

// ---------- notes ----------

#[derive(Debug, Subcommand)]
pub enum NotesCmd {
    Add(NotesAddArgs),
    List(NotesListArgs),
    Show { id: String },
    Search(NotesSearchArgs),
}

#[derive(Debug, Args)]
pub struct NotesAddArgs {
    #[arg(long, value_parser = parse_author)]
    pub author: String,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub body: String,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub link: Vec<String>,
}

#[derive(Debug, Args)]
pub struct NotesListArgs {
    #[arg(long, value_parser = parse_author)]
    pub author: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub limit: Option<i64>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct NotesSearchArgs {
    pub query: String,
    #[arg(long, default_value_t = 20)]
    pub limit: i64,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long, value_parser = parse_author)]
    pub author: Option<String>,
    #[arg(long)]
    pub hybrid: bool,
}

// ---------- wire ----------

#[derive(Debug, Subcommand)]
pub enum WireCmd {
    Add(WireAddArgs),
    List(WireListArgs),
    Ack { id: String },
}

#[derive(Debug, Args)]
pub struct WireAddArgs {
    #[arg(long)]
    pub source: String,
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long = "external-id")]
    pub external_id: Option<String>,
    #[arg(long, value_parser = parse_severity)]
    pub severity: Option<String>,
    #[arg(long)]
    pub affects: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub category: Option<String>,
}

#[derive(Debug, Args)]
pub struct WireListArgs {
    #[arg(long)]
    pub source: Option<String>,
    #[arg(long, value_parser = parse_severity)]
    pub severity: Option<String>,
    #[arg(long)]
    pub unacknowledged: bool,
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
    #[arg(long)]
    pub json: bool,
}

// ---------- links ----------

#[derive(Debug, Subcommand)]
pub enum LinksCmd {
    /// Add a link from source to target
    Add {
        #[arg(long)]
        source: String,
        #[arg(long)]
        target: String,
        #[arg(long = "type")]
        link_type: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// List outgoing links from an entity
    List {
        #[arg(long)]
        source: String,
    },
    /// List incoming links to an entity
    Incoming {
        #[arg(long)]
        target: String,
    },
    /// Show both outgoing and incoming links for an entity
    Show {
        #[arg(value_name = "REF")]
        reference: String,
    },
    /// Delete a link by id
    Remove { id: String },
    /// List distinct link_type values in use
    Types,
}

// ---------- graph + search ----------

#[derive(Debug, Args)]
pub struct GraphArgs {
    /// Minimum tag count to include as a hub
    #[arg(long, default_value_t = 2)]
    pub min: i64,
    /// Maximum number of tag hubs
    #[arg(long, default_value_t = 80)]
    pub tags: i64,
    /// Hard cap on total node count
    #[arg(long, default_value_t = 600)]
    pub nodes: i64,
    /// Include meta-tags (legacy-migration, *-authored)
    #[arg(long = "include-meta")]
    pub include_meta: bool,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long, default_value_t = 10)]
    pub limit: i64,
    #[arg(long)]
    pub hybrid: bool,
}

// ---------- value parsers ----------

fn validate(field: &str, s: &str, valid: &[&str]) -> Result<String, String> {
    if valid.contains(&s) {
        Ok(s.to_string())
    } else {
        let mut sorted = valid.to_vec();
        sorted.sort_unstable();
        Err(format!(
            "invalid {field} '{s}'. valid: {}",
            sorted.join(", ")
        ))
    }
}

fn parse_owner(s: &str) -> Result<String, String> {
    validate("owner", s, OWNERS)
}
fn parse_ai(s: &str) -> Result<String, String> {
    validate("ai", s, AIS)
}
fn parse_author(s: &str) -> Result<String, String> {
    validate("author", s, AUTHORS)
}
fn parse_severity(s: &str) -> Result<String, String> {
    validate("severity", s, SEVERITIES)
}
fn parse_project_status(s: &str) -> Result<String, String> {
    validate("status", s, PROJECT_STATUSES)
}
fn parse_task_status(s: &str) -> Result<String, String> {
    validate("status", s, TASK_STATUSES)
}
