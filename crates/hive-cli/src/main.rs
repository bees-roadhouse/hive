//! `hive` ... shared-state CLI.
//!
//! Drop-in replacement for `python ~/.hive/hive.py`. Subcommand grammar and
//! output shape match the python; this is enforced by snapshot tests.
//!
//! HTTP-client port: the CLI is a thin client over hive-api. The API is the
//! source of truth; every consumer (this CLI, hive-ui, the future iPad client)
//! hits it. There is no database access here ... see `api.rs` for the
//! network-aware base-URL resolver and the request functions.

mod api;
mod auth;
mod cli;
mod cmd;
mod format;
mod journal_input;

use clap::Parser;

use cli::{Cli, Top};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Top::Init => cmd::init::run().await?,
        Top::Login(args) => cmd::login::run(args).await?,
        Top::Logout => cmd::login::logout().await?,
        Top::Tasks { cmd } => cmd::tasks::run(cmd).await?,
        Top::Journal { cmd } => cmd::journal::run(cmd).await?,
        Top::Notes { cmd } => cmd::notes::run(cmd).await?,
        Top::Wire { cmd } => cmd::wire::run(cmd).await?,
        Top::Links { cmd } => cmd::links::run(cmd).await?,
        Top::Graph(args) => cmd::graph::run(args).await?,
        Top::Search(args) => cmd::search::run(args).await?,
    }
    Ok(())
}
