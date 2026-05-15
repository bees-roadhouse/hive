pub mod graph;
pub mod home;
pub mod journal;
pub mod notes;
pub mod tasks;
pub mod wire;

pub mod layout;

use hive_db::Pool;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
}

pub(crate) async fn with_conn<F, T>(state: &AppState, f: F) -> Result<T, axum::http::StatusCode>
where
    F: FnOnce(&hive_db::Connection) -> hive_db::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let pool = state.pool.clone();
    tokio::task::spawn_blocking(move || -> hive_db::Result<T> {
        let conn = pool.get().map_err(hive_db::Error::from)?;
        f(&conn)
    })
    .await
    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|e| {
        tracing::error!(error = %e, "db error");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    })
}
