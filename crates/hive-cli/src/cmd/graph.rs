use anyhow::Result;

use hive_db::queries::graph::{self, GraphOptions};

use crate::cli::GraphArgs;
use crate::format::print_json;

pub fn run(args: GraphArgs) -> Result<()> {
    let pool = super::pool(false)?;
    let conn = pool.get()?;
    let payload = graph::build(
        &conn,
        GraphOptions {
            min_tag_count: args.min,
            limit_tags: args.tags,
            limit_nodes: args.nodes,
            include_meta: args.include_meta,
        },
    )?;
    print_json(&payload)?;
    Ok(())
}
