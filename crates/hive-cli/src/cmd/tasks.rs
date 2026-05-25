use anyhow::Result;
use chrono::Local;
use serde_json::{Map, Value};

use crate::api::{self, Task};
use crate::cli::{ProjectAddArgs, ProjectCmd, TaskAddArgs, TaskListArgs, TaskUpdateArgs, TasksCmd};
use crate::cmd::links::attach_links;
use crate::format::{Column, fmt_ts_opt, pad_right, print_json, print_table};

pub async fn run(cmd: TasksCmd) -> Result<()> {
    match cmd {
        TasksCmd::Project { cmd } => match cmd {
            ProjectCmd::Add(args) => add_project(args).await,
            ProjectCmd::List { status } => list_projects(status).await,
            ProjectCmd::Archive { name } => archive_project(&name).await,
        },
        TasksCmd::Add(args) => add(args).await,
        TasksCmd::List(args) => list(args).await,
        TasksCmd::Show { id } => show(&id).await,
        TasksCmd::Update(args) => update(args).await,
        TasksCmd::Done { id } => {
            api::task_done(&id).await?;
            println!("closed task #{id}");
            Ok(())
        }
        TasksCmd::Block { id, reason } => {
            api::task_block(&id, &reason).await?;
            println!("blocked task #{id}: {reason}");
            Ok(())
        }
        TasksCmd::Drop { id } => {
            api::task_drop(&id).await?;
            println!("dropped task #{id}");
            Ok(())
        }
    }
}

async fn add_project(args: ProjectAddArgs) -> Result<()> {
    api::add_project(&args.name, args.description.as_deref(), &args.owner).await?;
    println!("added project: {}", args.name);
    Ok(())
}

async fn list_projects(status: Option<String>) -> Result<()> {
    let rows = api::list_projects(status.as_deref()).await?;
    if rows.is_empty() {
        println!("no projects");
        return Ok(());
    }
    use crate::api::Project as P;
    let cols: Vec<Column<'_, P>> = vec![
        Column::new("name", |p: &P| p.name.clone()),
        Column::new("owner", |p: &P| p.owner.clone()),
        Column::new("status", |p: &P| p.status.clone()),
    ];
    let trailing: Box<dyn Fn(&P) -> String> =
        Box::new(|p: &P| p.description.clone().unwrap_or_default());
    print_table(&cols, &rows, Some(("description", trailing)));
    Ok(())
}

async fn archive_project(name: &str) -> Result<()> {
    api::archive_project(name).await?;
    println!("archived project: {name}");
    Ok(())
}

async fn add(args: TaskAddArgs) -> Result<()> {
    let task = api::add_task(
        &args.project,
        &args.title,
        args.body.as_deref(),
        &args.owner,
        args.priority.as_deref(),
        args.due.as_deref(),
    )
    .await?;
    println!("added task #{}: {}", task.id, task.title);
    attach_links(&format!("tasks:{}", task.id), &args.link).await?;
    Ok(())
}

async fn list(args: TaskListArgs) -> Result<()> {
    let rows = api::list_tasks(
        args.project.as_deref(),
        args.owner.as_deref(),
        args.status.as_deref(),
        args.all,
    )
    .await?;

    if args.json {
        print_json(&rows)?;
        return Ok(());
    }
    if rows.is_empty() {
        println!("no tasks");
        return Ok(());
    }

    let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
    let due_today = today.clone();
    let cols: Vec<Column<'_, Task>> = vec![
        Column::new("id", |t: &Task| t.id.to_string()),
        Column::new("project", |t: &Task| t.project.clone().unwrap_or_default()),
        Column::new("owner", |t: &Task| t.owner.clone()),
        Column::new("status", |t: &Task| t.status.clone()),
        Column::new("pri", |t: &Task| t.priority.clone().unwrap_or_default()),
        Column::new("due", move |t: &Task| due_marker(t, &due_today)),
    ];
    let trailing: Box<dyn Fn(&Task) -> String> = Box::new(|t| t.title.clone());
    print_table(&cols, &rows, Some(("title", trailing)));
    Ok(())
}

fn due_marker(t: &Task, today: &str) -> String {
    let Some(due) = &t.due else {
        return String::new();
    };
    if matches!(t.status.as_str(), "done" | "dropped") {
        return due.clone();
    }
    if due.as_str() < today {
        return format!("{due} OVERDUE");
    }
    due.clone()
}

async fn show(id: &str) -> Result<()> {
    let row = api::show_task(id).await?;
    println!("#{}  {}", row.id, row.title);
    println!("{}", "-".repeat(60));
    let fields: Vec<(&str, String)> = vec![
        ("project", row.project.clone().unwrap_or_default()),
        ("owner", row.owner.clone()),
        ("status", row.status.clone()),
        ("priority", row.priority.clone().unwrap_or_default()),
        ("due", row.due.clone().unwrap_or_default()),
        ("block_reason", row.block_reason.clone().unwrap_or_default()),
        ("created_at", fmt_ts_opt(&row.created_at)),
        ("updated_at", fmt_ts_opt(&row.updated_at)),
        ("closed_at", fmt_ts_opt(&row.closed_at)),
    ];
    let label_w = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in &fields {
        println!("  {}  {}", pad_right(k, label_w), v);
    }
    if let Some(b) = &row.body
        && !b.is_empty()
    {
        println!();
        println!("body:");
        println!("{b}");
    }
    Ok(())
}

async fn update(args: TaskUpdateArgs) -> Result<()> {
    // Build the PATCH body. Only include fields the caller passed. `priority`,
    // `due`, and `body` accept an explicit null to clear (empty string for due
    // clears the date, matching python's `--due ''`).
    let mut body: Map<String, Value> = Map::new();
    if let Some(s) = args.status {
        body.insert("status".into(), Value::String(s));
    }
    if let Some(p) = args.priority {
        body.insert("priority".into(), str_or_null(&p));
    }
    if let Some(o) = args.owner {
        body.insert("owner".into(), Value::String(o));
    }
    if let Some(d) = args.due {
        if d.is_empty() {
            body.insert("due".into(), Value::Null);
        } else {
            chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("invalid --due '{d}'. expected YYYY-MM-DD"))?;
            body.insert("due".into(), Value::String(d));
        }
    }
    if let Some(b) = args.body {
        body.insert("body".into(), str_or_null(&b));
    }
    if let Some(t) = args.title {
        body.insert("title".into(), Value::String(t));
    }
    if body.is_empty() {
        anyhow::bail!(
            "at least one of --status / --priority / --owner / --due / --body / --title required"
        );
    }
    api::update_task(&args.id, &Value::Object(body)).await?;
    println!("updated task #{}", args.id);
    Ok(())
}

/// Empty string clears the field (JSON null); otherwise set the string.
fn str_or_null(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s.to_string())
    }
}
