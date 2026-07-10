// SQLCipher spike (PR 1.4, PLAN.md: "Day-one spike: rusqlite
// bundled-sqlcipher + FTS5 + JSON1 compiles") — de-risks the PR 1.5 fold
// target before anything is built on it.
//
// The feature combo, worked out from libsqlite3-sys 0.38's build.rs:
//
//   rusqlite = { version = "0.40", features = ["bundled-sqlcipher-vendored-openssl"] }
//
//   - "bundled-sqlcipher-vendored-openssl" → implies bundled-sqlcipher →
//     bundled: libsqlite3-sys compiles the SQLCipher amalgamation (SQLite
//     3.51.x + -DSQLITE_HAS_CODEC) instead of stock sqlite3.c, and builds
//     OpenSSL from source for the codec — no system OpenSSL needed, which is
//     the whole point for the mac/Windows bundles.
//   - FTS5 needs no feature: libsqlite3-sys passes -DSQLITE_ENABLE_FTS5
//     unconditionally for every bundled build.
//   - JSON needs no feature either: json_* has been core SQLite since 3.38
//     (the old JSON1 extension flag is obsolete).
//
// This test proves all three on Linux in one go: PRAGMA key encrypts the
// file (wrong key can't read it, the on-disk header is not plaintext
// SQLite), FTS5 virtual tables MATCH, and json_extract works. macOS and
// Windows proof is deferred to the Phase 2.5 CI bundle matrix (PLAN.md) —
// there is no such runner in this repo's CI yet.

use rusqlite::Connection;

#[test]
fn sqlcipher_fts5_json_spike() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spike.db");

    {
        let conn = Connection::open(&path).unwrap();
        // Must be the first statement against the connection.
        conn.pragma_update(None, "key", "hive-spike-passphrase")
            .unwrap();

        // Prove this is really SQLCipher, not stock SQLite: stock returns no
        // row for cipher_version.
        let cipher_version: String = conn
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .expect("PRAGMA cipher_version returned nothing — not a SQLCipher build");
        assert!(!cipher_version.is_empty());

        // FTS5: virtual table, insert, MATCH, bm25 ordering callable.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE notes USING fts5(body);
             INSERT INTO notes(body) VALUES
               ('the rust rewrite ships this week'),
               ('groceries: honey, oats, tea'),
               ('rust segments rotate at eight mebibytes');",
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM notes WHERE notes MATCH 'rust'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 2);
        let top: String = conn
            .query_row(
                "SELECT body FROM notes WHERE notes MATCH 'rust AND rewrite' ORDER BY bm25(notes) LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(top.contains("rewrite"));

        // JSON core: json_extract over nested structure.
        let v: i64 = conn
            .query_row(
                r#"SELECT json_extract('{"a":{"b":[10,20,30]}}', '$.a.b[1]')"#,
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 20);
    }

    // The file on disk must not be a plaintext SQLite database.
    let head = std::fs::read(&path).unwrap();
    assert!(
        !head.starts_with(b"SQLite format 3\0"),
        "database file is unencrypted"
    );

    // Reopening with the wrong key must fail to read anything.
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "key", "wrong-passphrase").unwrap();
    let res: rusqlite::Result<i64> =
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0));
    assert!(res.is_err(), "wrong key still read the database");

    // And the right key still works after all of the above.
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "key", "hive-spike-passphrase")
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3);
}
