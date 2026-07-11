// Architecture-guarding grep (PLAN.md PR 1.7): "no Postgres in hive" stays
// auditable because sqlx lives ONLY in importer/Cargo.toml. Every other
// workspace crate must be free of sqlx/pgvector — no dependency declaration
// in its Cargo.toml, no token anywhere in its sources (code or comments,
// same total-grep posture as core/tests/determinism.rs). Runs everywhere
// (no database needed), so the main CI job enforces it too.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &["sqlx", "pgvector"];

fn workspace_root() -> PathBuf {
    // importer/ → workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("importer sits in the workspace root")
        .to_path_buf()
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

/// Every workspace member except importer/ is fenced — parsed from the root
/// manifest's `members` line so a future crate can't dodge the gate by
/// simply not being listed here.
fn fenced_crates(root: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let members_line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("members"))
        .expect("workspace manifest declares members");
    let crates: Vec<String> = members_line
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|m| *m != "importer")
        .map(str::to_string)
        .collect();
    assert!(
        crates.len() >= 6,
        "member parse looks wrong: {crates:?} from {members_line:?}"
    );
    crates
}

#[test]
fn only_the_importer_speaks_postgres() {
    let root = workspace_root();
    let mut violations = Vec::new();

    for krate in &fenced_crates(&root) {
        let crate_dir = root.join(krate);
        assert!(
            crate_dir.is_dir(),
            "expected crate dir {}",
            crate_dir.display()
        );

        // Sources: total grep — a Postgres token has no business even in a
        // fenced crate's comments-adjacent code (tests included).
        let mut files = Vec::new();
        for sub in ["src", "tests"] {
            let dir = crate_dir.join(sub);
            if dir.is_dir() {
                rust_files(&dir, &mut files);
            }
        }
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            for (lineno, line) in text.lines().enumerate() {
                for token in FORBIDDEN {
                    if line.contains(token) {
                        violations.push(format!(
                            "{}:{}: forbidden token {token:?} in: {}",
                            file.display(),
                            lineno + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }

        // Manifests: no dependency DECLARATION (prose in comments may name
        // sqlx — e.g. the workspace manifest documents this very rule).
        for (lineno, line) in manifest_lines(&crate_dir.join("Cargo.toml")) {
            violations.push(format!(
                "{krate}/Cargo.toml:{lineno}: Postgres dependency declared outside importer/: {line}"
            ));
        }
    }
    for (lineno, line) in manifest_lines(&root.join("Cargo.toml")) {
        violations.push(format!(
            "Cargo.toml:{lineno}: Postgres dependency declared in the workspace manifest: {line}"
        ));
    }

    assert!(
        violations.is_empty(),
        "sqlx/pgvector may live only under importer/ (PLAN.md PR 1.7):\n{}",
        violations.join("\n")
    );
}

/// Dependency-declaration lines mentioning a forbidden crate: `sqlx =`,
/// `pgvector =`, or a `[dependencies.sqlx]`-style table header.
fn manifest_lines(path: &Path) -> Vec<(usize, String)> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut hits = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        let declares = FORBIDDEN.iter().any(|t| {
            line.strip_prefix(t)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
                || (line.starts_with('[') && line.contains(&format!("dependencies.{t}")))
        });
        if declares {
            hits.push((i + 1, line.to_string()));
        }
    }
    hits
}
