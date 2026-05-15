//! `hive` ... shared-state CLI.
//!
//! Drop-in replacement for `python ~/.hive/hive.py`. Subcommand grammar and
//! output shape match the python; this is enforced by snapshot tests.

mod cli;
mod cmd;
mod format;

use clap::Parser;

use cli::{Cli, Top};

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        // db-not-found exits 2 to match python; everything else 1.
        let code = match e.downcast_ref::<hive_db::Error>() {
            Some(hive_db::Error::DbNotFound(_)) => 2,
            _ => 1,
        };
        std::process::exit(code);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Top::Init => cmd::init::run()?,
        Top::Tasks { cmd } => cmd::tasks::run(cmd)?,
        Top::Journal { cmd } => cmd::journal::run(cmd)?,
        Top::Notes { cmd } => cmd::notes::run(cmd)?,
        Top::Wire { cmd } => cmd::wire::run(cmd)?,
        Top::Links { cmd } => cmd::links::run(cmd)?,
        Top::Graph(args) => cmd::graph::run(args)?,
        Top::Search(args) => cmd::search::run(args)?,
    }
    Ok(())
}
