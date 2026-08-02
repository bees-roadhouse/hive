// hive-node — the binary (PLAN-v2.1 PR 4.6, subcommands at 4.7).
//
// Thin by contract (docs/TESTING-STRATEGY.md §4.1): argument parsing, key
// resolution ordering, the boot announcement, and the signal handling live
// here; everything else is the library, which the smoke tier embeds in-proc.
//
// Three commands, and the shape of the split is deliberate: `serve` is the
// long-running one systemd starts, and `enroll`/`device` are one-shots an
// operator runs BESIDE it against the same root. They coexist because
// node-meta.db is WAL SQLite — a second process minting a code is a write the
// running listener sees on its very next connection, since the guest list is
// re-read per connection rather than snapshotted at boot (`listen.rs`).
//
// The stdout contract, which `node/tests/boot.rs` and the enrollment smoke
// parse, and which every later harness depends on:
//
//   serve:   node key <64 hex>          the key a device pins at enrollment
//            domain <tenant>/<domain>   one line per vault opened
//            listening on <addr>        the ACTUAL bound address
//   enroll:  enrollment code <code>     the one-time code, grouped for reading
//            domain <tenant>/<domain>
//            node key <64 hex>
//            expires <unix seconds>
//   device:  device <id> <pin> active|revoked
//            revoked <id>
//            control epoch <n>
//
// Machine-readable lines go to stdout; diagnostics go to stderr through
// tracing. (The bridge's rule — stdout is a protocol channel, never a log —
// applies here for the same reason: a harness parses it.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use hive_node::config::NodeConfig;
use hive_node::meta::unix_seconds;
use hive_node::server::Node;
use hive_node::{enroll, SegmentVault};

const USAGE: &str = "\
hive-node — the always-on hive peer (blind tier)

USAGE:
    hive-node [serve] --root <dir> [--listen <addr>] [--config <file>]
    hive-node enroll --root <dir> --domain <tenant>/<domain>
    hive-node device list --root <dir> --domain <tenant>/<domain>
    hive-node device revoke --root <dir> --domain <tenant>/<domain> --device <id>
    hive-node --version | --help

OPTIONS:
    --root <dir>      Node root: node.toml, node.key, and tenants/<t>/domains/<d>/.
                      Created if absent.
    --listen <addr>   Bind address, overriding node.toml. Default 0.0.0.0:7847;
                      tests use 127.0.0.1:0 and read the port from the
                      `listening on` line. A node.toml with
                      bind_scope = \"interface\" refuses a wildcard address here.
    --config <file>   node.toml to read. Default <root>/node.toml, and a root
                      without one boots on the defaults.
    --domain <t>/<d>  The tenant and domain to act on, exactly as the `domain`
                      line prints it. `enroll` creates the vault if absent.
    --device <id>     The device id to revoke.

ENROLLMENT (PLAN-v2.1 PR 4.7)
    `enroll` mints a ONE-TIME code, valid ten minutes, stored only as a hash.
    Read it to the device; the device redeems it over mTLS and the node pins
    its keys. v1 enrolls the domain's FIRST device — the one that already holds
    the master, so nothing secret crosses the wire. A second device needs the
    short-auth-string ceremony (PR 4.14) and is refused until it lands.

    `device revoke` unpins a device and raises the domain's control epoch. The
    tombstone is PERMANENT: there is deliberately no un-revoke, and a revoked
    device (or a revoked key under a new name) cannot be re-enrolled by a fresh
    code. Revoking takes effect on the next handshake; nothing restarts.

The node holds no key to any domain it stores (D29): vaults are verbatim
ciphertext in exact store shape, write-once, plus node-meta.db. Restore is
`hive-sync restore` (PLAN-v2.1 PR 4.8) — or, at any time, a copy of the
directory.
";

/// What the operator asked for. `serve` is the default because it is what a
/// supervisor starts, and because the flag-only form predates the subcommands.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve {
        listen: Option<SocketAddr>,
        config: Option<PathBuf>,
    },
    Enroll {
        domain: DomainRef,
    },
    DeviceList {
        domain: DomainRef,
    },
    DeviceRevoke {
        domain: DomainRef,
        device: String,
    },
}

/// `<tenant>/<domain>`, as the `domain` stdout line prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainRef {
    tenant: String,
    domain: String,
}

impl DomainRef {
    fn parse(raw: &str) -> Result<DomainRef> {
        let (tenant, domain) = raw.split_once('/').with_context(|| {
            format!("--domain {raw:?} is not <tenant>/<domain> (e.g. household/example.com)")
        })?;
        if tenant.is_empty() || domain.is_empty() || domain.contains('/') {
            bail!("--domain {raw:?} is not <tenant>/<domain> (e.g. household/example.com)");
        }
        Ok(DomainRef {
            tenant: tenant.to_string(),
            domain: domain.to_string(),
        })
    }
}

impl std::fmt::Display for DomainRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.tenant, self.domain)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Cli {
    root: PathBuf,
    command: Command,
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

    match cli.command {
        Command::Serve { listen, config } => serve(&cli.root, listen, config),
        Command::Enroll { domain } => mint_code(&cli.root, &domain),
        Command::DeviceList { domain } => list_devices(&cli.root, &domain),
        Command::DeviceRevoke { domain, device } => revoke_device(&cli.root, &domain, &device),
    }
}

fn serve(root: &PathBuf, listen: Option<SocketAddr>, config: Option<PathBuf>) -> Result<()> {
    // Config and key resolution happen BEFORE the runtime exists — the
    // ordering every binary here follows for key custody (server.rs).
    let mut config = match &config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            NodeConfig::parse(&text).with_context(|| format!("reading {}", path.display()))?
        }
        None => NodeConfig::load(root)?,
    };
    if let Some(listen) = listen {
        config.listen = listen;
    }
    let listen = config.listen;
    let node = Arc::new(Node::open_with_config(root, config)?);

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

        let report = bound.serve(node.clone(), shutdown()).await?;
        tracing::info!(
            accepted = report.accepted,
            rate_limited = report.rate_limited,
            over_capacity = report.over_capacity,
            "hive-node shutting down"
        );
        Ok(())
    })
}

/// `hive-node enroll` — no runtime, no socket: this is a database write and a
/// line of output.
fn mint_code(root: &PathBuf, domain: &DomainRef) -> Result<()> {
    let mut node = Node::open(root)?;
    let fresh = node.vault(&domain.tenant, &domain.domain).is_none();
    let vault = node.open_vault(&domain.tenant, &domain.domain)?;
    let minted = enroll::mint(&vault, SystemTime::now())?;

    println!("enrollment code {}", minted.display);
    println!("domain {domain}");
    println!("node key {}", node.node_key_hex());
    println!("expires {}", unix_seconds(minted.expires_at)?);
    if fresh {
        // The running listener discovered its vaults at boot (server.rs), so a
        // domain that did not exist a moment ago is one it cannot serve yet.
        // Say so here rather than let a redemption fail with "no domain minted
        // this code" on a box nobody is watching.
        eprintln!(
            "hive-node: created domain {domain} — restart the running hive-node so it serves \
             the new vault before redeeming this code"
        );
    }
    Ok(())
}

fn list_devices(root: &PathBuf, domain: &DomainRef) -> Result<()> {
    let vault = open_existing(root, domain)?;
    for pin in vault.meta().device_pins()? {
        println!(
            "device {} {} {}",
            pin.device,
            hive_sync::SpkiPin::from_ed25519_public(&pin.ed25519_pk),
            if pin.revoked { "revoked" } else { "active" }
        );
    }
    println!("control epoch {}", vault.meta().control_epoch()?);
    Ok(())
}

fn revoke_device(root: &PathBuf, domain: &DomainRef, device: &str) -> Result<()> {
    let vault = open_existing(root, domain)?;
    let epoch = enroll::revoke(&vault, device)?;
    println!("revoked {device}");
    println!("control epoch {epoch}");
    Ok(())
}

/// A vault that must already exist. `enroll` may create one (an operator's
/// first domain has to come into being somehow); `device` never does — being
/// asked to revoke a device in a domain that does not exist is a typo, and
/// answering it by creating an empty domain would hide the typo.
fn open_existing(root: &PathBuf, domain: &DomainRef) -> Result<SegmentVault> {
    let node = Node::open(root)?;
    node.vault(&domain.tenant, &domain.domain)
        .cloned()
        .with_context(|| format!("node root {} holds no domain {domain}", root.display()))
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
    let mut domain: Option<DomainRef> = None;
    let mut device: Option<String> = None;
    // The verb, as words, before any flag is interpreted. Absent means
    // `serve`: the flag-only form is what a supervisor unit already runs.
    let mut verb: Vec<String> = Vec::new();

    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.peek() {
        if arg.starts_with('-') {
            break;
        }
        verb.push(it.next().expect("peeked"));
    }
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
            "--domain" => {
                let raw = it.next().context("--domain needs <tenant>/<domain>")?;
                domain = Some(DomainRef::parse(&raw)?);
            }
            "--device" => {
                device = Some(it.next().context("--device needs a device id")?);
            }
            other => bail!("unrecognized argument {other:?}\n\n{USAGE}"),
        }
    }

    let verb: Vec<&str> = verb.iter().map(String::as_str).collect();
    let command = match verb.as_slice() {
        [] | ["serve"] => Command::Serve { listen, config },
        ["enroll"] => Command::Enroll {
            domain: needs_domain(domain, "enroll")?,
        },
        ["device", "list"] => Command::DeviceList {
            domain: needs_domain(domain, "device list")?,
        },
        ["device", "revoke"] => Command::DeviceRevoke {
            domain: needs_domain(domain, "device revoke")?,
            device: device.context("device revoke needs --device <id>")?,
        },
        ["device"] => bail!("device needs a subcommand: list or revoke\n\n{USAGE}"),
        other => bail!("unrecognized command {:?}\n\n{USAGE}", other.join(" ")),
    };
    let Some(root) = root else {
        bail!("--root is required\n\n{USAGE}");
    };
    Ok(Some(Cli { root, command }))
}

fn needs_domain(domain: Option<DomainRef>, what: &str) -> Result<DomainRef> {
    domain.with_context(|| format!("{what} needs --domain <tenant>/<domain>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    fn parse(raw: &[&str]) -> Cli {
        parse_args(args(raw))
            .unwrap_or_else(|e| panic!("parsing {raw:?}: {e:#}"))
            .expect("a command, not the usage")
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

    /// The flag-only form is the supervisor's, and it must keep meaning
    /// `serve` — `node/tests/boot.rs` and any systemd unit already run it.
    #[test]
    fn no_verb_means_serve() {
        let cli = parse(&["--root", "/srv/hive", "--listen", "127.0.0.1:0"]);
        assert_eq!(cli.root, PathBuf::from("/srv/hive"));
        assert_eq!(
            cli.command,
            Command::Serve {
                listen: Some("127.0.0.1:0".parse().unwrap()),
                config: None,
            }
        );
        assert_eq!(parse(&["serve", "--root", "/srv/hive"]).command, {
            Command::Serve {
                listen: None,
                config: None,
            }
        });
    }

    #[test]
    fn the_listen_override_is_parsed_where_someone_can_read_the_error() {
        let err = parse_args(args(&["--root", "/srv/hive", "--listen", "7847"])).unwrap_err();
        assert!(format!("{err:#}").contains("host:port"), "{err:#}");
    }

    #[test]
    fn an_unknown_flag_is_a_refusal_not_a_default() {
        let err = parse_args(args(&["--root", "/srv/hive", "--tenant", "household"])).unwrap_err();
        assert!(format!("{err:#}").contains("--tenant"), "{err:#}");

        let err = parse_args(args(&["enrol", "--root", "/srv/hive"])).unwrap_err();
        assert!(format!("{err:#}").contains("enrol"), "{err:#}");
    }

    #[test]
    fn enroll_and_revoke_name_the_domain_the_way_the_node_prints_it() {
        assert_eq!(
            parse(&[
                "enroll",
                "--root",
                "/srv/hive",
                "--domain",
                "household/example.com"
            ])
            .command,
            Command::Enroll {
                domain: DomainRef {
                    tenant: "household".to_string(),
                    domain: "example.com".to_string(),
                },
            }
        );
        assert_eq!(
            parse(&[
                "device",
                "revoke",
                "--root",
                "/srv/hive",
                "--domain",
                "household/example.com",
                "--device",
                "dev-laptop"
            ])
            .command,
            Command::DeviceRevoke {
                domain: DomainRef {
                    tenant: "household".to_string(),
                    domain: "example.com".to_string(),
                },
                device: "dev-laptop".to_string(),
            }
        );
    }

    #[test]
    fn a_command_that_needs_a_domain_says_so() {
        let err = parse_args(args(&["enroll", "--root", "/srv/hive"])).unwrap_err();
        assert!(format!("{err:#}").contains("--domain"), "{err:#}");

        let err = parse_args(args(&[
            "device",
            "revoke",
            "--root",
            "/srv/hive",
            "--domain",
            "household/example.com",
        ]))
        .unwrap_err();
        assert!(format!("{err:#}").contains("--device"), "{err:#}");

        let err = parse_args(args(&["device", "--root", "/srv/hive"])).unwrap_err();
        assert!(format!("{err:#}").contains("list or revoke"), "{err:#}");
    }

    /// A domain reference is `<tenant>/<domain>` and nothing looser — the two
    /// halves become directory names, and the vault's own allowlist is the
    /// second gate, not the first.
    #[test]
    fn a_domain_reference_is_exactly_two_parts() {
        assert_eq!(DomainRef::parse("a/b").unwrap().to_string(), "a/b");
        for bad in ["", "household", "/example.com", "household/", "a/b/c"] {
            assert!(DomainRef::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// The stdout contract is a contract: the harness greps these prefixes.
    #[test]
    fn the_usage_documents_the_lines_a_harness_parses() {
        assert!(USAGE.contains("listening on"));
        assert!(USAGE.contains("127.0.0.1:0"));
        assert!(USAGE.contains("enroll"));
        assert!(USAGE.contains("device revoke"));
    }
}
