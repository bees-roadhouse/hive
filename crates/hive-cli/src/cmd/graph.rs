use anyhow::Result;

use crate::api;
use crate::cli::GraphArgs;
use crate::format::print_json;

pub async fn run(args: GraphArgs) -> Result<()> {
    let payload = api::graph(args.min, args.tags, args.nodes, args.include_meta).await?;
    print_json(&payload)?;
    Ok(())
}
