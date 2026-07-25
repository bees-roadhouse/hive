// GREP-FENCE (PLAN-v2.1 PR 4.6, docs/TESTING-STRATEGY.md §2): two
// architecture rules the tree enforces mechanically, in the crate whose birth
// creates the exception to the first one.
//
//   1. IDENTITY FENCE — "no sessions, tokens, or OIDC below the node binary."
//      Everything about who a human is belongs to the node's console (D33,
//      deferred to tenant 2 and designed on paper); everything below it knows
//      only devices and keys (D16: single user, no accounts). A session crate
//      that drifted into hive-core would be an authentication layer nobody
//      decided to build.
//
//   2. TENANCY FENCE — "the data plane is tenancy-blind." A tenant is
//      grouping plus quotas plus console scope (D33). `tenants/<t>/domains/<d>/`
//      is a path THIS crate computes; below it, a domain is a domain and
//      hive-core must not learn the word.
//
// Both fence tokens that are IDENTIFIERS or DEPENDENCY names, never English
// words — the revision the plan's critique forced, and the reason these land
// green against the existing tree:
//
//   * `core/src/store/cc_credentials.rs:53` legitimately defines the
//     credential kind string "oauth_token", and comments in
//     `core/tests/mcp_tools.rs`, `core/tests/store_smoke.rs` and
//     `importer/src/lib.rs` mention OAuth as history. Bare `oauth`/`token`
//     are therefore NOT fenced; `use oauth2` and `oauth2::` are.
//   * A comment saying "core is deliberately tenancy-blind" must stay
//     writable, so the English word "tenant" is not fenced; `tenant_id`,
//     `Tenant`, and `tenants/` are.
//
// Source greps are total (code and comments alike, the posture
// core/tests/determinism.rs takes) and the crate list is parsed from the
// workspace manifest, so a new crate cannot dodge a fence by not being listed
// here (the shape importer/tests/no_postgres_gate.rs established). On top of
// the greps, the identity fence walks the RESOLVED dependency graph: a
// forbidden crate must not be reachable from any workspace crate except
// hive-node, however it got there.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The crate this PR creates, and the ONE place the identity tokens are
/// allowed to appear.
const IDENTITY_EXCEPTION: &str = "node";

/// Namespace/dependency tokens for sessions, cookies, JWTs, and OIDC.
const IDENTITY_TOKENS: &[&str] = &[
    "openidconnect",
    "jsonwebtoken",
    "use oauth2",
    "oauth2::",
    "tower-sessions",
    "tower_sessions",
    "axum-extra",
    "axum_extra",
];

/// Crate NAMES for the dependency-graph half of the identity fence.
const IDENTITY_CRATES: &[&str] = &[
    "openidconnect",
    "jsonwebtoken",
    "oauth2",
    "tower-sessions",
    "axum-extra",
];

/// Tenancy identifiers. Fenced under core/ only: the app, the node, and the
/// smoke tier may all name a tenant.
const TENANCY_TOKENS: &[&str] = &["tenant_id", "Tenant", "tenants/"];

fn workspace_root() -> PathBuf {
    // node/ → workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("node sits in the workspace root")
        .to_path_buf()
}

/// Every workspace member, parsed from the root manifest's `members` line so
/// a future crate is fenced the day it is added.
fn members(root: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("members"))
        .expect("workspace manifest declares members");
    let crates: Vec<String> = line
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    assert!(
        crates.len() >= 8 && crates.iter().any(|c| c == IDENTITY_EXCEPTION),
        "member parse looks wrong: {crates:?}"
    );
    crates
}

fn rust_files(dir: &Path, acc: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            rust_files(&path, acc);
        } else if path.extension().is_some_and(|e| e == "rs") {
            acc.push(path);
        }
    }
}

/// Every `.rs` file under `<crate>/src` and `<crate>/tests`.
fn sources(crate_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for sub in ["src", "tests"] {
        let dir = crate_dir.join(sub);
        if dir.is_dir() {
            rust_files(&dir, &mut files);
        }
    }
    files
}

/// Token hits in a set of files, as `path:line: text`.
fn hits(files: &[PathBuf], tokens: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file).unwrap();
        for (i, line) in text.lines().enumerate() {
            for token in tokens {
                if line.contains(token) {
                    out.push(format!(
                        "{}:{}: forbidden token {token:?} in: {}",
                        file.display(),
                        i + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    out
}

/// Dependency DECLARATION lines naming one of `crates` (prose in comments may
/// name them — this very file's rationale does).
fn declarations(manifest: &Path, crates: &[&str]) -> Vec<String> {
    let text = std::fs::read_to_string(manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        for name in crates {
            let declares = line
                .strip_prefix(name)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
                || (line.starts_with('[') && line.contains(&format!("dependencies.{name}")));
            if declares {
                out.push(format!("{}:{}: {line}", manifest.display(), i + 1));
            }
        }
    }
    out
}

#[test]
fn sessions_tokens_and_oidc_live_only_below_the_node_binary() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for krate in members(&root) {
        if krate == IDENTITY_EXCEPTION {
            continue;
        }
        let dir = root.join(&krate);
        assert!(dir.is_dir(), "expected crate dir {}", dir.display());
        violations.extend(hits(&sources(&dir), IDENTITY_TOKENS));
        violations.extend(declarations(&dir.join("Cargo.toml"), IDENTITY_CRATES));
    }
    // Not in [workspace.dependencies] either: if the node ever adopts one of
    // these, it declares it in node/Cargo.toml where the exception is visible.
    violations.extend(declarations(&root.join("Cargo.toml"), IDENTITY_CRATES));

    assert!(
        violations.is_empty(),
        "sessions/cookies/JWTs/OIDC may exist only inside node/ (D16 below it: no accounts, \
         no sessions, no tokens — the console is the node's, and it is designed on paper, \
         not shipped):\n{}",
        violations.join("\n")
    );
}

/// The dependency-graph half: a forbidden crate must not be REACHABLE from
/// any workspace crate but hive-node, however it arrived — a transitive pull
/// through some convenience crate is exactly the way an auth stack sneaks
/// into an engine.
#[test]
fn no_workspace_crate_but_the_node_can_reach_an_identity_crate() {
    let metadata = cargo_metadata();
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("cargo metadata resolve.nodes");
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in nodes {
        let id = node["id"].as_str().expect("package id");
        deps.insert(
            id,
            node["deps"]
                .as_array()
                .expect("deps")
                .iter()
                .map(|d| d["pkg"].as_str().expect("dep pkg"))
                .collect(),
        );
    }
    let name_of: HashMap<&str, &str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| {
            (
                p["id"].as_str().expect("package id"),
                p["name"].as_str().expect("package name"),
            )
        })
        .collect();

    let mut violations = Vec::new();
    for member in metadata["workspace_members"]
        .as_array()
        .expect("workspace_members")
    {
        let member = member.as_str().expect("member id");
        let name = name_of.get(member).copied().unwrap_or(member);
        if name == "hive-node" {
            continue;
        }
        // BFS over the resolved graph, dev-dependency edges included: the
        // fence is about what a crate can pull in at all.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue = vec![member];
        while let Some(id) = queue.pop() {
            if !seen.insert(id) {
                continue;
            }
            let reached = name_of.get(id).copied().unwrap_or(id);
            if IDENTITY_CRATES.contains(&reached) && id != member {
                violations.push(format!("{name} can reach {reached}"));
            }
            queue.extend(deps.get(id).into_iter().flatten().copied());
        }
    }
    assert!(
        violations.is_empty(),
        "an identity/session crate is reachable outside hive-node:\n{}",
        violations.join("\n")
    );
}

/// Sanity: the graph walk actually walked something. A fence that silently
/// inspected nothing would pass forever.
#[test]
fn the_dependency_walk_sees_the_workspace_it_claims_to_fence() {
    let metadata = cargo_metadata();
    let names: BTreeSet<&str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| p["name"].as_str().expect("package name"))
        .collect();
    for expected in ["hive-core", "hive-node", "hive-sync", "rusqlite"] {
        assert!(names.contains(expected), "{expected} missing from metadata");
    }
    assert!(
        metadata["resolve"]["nodes"]
            .as_array()
            .expect("resolve.nodes")
            .len()
            > 50,
        "the resolved graph looks empty"
    );
}

#[test]
fn the_data_plane_stays_tenancy_blind() {
    let root = workspace_root();
    let violations = hits(&sources(&root.join("core")), TENANCY_TOKENS);
    assert!(
        violations.is_empty(),
        "core/ must not learn about tenants (D33: a tenant is grouping, quotas, and console \
         scope; the data plane sees a domain and a device). The English word is fine — these \
         are identifiers:\n{}",
        violations.join("\n")
    );
}

/// The fences are only worth anything if they can fail. Both token lists are
/// checked against text that MUST trip them, so a typo in a constant cannot
/// quietly disarm the gate.
#[test]
fn the_fences_trip_on_what_they_are_meant_to_catch() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("pretend.rs"),
        "use oauth2::TokenResponse;\nfn f() { let t: jsonwebtoken::Header; }\n",
    )
    .unwrap();
    assert_eq!(
        hits(&sources(dir.path()), IDENTITY_TOKENS).len(),
        3,
        "`use oauth2`, `oauth2::`, and `jsonwebtoken`"
    );

    std::fs::write(
        src.join("pretend.rs"),
        "// core is deliberately tenancy-blind: the word tenant is fine here\n\
         struct Tenant { tenant_id: String }\n",
    )
    .unwrap();
    let tenancy = hits(&sources(dir.path()), TENANCY_TOKENS);
    assert_eq!(tenancy.len(), 2, "Tenant and tenant_id, not the prose");

    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[dependencies]\nopenidconnect = \"3\"\n# jsonwebtoken = \"9\" (a comment is prose)\n",
    )
    .unwrap();
    assert_eq!(
        declarations(&dir.path().join("Cargo.toml"), IDENTITY_CRATES).len(),
        1
    );
}

/// The resolved graph, from the cargo that is running this test.
fn cargo_metadata() -> serde_json::Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(workspace_root())
        .output()
        .expect("running `cargo metadata` (the fence needs the resolved graph)");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("cargo metadata is JSON")
}
