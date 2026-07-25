// hive-node — the binary (PLAN-v2.1 PR 4.6).
//
// Thin by contract (docs/TESTING-STRATEGY.md §4.1): argument parsing, key
// resolution ordering, the boot announcement, and the signal handling live
// here; everything else is the library, which the smoke tier embeds in-proc.
//
// The stdout contract, which `node/tests/boot.rs` parses and every later
// harness depends on:
//
//   node key <64 hex>                 the key a device pins at enrollment
//   domain <tenant>/<domain>          one line per vault opened
//   listening on <addr>               the ACTUAL bound address
//
// Machine-readable lines go to stdout; diagnostics go to stderr through
// tracing. (The bridge's rule — stdout is a protocol channel, never a log —
// applies here for the same reason: a harness parses it.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use hive_node::config::NodeConfig;
use hive_node::server::Node;

const USAGE: &str = "\
hive-node — the always-on hive peer (blind tier)

USAGE:
    hive-node --root <dir> [--listen <addr>] [--config <file>]
    hive-node --version | --help

OPTIONS:
    --root <dir>      Node root: node.toml, node.key, and tenants/<t>/domains/<d>/.
                      Created if absent.
    --listen <addr>   Bind address, overriding node.toml. Default 0.0.0.0:7847;
                      tests use 127.0.0.1:0 and read the port from the
                      `listening on` line.
    --config <file>   node.toml to read. Default <root>/node.toml, and a root
                      without one boots on the defaults.

The node holds no key to any domain it stores (D29): vaults are verbatim
ciphertext in exact store shape, write-once, plus node-meta.db. Restore is
`hive-sync restore` (PLAN-v2.1 PR 4.8) — or, at any time, a copy of the
directory.
";

#[derive(Debug)]
struct Cli {
    root: PathBuf,
    listen: Option<SocketAddr>,
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hive-node: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = match parse_args(std::env::args().skip(1).collect())? {
        Some(cli) => cli,
        None => {
            println!("{USAGE}");
            return Ok(());
        }
    };

    // Config and key resolution happen BEFORE the runtime exists — the
    // ordering every binary here follows for key custody (server.rs).
    let mut config = match &cli.config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            NodeConfig::parse(&text).with_context(|| format!("reading {}", path.display()))?
        }
        None => NodeConfig::load(&cli.root)?,
    };
    if let Some(listen) = cli.listen {
        config.listen = listen;
    }
    let listen = config.listen;
    let node = Node::open_with_config(&cli.root, config)?;

    println!("node key {}", node.node_key_hex());
    for vault in node.vaults() {
        println!("domain {}/{}", vault.tenant(), vault.domain());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;
    runtime.block_on(async move {
        let bound = node.bind(listen).await?;
        let addr = bound.local_addr();
        // The line every harness waits for. Flushed, because a test that
        // reads a pipe cannot wait for a buffer.
        println!("listening on {addr}");
        use std::io::Write;
        std::io::stdout().flush().context("flushing stdout")?;
        tracing::info!(%addr, vaults = node.vaults().len(), "hive-node listening");

        node.publish_dns(addr.port()).await?;

        let served = bound.serve(shutdown()).await?;
        tracing::info!(connections = served, "hive-node shutting down");
        Ok(())
    })
}

/// Resolves when the supervisor asks us to stop. SIGTERM is what a container
/// runtime and systemd send, and answering it cleanly (rather than being
/// killed) is what makes a restart cheap: the vault is only ever mid-append,
/// never mid-rewrite.
#[cfg(unix)]
async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot listen for SIGTERM: {e}");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
    }
}

#[cfg(not(unix))]
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// `Ok(None)` means "print the usage and stop" — help and version both.
fn parse_args(args: Vec<String>) -> Result<Option<Cli>> {
    let mut root: Option<PathBuf> = None;
    let mut listen: Option<SocketAddr> = None;
    let mut config: Option<PathBuf> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--version" | "-V" => {
                println!("hive-node {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--root" => {
                root = Some(PathBuf::from(
                    it.next().context("--root needs a directory")?,
                ));
            }
            "--listen" => {
                let raw = it.next().context("--listen needs an address")?;
                listen = Some(raw.parse().with_context(|| {
                    format!("--listen {raw:?} is not a host:port address (e.g. 127.0.0.1:0)")
                })?);
            }
            "--config" => {
                config = Some(PathBuf::from(it.next().context("--config needs a file")?));
            }
            other => bail!("unrecognized argument {other:?}\n\n{USAGE}"),
        }
    }
    let Some(root) = root else {
        bail!("--root is required\n\n{USAGE}");
    };
    Ok(Some(Cli {
        root,
        listen,
        config,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_prints_the_usage_and_stops() {
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
        assert!(parse_args(args(&["-h"])).unwrap().is_none());
    }

    #[test]
    fn a_root_is_required_because_a_node_is_its_directory() {
        let err = parse_args(args(&["--listen", "127.0.0.1:0"])).unwrap_err();
        assert!(format!("{err:#}").contains("--root is required"), "{err:#}");
    }

    #[test]
    fn the_listen_override_is_parsed_where_someone_can_read_the_error() {
        let cli = parse_args(args(&["--root", "/srv/hive", "--listen", "127.0.0.1:0"]))
            .unwrap()
            .unwrap();
        assert_eq!(cli.root, PathBuf::from("/srv/hive"));
        assert_eq!(cli.listen, Some("127.0.0.1:0".parse().unwrap()));
        assert!(cli.config.is_none());

        let err = parse_args(args(&["--root", "/srv/hive", "--listen", "7847"])).unwrap_err();
        assert!(format!("{err:#}").contains("host:port"), "{err:#}");
    }

    #[test]
    fn an_unknown_flag_is_a_refusal_not_a_default() {
        let err = parse_args(args(&["--root", "/srv/hive", "--tenant", "household"])).unwrap_err();
        assert!(format!("{err:#}").contains("--tenant"), "{err:#}");
    }

    /// The stdout contract is a contract: the harness greps these prefixes.
    #[test]
    fn the_usage_documents_the_lines_a_harness_parses() {
        assert!(USAGE.contains("listening on"));
        assert!(USAGE.contains("127.0.0.1:0"));
    }
}
