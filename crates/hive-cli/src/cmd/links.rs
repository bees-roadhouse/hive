use anyhow::Result;

use crate::api::{self, Link};
use crate::cli::LinksCmd;
use crate::format::{Column, pad_right, print_table};

// Short table names accepted in link specs, mirroring hive-api's
// `LinkTable::parse_short`. Validated CLI-side so a typo fails before the HTTP
// round-trip; the API re-validates.
const LINK_TABLES: &[&str] = &["tasks", "journal", "notes", "wire", "projects"];

pub async fn run(cmd: LinksCmd) -> Result<()> {
    match cmd {
        LinksCmd::Add {
            source,
            target,
            link_type,
            note,
        } => add(&source, &target, link_type.as_deref(), note.as_deref()).await,
        LinksCmd::List { source } => list_outgoing(&source).await,
        LinksCmd::Incoming { target } => list_incoming(&target).await,
        LinksCmd::Show { reference } => show(&reference).await,
        LinksCmd::Remove { id } => {
            api::remove_link(&id).await?;
            println!("removed link #{id}");
            Ok(())
        }
        LinksCmd::Types => types().await,
    }
}

/// Pull an id out of a `{"id": ...}` response, tolerating either a JSON string
/// (uuid schema) or a number (legacy integer schema).
fn id_str(v: &serde_json::Value) -> String {
    match v.get("id") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "?".to_string(),
    }
}

/// Validate a `<table>:<id>` reference CLI-side: known table + non-empty id.
/// The id is NOT parsed as a uuid ... the server may be on legacy integer PKs,
/// so we accept any non-empty id and let hive-api validate it.
fn validate_ref(spec: &str, field: &str) -> Result<()> {
    let (table, ident) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid {field} '{spec}'. expected <table>:<id>"))?;
    if !LINK_TABLES.contains(&table) {
        let mut valid = LINK_TABLES.to_vec();
        valid.sort_unstable();
        anyhow::bail!(
            "invalid {field} table '{table}'. valid: {}",
            valid.join(", ")
        );
    }
    if ident.is_empty() {
        anyhow::bail!("invalid {field} '{spec}'. id missing");
    }
    Ok(())
}

async fn add(
    source: &str,
    target: &str,
    link_type: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    validate_ref(source, "--source")?;
    validate_ref(target, "--target")?;
    let res = api::add_link(source, target, link_type, note).await?;
    let id = id_str(&res);
    println!(
        "added link #{id}: {source} -> {target} ({})",
        link_type.unwrap_or("-")
    );
    Ok(())
}

/// Attach `--link` specs (`<table>:<uuid>[:<link_type>]`) to a freshly-created
/// `source` (`<table>:<uuid>`). Mirrors python `attach_links_from_args`: one
/// `POST /links` per spec, printing a line per result. Shared by tasks /
/// journal / notes add commands.
pub async fn attach_links(source: &str, specs: &[String]) -> Result<()> {
    for spec in specs {
        let mut parts = spec.splitn(3, ':');
        let table = parts.next().unwrap_or("");
        let ident = parts.next().unwrap_or("");
        let link_type = parts.next().filter(|s| !s.is_empty());
        let target = format!("{table}:{ident}");
        validate_ref(&target, "--link")?;
        match api::add_link(source, &target, link_type, None).await {
            Ok(res) => {
                let id = id_str(&res);
                println!(
                    "  linked #{id}: {source} -> {target} ({})",
                    link_type.unwrap_or("-")
                );
            }
            Err(e) => println!("  {e}"),
        }
    }
    Ok(())
}

fn print_link_rows(rows: &[Link], outgoing: bool) {
    let cols: Vec<Column<'_, Link>> = vec![
        Column::new("id", |r: &Link| r.id.to_string()),
        Column::new("type", |r: &Link| {
            r.link_type.clone().unwrap_or_else(|| "-".into())
        }),
        Column::new("ref", move |r: &Link| {
            if outgoing {
                format!("{}:{}", r.target_table, r.target_id)
            } else {
                format!("{}:{}", r.source_table, r.source_id)
            }
        }),
    ];
    let trailing: Box<dyn Fn(&Link) -> String> = Box::new(|r| r.note.clone().unwrap_or_default());
    print_table(&cols, rows, Some(("note", trailing)));
}

async fn list_outgoing(source: &str) -> Result<()> {
    validate_ref(source, "--source")?;
    let rows = api::links_outgoing(source).await?;
    if rows.is_empty() {
        println!("no outgoing links");
        return Ok(());
    }
    print_link_rows(&rows, true);
    Ok(())
}

async fn list_incoming(target: &str) -> Result<()> {
    validate_ref(target, "--target")?;
    let rows = api::links_incoming(target).await?;
    if rows.is_empty() {
        println!("no incoming links");
        return Ok(());
    }
    print_link_rows(&rows, false);
    Ok(())
}

/// `links show <ref>` ... compose outgoing + incoming (the API has no single
/// "both directions" endpoint, so the CLI fans out two GETs).
async fn show(reference: &str) -> Result<()> {
    validate_ref(reference, "ref")?;
    let out_rows = api::links_outgoing(reference).await?;
    let in_rows = api::links_incoming(reference).await?;

    println!("{reference}");
    println!("{}", "-".repeat(60));

    println!("outgoing:");
    if out_rows.is_empty() {
        println!("  (none)");
    } else {
        print_link_rows(&out_rows, true);
    }
    println!();
    println!("incoming:");
    if in_rows.is_empty() {
        println!("  (none)");
    } else {
        print_link_rows(&in_rows, false);
    }
    Ok(())
}

async fn types() -> Result<()> {
    let rows = api::link_types().await?;
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
