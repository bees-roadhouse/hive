pub mod graph;
pub mod init;
pub mod journal;
pub mod links;
pub mod notes;
pub mod search;
pub mod tasks;
pub mod wire;

use hive_db::{Pool, default_db_path, open_pool};

/// Open the pool against the default DB path (`$HIVE_DB` or `~/.hive/hive.db`).
/// `create_if_missing` is true only for `hive init`.
pub fn pool(create_if_missing: bool) -> hive_db::Result<Pool> {
    let path = default_db_path();
    open_pool(&path, create_if_missing, 1)
}
