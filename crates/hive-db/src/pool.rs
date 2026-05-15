use std::path::{Path, PathBuf};

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::schema::SCHEMA_SQL;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

/// Resolve the default DB path: `$HIVE_DB` if set, otherwise `~/.hive/hive.db`.
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIVE_DB") {
        return PathBuf::from(p);
    }
    if let Some(home) = directories::UserDirs::new().and_then(|u| u.home_dir().to_path_buf().into())
    {
        return home.join(".hive").join("hive.db");
    }
    PathBuf::from("hive.db")
}

/// Open a connection pool against the given DB path.
///
/// `create_if_missing` mirrors the python `db(create_if_missing=...)` helper:
/// when false and the file is absent, returns `Error::DbNotFound`. When true,
/// the parent directory is created and the schema is applied.
pub fn open_pool(path: &Path, create_if_missing: bool, max_size: u32) -> Result<Pool> {
    if !path.exists() && !create_if_missing {
        return Err(Error::DbNotFound(path.to_path_buf()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE)
        .with_init(configure_connection);

    let pool = r2d2::Pool::builder()
        .max_size(max_size)
        .build(manager)?;

    // Apply schema (idempotent) on first checkout.
    {
        let conn = pool.get()?;
        conn.execute_batch(SCHEMA_SQL)?;
    }
    Ok(pool)
}

/// Per-connection setup applied at connect time by r2d2.
///
/// `r2d2_sqlite`'s `with_init` callback in 0.25 invokes pragmas via a path
/// that fails on `PRAGMA journal_mode = WAL` (it returns the new mode as a
/// row, which trips `Error::ExecuteReturnedResults`). The original code used
/// `pragma_update`, which has the same problem at the rusqlite layer. WAL
/// mode is persistent on the database file, so we don't need to set it on
/// every connection — `~/.hive/hive.db` is already in WAL mode from the
/// python toolchain. `foreign_keys` is per-connection, but read-only display
/// (hive-ui v1) doesn't depend on it; future writers can re-enable it via a
/// safer code path (`pragma_query_value` for journal_mode, plain SET for
/// foreign_keys).
fn configure_connection(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}
