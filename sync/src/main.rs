// hive-sync — the replication CLI (PLAN-v2.1 PR 4.4: the skeleton).
//
// The protocol lives in this crate's LIBRARY half, which is what the desktop
// push loop (PR 4.8) and the node (PR 4.6) embed. This binary is the
// operator-facing surface that grows onto it, and it exists now so the shape
// is decided before there are two of them: the first command is
// `restore --node <addr> --into <empty-dir>` (PR 4.8) — verbatim files down,
// then the store's own heal folds everything at next open, which needs no
// core write API at all.
//
// It deliberately does nothing else yet. An unimplemented command exits 1
// with the PR that brings it, rather than pretending to be a stub server.

use std::process::ExitCode;

const USAGE: &str = "\
hive-sync — hive replication

USAGE
    hive-sync --version
    hive-sync --help

NOT YET AVAILABLE
    restore --node <addr> --into <dir>   pull a domain back down (PLAN-v2.1 PR 4.8)

The protocol library is what ships today: hive-node embeds it to serve, and
the desktop app embeds it to push.";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Outcome::Usage => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Outcome::Version => {
            println!("hive-sync {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Outcome::NotYet(what, pr) => {
            eprintln!("hive-sync: `{what}` is not implemented yet — it lands with {pr}.");
            ExitCode::FAILURE
        }
        Outcome::Unknown(arg) => {
            eprintln!("hive-sync: unrecognized argument {arg:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// What the CLI was asked to do. A pure function of the arguments so the
/// surface is testable without a process (the bridge's `parse_args`
/// precedent).
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Usage,
    Version,
    /// A command this skeleton names but does not serve, and the PR that
    /// brings it.
    NotYet(String, &'static str),
    Unknown(String),
}

fn run(args: Vec<String>) -> Outcome {
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") | Some("help") => Outcome::Usage,
        Some("--version") | Some("-V") => Outcome::Version,
        Some("restore") => Outcome::NotYet("restore".to_string(), "PLAN-v2.1 PR 4.8"),
        Some(other) => Outcome::Unknown(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_invocation_and_help_print_the_usage() {
        assert_eq!(run(Vec::new()), Outcome::Usage);
        assert_eq!(run(args(&["--help"])), Outcome::Usage);
        assert_eq!(run(args(&["-h"])), Outcome::Usage);
    }

    #[test]
    fn the_version_comes_from_the_workspace() {
        assert_eq!(run(args(&["--version"])), Outcome::Version);
        assert_eq!(run(args(&["-V"])), Outcome::Version);
    }

    /// The planned command is named, not silently missing: someone reading
    /// `--help` learns that restore exists and when.
    #[test]
    fn restore_says_which_pr_brings_it() {
        assert_eq!(
            run(args(&["restore", "--into", "/tmp/x"])),
            Outcome::NotYet("restore".to_string(), "PLAN-v2.1 PR 4.8")
        );
        assert!(USAGE.contains("restore"));
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        assert_eq!(
            run(args(&["serve"])),
            Outcome::Unknown("serve".to_string()),
            "there is no node in this binary — hive-node serves (PR 4.6)"
        );
    }
}
