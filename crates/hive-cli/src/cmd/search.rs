use anyhow::Result;

use hive_db::queries::search;

use crate::cli::SearchArgs;

pub fn run(args: SearchArgs) -> Result<()> {
    if args.hybrid {
        anyhow::bail!(
            "hybrid search is pending (see DESIGN.md embedder section ... task #4 / hive-embed)"
        );
    }
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    let j = search::journal(&conn, &args.query, args.limit)?;
    let n = search::notes(&conn, &args.query, args.limit)?;

    if j.is_empty() && n.is_empty() {
        println!("no matches");
        return Ok(());
    }
    if !j.is_empty() {
        println!("=== journal ===");
        for h in &j {
            let title = h.title.clone().unwrap_or_else(|| "(untitled)".into());
            println!("#{}  {}  {}  {}", h.id, h.entry_date, h.ai, title);
            println!("    {}", h.snippet);
        }
    }
    if !n.is_empty() {
        if !j.is_empty() {
            println!();
        }
        println!("=== notes ===");
        for h in &n {
            let title = h.title.clone().unwrap_or_else(|| "(untitled)".into());
            let proj = h.project.as_deref().map(|p| format!(" [{p}]")).unwrap_or_default();
            println!("#{}  {}{}  {}", h.id, h.author, proj, title);
            println!("    {}", h.snippet);
        }
    }
    Ok(())
}
