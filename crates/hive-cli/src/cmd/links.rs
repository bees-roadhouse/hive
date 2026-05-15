use anyhow::Result;

use hive_db::queries::links::{self, EntityRef};

use crate::cli::LinksCmd;
use crate::format::{Column, pad_right, print_table, truncate};

pub fn run(cmd: LinksCmd) -> Result<()> {
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    match cmd {
        LinksCmd::Add { source, target, link_type, note } => {
            add(&conn, &source, &target, link_type.as_deref(), note.as_deref())
        }
        LinksCmd::List { source } => list_outgoing(&conn, &source),
        LinksCmd::Incoming { target } => list_incoming(&conn, &target),
        LinksCmd::Show { reference } => show(&conn, &reference),
        LinksCmd::Remove { id } => {
            links::remove(&conn, id)?;
            println!("removed link #{id}");
            Ok(())
        }
        LinksCmd::Types => types(&conn),
    }
}

fn add(
    conn: &hive_db::Connection,
    source: &str,
    target: &str,
    link_type: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    let src = EntityRef::parse(source, "--source")?;
    let tgt = EntityRef::parse(target, "--target")?;
    links::require_exists(conn, &src, "source")?;
    links::require_exists(conn, &tgt, "target")?;
    match links::add(conn, &src, &tgt, link_type, note)? {
        Some(id) => println!(
            "added link #{id}: {}:{} -> {}:{} ({})",
            src.table,
            src.id,
            tgt.table,
            tgt.id,
            link_type.unwrap_or("-")
        ),
        None => anyhow::bail!(
            "link already exists: {}:{} -> {}:{} (type={})",
            src.table,
            src.id,
            tgt.table,
            tgt.id,
            link_type.unwrap_or("NULL")
        ),
    }
    Ok(())
}

#[derive(Clone)]
struct EnrichedLink {
    id: i64,
    link_type: String,
    other: String,
    title: String,
    note: String,
}

fn enrich(
    conn: &hive_db::Connection,
    rows: Vec<hive_db::types::Link>,
    direction: Direction,
) -> Result<Vec<EnrichedLink>> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let (other_table_str, other_id) = match direction {
            Direction::Out => (r.target_table.as_str(), r.target_id),
            Direction::In => (r.source_table.as_str(), r.source_id),
        };
        let table = hive_db::enums::LinkTable::parse_short(other_table_str)?;
        let other_ref = EntityRef { table, id: other_id };
        let label = links::label_for(conn, &other_ref)?.unwrap_or_default();
        out.push(EnrichedLink {
            id: r.id,
            link_type: r.link_type.unwrap_or_else(|| "-".into()),
            other: format!("{}:{}", other_table_str, other_id),
            title: truncate(&label, 60),
            note: r.note.unwrap_or_default(),
        });
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum Direction {
    Out,
    In,
}

fn print_link_rows(rows: &[EnrichedLink]) {
    let cols: Vec<Column<'_, EnrichedLink>> = vec![
        Column::new("id", |r: &EnrichedLink| r.id.to_string()),
        Column::new("type", |r: &EnrichedLink| r.link_type.clone()),
        Column::new("ref", |r: &EnrichedLink| r.other.clone()),
        Column::new("title", |r: &EnrichedLink| r.title.clone()),
    ];
    let trailing: Box<dyn Fn(&EnrichedLink) -> String> = Box::new(|r| r.note.clone());
    print_table(&cols, rows, Some(("note", trailing)));
}

fn list_outgoing(conn: &hive_db::Connection, source: &str) -> Result<()> {
    let src = EntityRef::parse(source, "--source")?;
    links::require_exists(conn, &src, "source")?;
    let rows = links::outgoing(conn, &src)?;
    if rows.is_empty() {
        println!("no outgoing links");
        return Ok(());
    }
    let enriched = enrich(conn, rows, Direction::Out)?;
    print_link_rows(&enriched);
    Ok(())
}

fn list_incoming(conn: &hive_db::Connection, target: &str) -> Result<()> {
    let tgt = EntityRef::parse(target, "--target")?;
    links::require_exists(conn, &tgt, "target")?;
    let rows = links::incoming(conn, &tgt)?;
    if rows.is_empty() {
        println!("no incoming links");
        return Ok(());
    }
    let enriched = enrich(conn, rows, Direction::In)?;
    print_link_rows(&enriched);
    Ok(())
}

fn show(conn: &hive_db::Connection, reference: &str) -> Result<()> {
    let ent = EntityRef::parse(reference, "ref")?;
    let title = links::require_exists(conn, &ent, "entity")?;
    let out_rows = links::outgoing(conn, &ent)?;
    let in_rows = links::incoming(conn, &ent)?;

    println!("{}:{}  {}", ent.table, ent.id, title);
    println!("{}", "-".repeat(60));

    println!("outgoing:");
    if out_rows.is_empty() {
        println!("  (none)");
    } else {
        let enriched = enrich(conn, out_rows, Direction::Out)?;
        print_link_rows(&enriched);
    }
    println!();
    println!("incoming:");
    if in_rows.is_empty() {
        println!("  (none)");
    } else {
        let enriched = enrich(conn, in_rows, Direction::In)?;
        print_link_rows(&enriched);
    }
    Ok(())
}

fn types(conn: &hive_db::Connection) -> Result<()> {
    let rows = links::type_counts(conn)?;
    if rows.is_empty() {
        println!("no links yet");
        return Ok(());
    }
    let type_w = rows
        .iter()
        .map(|r| r.link_type.len())
        .max()
        .unwrap_or(0)
        .max("type".len());
    println!("{}  count", pad_right("type", type_w));
    println!("{}", "-".repeat(type_w + 7));
    for r in &rows {
        println!("{}  {}", pad_right(&r.link_type, type_w), r.count);
    }
    Ok(())
}
