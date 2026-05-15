use hive_db::Pool;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
}
