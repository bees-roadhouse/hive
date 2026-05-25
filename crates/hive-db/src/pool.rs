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

/// Per-connection PRAGMAs applied at checkout.
fn configure_connection(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}
