//! Clap structure mirroring `~/.hive/hive.py`'s argparse grammar.
//!
//! Names, defaults, and help text are kept close to the python so callers
//! that read `--help` see the same shape.

use clap::{Args, Parser, Subcommand};

use hive_db::enums::{Ai, Author, Owner, ProjectStatus, Severity, TaskStatus};

#[derive(Debug, Parser)]
#[command(
    name = "hive",
    about = "Hive shared-state helper (sqlite) for Pia / Apis / Cera",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Top,
}

#[derive(Debug, Subcommand)]
pub enum Top {
    /// Create the database
    Init,

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

    /// FTS5 search across journal + notes (add --hybrid for vector + rerank)
    Search(SearchArgs),
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
    Show {
        id: i64,
    },
    /// Update task fields
    Update(TaskUpdateArgs),
    /// Mark task done
    Done {
        id: i64,
    },
    /// Mark task blocked
    Block {
        id: i64,
        #[arg(long)]
        reason: String,
    },
    /// Mark task dropped (cancelled)
    Drop {
        id: i64,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    /// Add a project
    Add(ProjectAddArgs),
    /// List projects
    List {
        #[arg(long, value_parser = parse_project_status)]
        status: Option<ProjectStatus>,
    },
    /// Archive a project
    Archive {
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct ProjectAddArgs {
    pub name: String,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, value_parser = parse_owner)]
    pub owner: Owner,
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
    pub owner: Owner,
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
    pub owner: Option<Owner>,
    #[arg(long, value_parser = parse_task_status)]
    pub status: Option<TaskStatus>,
    /// Include closed/dropped
    #[arg(long)]
    pub all: bool,
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TaskUpdateArgs {
    pub id: i64,
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
    Show { id: i64 },
    Search(JournalSearchArgs),
}

#[derive(Debug, Args)]
pub struct JournalAddArgs {
    #[arg(long, value_parser = parse_ai)]
    pub ai: Ai,
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
    pub ai: Option<Ai>,
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
    pub ai: Option<Ai>,
    /// Run FTS5 + vector + cross-encoder rerank via hive-embed
    #[arg(long)]
    pub hybrid: bool,
}

// ---------- notes ----------

#[derive(Debug, Subcommand)]
pub enum NotesCmd {
    Add(NotesAddArgs),
    List(NotesListArgs),
    Show { id: i64 },
    Search(NotesSearchArgs),
}

#[derive(Debug, Args)]
pub struct NotesAddArgs {
    #[arg(long, value_parser = parse_author)]
    pub author: Author,
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
    pub author: Option<Author>,
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
    pub author: Option<Author>,
    #[arg(long)]
    pub hybrid: bool,
}

// ---------- wire ----------

#[derive(Debug, Subcommand)]
pub enum WireCmd {
    Add(WireAddArgs),
    List(WireListArgs),
    Ack { id: i64 },
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
    pub severity: Option<Severity>,
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
    pub severity: Option<Severity>,
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
    Remove {
        id: i64,
    },
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

fn parse_owner(s: &str) -> Result<Owner, String> {
    s.parse::<Owner>().map_err(|e| e.to_string())
}
fn parse_ai(s: &str) -> Result<Ai, String> {
    s.parse::<Ai>().map_err(|e| e.to_string())
}
fn parse_author(s: &str) -> Result<Author, String> {
    s.parse::<Author>().map_err(|e| e.to_string())
}
fn parse_severity(s: &str) -> Result<Severity, String> {
    s.parse::<Severity>().map_err(|e| e.to_string())
}
fn parse_project_status(s: &str) -> Result<ProjectStatus, String> {
    s.parse::<ProjectStatus>().map_err(|e| e.to_string())
}
fn parse_task_status(s: &str) -> Result<TaskStatus, String> {
    s.parse::<TaskStatus>().map_err(|e| e.to_string())
}
