// Architecture-guarding test (PLAN.md "cross-cutting"): the durable layer is
// deterministic. core/src/oplog and core/src/blockstore never read clocks,
// environment, or randomness — ts/seq/lc arrive from callers, key material
// arrives through the keys::KeySource seam, and every nonce/segment key is
// derived, not sampled (the modules document each derivation).
//
// keys.rs is deliberately OUTSIDE this fence: generating a fresh master key
// and Argon2 salts is exactly the place OS randomness belongs.
//
// This is a source-text grep on purpose — cheap, total, and impossible to
// dodge without editing this list in review.

use std::path::{Path, PathBuf};

/// Substrings that must not appear anywhere (code or comments) in the
/// fenced modules. The first five are the canonical set from PLAN.md/PR 1.4;
/// the rest close the equivalent side doors.
const FORBIDDEN: &[&str] = &[
    "SystemTime",
    "now_iso",
    "Utc::now",
    "rand::",
    "thread_rng",
    "OsRng",
    "getrandom",
    "Instant::now",
    "std::env",
    "env::var",
];

/// Modules under the determinism fence, relative to core/src/.
const FENCED: &[&str] = &["oplog", "blockstore"];

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

#[test]
fn oplog_and_blockstore_are_clock_env_and_randomness_free() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    for module in FENCED {
        let dir = src.join(module);
        assert!(dir.is_dir(), "expected fenced module dir {}", dir.display());
        rust_files(&dir, &mut files);
    }
    assert!(!files.is_empty(), "determinism fence found no source files");

    let mut violations = Vec::new();
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
    assert!(
        violations.is_empty(),
        "the durable layer must stay deterministic (take inputs from callers, \
         key material via keys::KeySource):\n{}",
        violations.join("\n")
    );
}
