// hive-import — CLI shell over importer/src/lib.rs. Argument parsing, data
// dir resolution (the app/bridge rules), and master-key custody live here;
// the migration itself is the library so the fixture tests drive it directly.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use hive_core::keys::{KeySource, KeychainKeySource};
use hive_import::{redact_url, run, Opts, RunOutcome};

const USAGE: &str = "\
hive-import — one-shot migration of a hosted hive Postgres into a local data dir

USAGE:
    hive-import --from <postgres-url> [--data-dir <dir>] [--dry-run]

OPTIONS:
    --from <url>      Postgres URL of the old hosted instance (required).
    --data-dir <dir>  Target data dir. Default: $HIVE_DATA_DIR, else
                      $XDG_DATA_HOME/hive, else ~/.local/share/hive — the
                      same resolution the hive app and hive-bridge use.
    --dry-run         Connect, count, print the plan; write nothing.

The target must not already hold a hive store (a device file or op-log
segments): hive-import is one-shot, not incremental.

Keys: the master key comes from the OS keychain (created there on first
use), so the hive app opens the imported store with no extra steps.
HIVE_IMPORT_KEY_HEX (64 hex chars) bypasses the keychain — tests and
keychain-less hosts only; a store imported under it is NOT readable by an
app using the keychain. (The equivalent bridge-only escape hatch is
HIVE_MEMORY_KEY_HEX; each binary deliberately names its own.)

Embeddings are not computed during import — the hive app backfills them in
the background. Mail account credentials do not migrate; Phase 3 re-enters
them against the OS keychain.
";

struct Cli {
    from: String,
    data_dir: PathBuf,
    dry_run: bool,
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    if let Err(e) = try_main() {
        eprintln!("hive-import: {e:#}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = parse_args(std::env::args().skip(1).collect())?;
    // Key resolution happens BEFORE the tokio runtime exists: keyring's sync
    // Secret Service backend drives its own zbus executor via block_on and
    // panics on a tokio thread (the app and bridge order it the same way).
    // A dry run resolves nothing — it must not mint a keychain entry.
    let master_key = if cli.dry_run {
        None
    } else {
        Some(master_key()?)
    };
    let opts = Opts {
        from: cli.from,
        data_dir: cli.data_dir,
        dry_run: cli.dry_run,
        master_key,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;
    let outcome = rt.block_on(async { run(&opts).await })?;
    report(&opts, &outcome);
    Ok(())
}

/// Human-readable results on stdout. The lib returns data (RunOutcome) so
/// the app's onboarding can render the same structs as cards; formatting
/// them is this CLI's job.
fn report(opts: &Opts, outcome: &RunOutcome) {
    println!("source     {}", redact_url(&opts.from));
    println!("data dir   {}", opts.data_dir.display());
    println!("plan (source rows):");
    for (table, n) in &outcome.plan().tables {
        println!("  {table:<22} {n}");
    }
    match outcome {
        RunOutcome::Plan(_) => println!("[dry run] nothing written."),
        RunOutcome::Imported(summary) => {
            println!(
                "imported {} records into {} ({} attachment blobs stored, {} mail FTS rows)",
                summary.records,
                opts.data_dir.display(),
                summary.blobs_stored,
                summary.mail_fts_rows
            );
            println!(
                "no embeddings were computed — the hive app backfills them in the background \
                 after its first open of this store"
            );
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<Cli> {
    let mut from: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                eprint!("{USAGE}");
                std::process::exit(0);
            }
            "--from" => {
                from = Some(
                    it.next()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| anyhow!("--from requires a value\n\n{USAGE}"))?,
                );
            }
            "--data-dir" => {
                data_dir = Some(PathBuf::from(
                    it.next()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| anyhow!("--data-dir requires a value\n\n{USAGE}"))?,
                ));
            }
            "--dry-run" => dry_run = true,
            other => return Err(anyhow!("unrecognized argument {other:?}\n\n{USAGE}")),
        }
    }
    Ok(Cli {
        from: from.ok_or_else(|| anyhow!("--from is required\n\n{USAGE}"))?,
        data_dir: data_dir.unwrap_or_else(default_data_dir),
        dry_run,
    })
}

/// Mirrors the bridge's data_dir() (which mirrors app/src/main.rs): the
/// HIVE_DATA_DIR override, else XDG, else ~/.local/share/hive — the importer
/// must fill exactly the store the app will open.
fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HIVE_DATA_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        });
    base.join("hive")
}

/// Master key: HIVE_IMPORT_KEY_HEX when set (tests / keychain-less hosts),
/// else the OS keychain — the same entry the app reads, so the imported
/// store opens there without ceremony.
fn master_key() -> Result<[u8; 32]> {
    if let Ok(hex) = std::env::var("HIVE_IMPORT_KEY_HEX") {
        let hex = hex.trim();
        if !hex.is_empty() {
            let bytes = data_encoding::HEXLOWER_PERMISSIVE
                .decode(hex.as_bytes())
                .context("HIVE_IMPORT_KEY_HEX is not valid hex")?;
            return bytes
                .try_into()
                .map_err(|_| anyhow!("HIVE_IMPORT_KEY_HEX must decode to exactly 32 bytes"));
        }
    }
    KeychainKeySource::new()
        .master_key()
        .context("OS keychain unavailable (set HIVE_IMPORT_KEY_HEX only for tests)")
}
