use anyhow::Result;

use hive_db::queries::wire::{self, AddArgs, AddResult};
use hive_db::types::WireEvent;

use crate::cli::{WireAddArgs, WireCmd, WireListArgs};
use crate::format::{Column, print_json, print_table, truncate};

pub fn run(cmd: WireCmd) -> Result<()> {
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    match cmd {
        WireCmd::Add(args) => add(&conn, args),
        WireCmd::List(args) => list(&conn, args),
        WireCmd::Ack { id } => {
            wire::ack(&conn, id)?;
            println!("acknowledged wire event #{id}");
            Ok(())
        }
    }
}

fn add(conn: &hive_db::Connection, args: WireAddArgs) -> Result<()> {
    let res = wire::add(
        conn,
        AddArgs {
            source: &args.source,
            title: &args.title,
            body: args.body.as_deref(),
            external_id: args.external_id.as_deref(),
            severity: args.severity,
            affects: args.affects.as_deref(),
            url: args.url.as_deref(),
            category: args.category.as_deref(),
        },
    )?;
    match res {
        AddResult::Added(e) => println!("added wire event #{}", e.id),
        AddResult::AlreadySeen { id } => {
            println!("wire event #{id} already seen (last_seen_at bumped)")
        }
    }
    Ok(())
}

fn list(conn: &hive_db::Connection, args: WireListArgs) -> Result<()> {
    let filters = wire::ListFilters {
        source: args.source,
        severity: args.severity,
        unacknowledged: args.unacknowledged,
        limit: Some(args.limit),
    };
    let rows = wire::list(conn, &filters)?;

    if args.json {
        print_json(&rows)?;
        return Ok(());
    }
    if rows.is_empty() {
        println!("no wire events");
        return Ok(());
    }

    let cols: Vec<Column<'_, WireEvent>> = vec![
        Column::new("id", |r: &WireEvent| r.id.to_string()),
        Column::new("source", |r: &WireEvent| r.source.clone()),
        Column::new("sev", |r: &WireEvent| r.severity.clone().unwrap_or_default()),
        Column::new("ack", |r: &WireEvent| {
            if r.acknowledged { "yes".into() } else { "no".into() }
        }),
        Column::new("title", |r: &WireEvent| truncate(&r.title, 60)),
    ];
    let trailing: Box<dyn Fn(&WireEvent) -> String> =
        Box::new(|r| r.affects.clone().unwrap_or_default());
    print_table(&cols, &rows, Some(("affects", trailing)));
    Ok(())
}
