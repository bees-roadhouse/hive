use anyhow::Result;
use chrono::Local;

use hive_db::enums::TaskStatus;
use hive_db::queries::links::{EntityRef, LinkSpec, attach_from_specs};
use hive_db::queries::{projects, tasks};
use hive_db::types::Task;

use crate::cli::{
    ProjectAddArgs, ProjectCmd, TaskAddArgs, TaskListArgs, TaskUpdateArgs, TasksCmd,
};
use crate::format::{Column, pad_right, print_json, print_table};

pub fn run(cmd: TasksCmd) -> Result<()> {
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    match cmd {
        TasksCmd::Project { cmd } => match cmd {
            ProjectCmd::Add(args) => add_project(&conn, args),
            ProjectCmd::List { status } => list_projects(&conn, status),
            ProjectCmd::Archive { name } => archive_project(&conn, &name),
        },
        TasksCmd::Add(args) => add(&conn, args),
        TasksCmd::List(args) => list(&conn, args),
        TasksCmd::Show { id } => show(&conn, id),
        TasksCmd::Update(args) => update(&conn, args),
        TasksCmd::Done { id } => {
            tasks::mark_done(&conn, id)?;
            println!("closed task #{id}");
            Ok(())
        }
        TasksCmd::Block { id, reason } => {
            tasks::mark_blocked(&conn, id, &reason)?;
            println!("blocked task #{id}: {reason}");
            Ok(())
        }
        TasksCmd::Drop { id } => {
            tasks::mark_dropped(&conn, id)?;
            println!("dropped task #{id}");
            Ok(())
        }
    }
}

fn add_project(conn: &hive_db::Connection, args: ProjectAddArgs) -> Result<()> {
    projects::add(conn, &args.name, args.description.as_deref(), args.owner)?;
    println!("added project: {}", args.name);
    Ok(())
}

fn list_projects(
    conn: &hive_db::Connection,
    status: Option<hive_db::enums::ProjectStatus>,
) -> Result<()> {
    let rows = projects::list(conn, status)?;
    if rows.is_empty() {
        println!("no projects");
        return Ok(());
    }
    use hive_db::types::Project as P;
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

fn archive_project(conn: &hive_db::Connection, name: &str) -> Result<()> {
    projects::archive(conn, name)?;
    println!("archived project: {name}");
    Ok(())
}

fn add(conn: &hive_db::Connection, args: TaskAddArgs) -> Result<()> {
    let task = tasks::add(
        conn,
        &args.project,
        &args.title,
        args.body.as_deref(),
        args.owner,
        args.priority.as_deref(),
        args.due.as_deref(),
    )?;
    println!("added task #{}: {}", task.id, task.title);

    if !args.link.is_empty() {
        let specs = args
            .link
            .iter()
            .map(|s| LinkSpec::parse(s))
            .collect::<hive_db::Result<Vec<_>>>()?;
        let source = EntityRef {
            table: hive_db::enums::LinkTable::Tasks,
            id: task.id,
        };
        let msgs = attach_from_specs(conn, &source, &specs)?;
        for m in msgs {
            println!("  {m}");
        }
    }
    Ok(())
}

fn list(conn: &hive_db::Connection, args: TaskListArgs) -> Result<()> {
    let filters = tasks::ListFilters {
        project: args.project,
        owner: args.owner,
        status: args.status,
        all: args.all,
    };
    let rows = tasks::list(conn, &filters)?;

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
        Column::new("project", |t: &Task| t.project.clone()),
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

fn show(conn: &hive_db::Connection, id: i64) -> Result<()> {
    let row = tasks::require(conn, id)?;
    println!("#{}  {}", row.id, row.title);
    println!("{}", "-".repeat(60));
    let fields: Vec<(&str, String)> = vec![
        ("project", row.project.clone()),
        ("owner", row.owner.clone()),
        ("status", row.status.clone()),
        ("priority", row.priority.clone().unwrap_or_default()),
        ("due", row.due.clone().unwrap_or_default()),
        ("block_reason", row.block_reason.clone().unwrap_or_default()),
        ("created_at", row.created_at.clone()),
        ("updated_at", row.updated_at.clone()),
        ("closed_at", row.closed_at.clone().unwrap_or_default()),
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

fn update(conn: &hive_db::Connection, args: TaskUpdateArgs) -> Result<()> {
    let mut fields = tasks::UpdateFields::default();
    if let Some(s) = args.status {
        fields.status = Some(s.parse::<TaskStatus>()?);
    }
    if args.priority.is_some() {
        fields.priority = Some(args.priority);
    }
    if let Some(o) = args.owner {
        fields.owner = Some(o.parse::<hive_db::enums::Owner>()?);
    }
    if let Some(d) = args.due {
        if d.is_empty() {
            fields.due = Some(None);
        } else {
            chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").map_err(|_| {
                hive_db::Error::InvalidFormat {
                    field: "--due",
                    value: d.clone(),
                    expected: "YYYY-MM-DD",
                }
            })?;
            fields.due = Some(Some(d));
        }
    }
    if args.body.is_some() {
        fields.body = Some(args.body);
    }
    if let Some(t) = args.title {
        fields.title = Some(t);
    }
    tasks::update(conn, args.id, &fields)?;
    println!("updated task #{}", args.id);
    Ok(())
}
