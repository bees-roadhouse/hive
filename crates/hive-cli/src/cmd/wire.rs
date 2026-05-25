use anyhow::Result;

use crate::api::{self, WireEvent};
use crate::cli::{WireAddArgs, WireCmd, WireListArgs};
use crate::format::{Column, print_json, print_table, truncate};

pub async fn run(cmd: WireCmd) -> Result<()> {
    match cmd {
        WireCmd::Add(args) => add(args).await,
        WireCmd::List(args) => list(args).await,
        WireCmd::Ack { id } => {
            api::ack_wire(&id).await?;
            println!("acknowledged wire event #{id}");
            Ok(())
        }
    }
}

async fn add(args: WireAddArgs) -> Result<()> {
    let res = api::add_wire(
        &args.source,
        &args.title,
        args.body.as_deref(),
        args.external_id.as_deref(),
        args.severity.as_deref(),
        args.affects.as_deref(),
        args.url.as_deref(),
        args.category.as_deref(),
    )
    .await?;

    // `/wire` POST returns {"added": <event>} or {"already_seen": {"id": ...}}.
    if let Some(seen) = res.get("already_seen") {
        println!(
            "wire event #{} already seen (last_seen_at bumped)",
            id_str(seen)
        );
    } else if let Some(added) = res.get("added") {
        println!("added wire event #{}", id_str(added));
    } else {
        println!("added wire event");
    }
    Ok(())
}

/// Stringify a nested `id` value, tolerating string (uuid) or number (legacy).
fn id_str(v: &serde_json::Value) -> String {
    match v.get("id") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "?".to_string(),
    }
}

async fn list(args: WireListArgs) -> Result<()> {
    let rows = api::list_wire(
        args.source.as_deref(),
        args.severity.as_deref(),
        args.unacknowledged,
        args.limit,
    )
    .await?;

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
        Column::new("sev", |r: &WireEvent| {
            r.severity.clone().unwrap_or_default()
        }),
        Column::new("ack", |r: &WireEvent| {
            if r.acknowledged {
                "yes".into()
            } else {
                "no".into()
            }
        }),
        Column::new("title", |r: &WireEvent| truncate(&r.title, 60)),
    ];
    let trailing: Box<dyn Fn(&WireEvent) -> String> =
        Box::new(|r| r.affects.clone().unwrap_or_default());
    print_table(&cols, &rows, Some(("affects", trailing)));
    Ok(())
}
