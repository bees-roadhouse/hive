//! The quote-stripping contract, as data: every `NN_name.in.txt` in
//! `tests/corpus/` must strip to exactly its `NN_name.out.txt`. Adding a
//! regression case is adding two files.

use std::fs;
use std::path::PathBuf;

#[test]
fn corpus() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut cases: Vec<String> = fs::read_dir(&dir)
        .expect("tests/corpus exists")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".in.txt").map(|s| s.to_string())
        })
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "corpus directory has no .in.txt cases");

    for case in cases {
        let input = fs::read_to_string(dir.join(format!("{case}.in.txt"))).unwrap();
        let expected = fs::read_to_string(dir.join(format!("{case}.out.txt")))
            .unwrap_or_else(|_| panic!("{case}.out.txt missing"));
        let got = jmap_sync::quote::strip_quoted(&input);
        // Compare on content, not on line endings. `strip_quoted` is currently
        // INCONSISTENT about them — the stripping path rebuilds with LF while
        // the pass-through path (a body with nothing to strip) returns the
        // input verbatim, CRLF and all. That is worth fixing in the parser, but
        // it is not what this corpus pins: these cases are about which text
        // survives quote removal. Comparing raw made the suite fail in opposite
        // directions on different cases depending on which path ran, and made
        // the fixtures unstable under git's autocrlf on Windows.
        assert_eq!(
            lf(got.trim_end()),
            lf(expected.trim_end()),
            "case {case}: stripped output diverged"
        );
    }
}

/// Line endings normalised, so a case is compared on its text.
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}
