use anyhow::Result;

use crate::api;
use crate::cli::SearchArgs;

pub async fn run(args: SearchArgs) -> Result<()> {
    if args.hybrid {
        anyhow::bail!(
            "hybrid search is pending (hive-api /search/semantic returns 501; task #4 / hive-embed)"
        );
    }
    let hits = api::search(&args.query, args.limit).await?;

    if hits.journal.is_empty() && hits.notes.is_empty() {
        println!("no matches");
        return Ok(());
    }
    if !hits.journal.is_empty() {
        println!("=== journal ===");
        for h in &hits.journal {
            let title = h.title.clone().unwrap_or_else(|| "(untitled)".into());
            println!("#{}  {}  {}  {}", h.id, h.entry_date, h.ai, title);
            println!("    {}", h.snippet);
        }
    }
    if !hits.notes.is_empty() {
        if !hits.journal.is_empty() {
            println!();
        }
        println!("=== notes ===");
        for h in &hits.notes {
            let title = h.title.clone().unwrap_or_else(|| "(untitled)".into());
            let proj = h
                .project
                .as_deref()
                .map(|p| format!(" [{p}]"))
                .unwrap_or_default();
            println!("#{}  {}{}  {}", h.id, h.author, proj, title);
            println!("    {}", h.snippet);
        }
    }
    Ok(())
}
