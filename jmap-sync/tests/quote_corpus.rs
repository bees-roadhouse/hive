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
        assert_eq!(
            got.trim_end(),
            expected.trim_end(),
            "case {case}: stripped output diverged"
        );
    }
}
