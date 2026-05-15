use hive_db::default_db_path;

pub fn run() -> anyhow::Result<()> {
    let pool = super::pool(true)?;
    drop(pool); // pool init applies the schema on first checkout
    println!("initialized {}", default_db_path().display());
    Ok(())
}
