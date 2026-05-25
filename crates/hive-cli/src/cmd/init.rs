//! `hive init`. In the HTTP-client world there is no local DB to create ...
//! schema/migrations are hive-api's job. `init` is repurposed as a
//! connectivity check: hit `GET /healthz` and report the resolved API base.
//!
//! GAP NOTE: python `hive.py init` created the sqlite file. There is no API
//! endpoint for "initialize storage" (and there shouldn't be ... that's a
//! server-side migration concern). This health-check is the closest faithful
//! analog; flagged to the lead as a behavioral change.

use crate::api;

pub async fn run() -> anyhow::Result<()> {
    let base = api::api_base();
    api::healthz()
        .await
        .map_err(|e| anyhow::anyhow!("hive-api not reachable at {base}: {e}"))?;
    println!("hive-api reachable at {base}");
    Ok(())
}
