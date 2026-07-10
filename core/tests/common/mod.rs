// The one store-construction seam for every hive-core integration test.
//
// PR 1.6 (the SQLite cutover) swapped THIS function to a tempdir data dir +
// in-memory keys + the injected deterministic hash embedder — test bodies
// stayed unchanged. That only works if no test body touches store
// construction directly, so: every integration test builds its store through
// `test_store()` (or `test_store_with` when it needs a different embedder or
// to reopen the same data dir).

use std::sync::Arc;

use hive_core::keys::MemoryKeySource;
use hive_core::store::Store;
use hive_embed::Embedder;

/// Keep each store's tempdir alive for the test's duration (a dropped
/// TempDir deletes the data dir under the writer thread).
static DIRS: std::sync::Mutex<Vec<tempfile::TempDir>> = std::sync::Mutex::new(Vec::new());

/// A Store over a fresh, isolated data dir: tempdir SQLite index + op log,
/// MemoryKeySource, deterministic hash embedder.
#[allow(dead_code)] // each test binary compiles common/ separately; some only use test_store_with
pub async fn test_store() -> Store {
    test_store_with(Arc::new(hive_embed::HashEmbedder))
}

/// Same, with an explicit embedder (the 384-dim mock suites inject theirs).
#[allow(dead_code)]
pub fn test_store_with(embedder: Arc<dyn Embedder>) -> Store {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(dir.path(), Arc::new(MemoryKeySource([7u8; 32])), embedder)
        .expect("open test store");
    DIRS.lock().expect("dirs lock").push(dir);
    store
}
