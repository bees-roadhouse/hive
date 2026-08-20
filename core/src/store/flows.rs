// The flow registry: pluggable workflows and their run log (docs/FLOWS.md).
// The manifest is the unit of authorship — stored verbatim, validated on
// write, and the source the dynamic MCP section (`flow_<slug>_<op>` tools)
// is derived from. `builtin` flows dispatch to native store functions
// (api/src/mcp.rs); `wasm` flows execute behind the `flow-exec` feature (F3).

use anyhow::{Context, Result};
use hive_shared::{Flow, FlowKind, FlowManifest, FlowOpDef, FlowRun, FlowTrigger};
use serde_json::{json, Value};
use sqlx::Row;

use super::{new_id, now_iso, Store};

/// A rejected registry write, rendered for the caller. Store failures keep
/// propagating as errors.
#[derive(Debug)]
pub enum FlowWriteError {
    Invalid(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for FlowWriteError {
    fn from(e: anyhow::Error) -> Self {
        FlowWriteError::Other(e)
    }
}

impl From<sqlx::Error> for FlowWriteError {
    fn from(e: sqlx::Error) -> Self {
        FlowWriteError::Other(e.into())
    }
}

/// The guest ABI major version this build speaks (docs/FLOWS.md D2).
pub const FLOW_ABI: i64 = 1;

/// Slugs allow no underscores and ops allow no hyphens, so the MCP tool name
/// `flow_<slug>_<op>` parses unambiguously: strip `flow_`, the first `_`
/// ends the slug (docs/FLOWS.md D6).
fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_op(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn validate_manifest(m: &FlowManifest) -> Result<(), FlowWriteError> {
    if !valid_slug(&m.slug) {
        return Err(FlowWriteError::Invalid(format!(
            "slug '{}' is invalid — lowercase letters, digits and hyphens, no underscores (^[a-z][a-z0-9-]*$, max 48)",
            m.slug
        )));
    }
    if m.name.trim().is_empty() {
        return Err(FlowWriteError::Invalid("name required".into()));
    }
    if m.abi != FLOW_ABI {
        return Err(FlowWriteError::Invalid(format!(
            "abi {} is not supported (this build speaks {FLOW_ABI})",
            m.abi
        )));
    }
    let mut seen: Vec<&str> = Vec::new();
    for op in &m.operations {
        if !valid_op(&op.name) {
            return Err(FlowWriteError::Invalid(format!(
                "operation '{}' is invalid — lowercase letters, digits and underscores, no hyphens (^[a-z][a-z0-9_]*$, max 48)",
                op.name
            )));
        }
        if seen.contains(&op.name.as_str()) {
            return Err(FlowWriteError::Invalid(format!(
                "operation '{}' is declared twice",
                op.name
            )));
        }
        seen.push(&op.name);
    }
    Ok(())
}

/// The builtin `wire` flow — the existing signal ingestion described in the
/// registry's own terms (docs/FLOWS.md D4). Its ops dispatch to native store
/// functions in api/src/mcp.rs; the manifest here is what tools/list serves.
pub fn wire_builtin_manifest() -> FlowManifest {
    FlowManifest {
        slug: "wire".to_string(),
        name: "The Wire".to_string(),
        description: "External signal ingestion: feeds and page monitors polled into wire events, \
             plus namespaced flow signal storage."
            .to_string(),
        version: "0.1.0".to_string(),
        abi: FLOW_ABI,
        operations: vec![
            FlowOpDef {
                name: "recent".to_string(),
                title: "Recent wire events".to_string(),
                description: "The wire log, newest first — the same feed GET /api/wire serves."
                    .to_string(),
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1, "maximum": 500}
                    },
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#"
                })),
                admin: false,
            },
            FlowOpDef {
                name: "emit".to_string(),
                title: "Emit a wire event".to_string(),
                description:
                    "Store a wire event and fan it to live listeners. The kind is namespaced \
                     `flow.wire.<kind>` — flow signal can never forge lifecycle kinds."
                        .to_string(),
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "description": "event kind suffix; stored as flow.wire.<kind>"},
                        "payload": {"description": "arbitrary JSON payload"}
                    },
                    "required": ["kind"],
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#"
                })),
                admin: false,
            },
            FlowOpDef {
                name: "poll".to_string(),
                title: "Poll sources now".to_string(),
                description:
                    "Poll due feed/scrape sources into wire events (the POST /api/sources/poll \
                     ingestion). Admin only."
                        .to_string(),
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "poll one source regardless of interval; omit for all due"}
                    },
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#"
                })),
                admin: true,
            },
        ],
        triggers: vec![FlowTrigger {
            kind: "schedule".to_string(),
            config: json!({"note": "per-source interval_secs; the F4 scheduler owns firing this"}),
        }],
        allowed_hosts: Vec::new(),
    }
}

impl Store {
    /// Make sure the builtin flows exist in the acting org, and keep their
    /// stored manifests current with this build's definition. Idempotent and
    /// cheap in the steady state (one SELECT, no writes), so the read paths
    /// (REST list, MCP tools/list) call it freely — which is what makes the
    /// registry self-seeding per org instead of a boot-time special case.
    pub async fn flows_ensure_builtins(&self) -> Result<()> {
        let manifest = wire_builtin_manifest();
        let manifest_json = super::to_json(&manifest);
        let existing = crate::pgq::query("SELECT id, manifest FROM flows WHERE slug = ?")
            .bind(&manifest.slug)
            .fetch_optional(self.db())
            .await?;
        match existing {
            None => {
                let now = now_iso();
                let id = new_id("flow");
                let inserted = crate::pgq::query(
                    "INSERT INTO flows (id, slug, name, description, version, abi, kind, module_sha256, manifest, enabled, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, 'builtin', NULL, ?, TRUE, 'system', ?, ?) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(&id)
                .bind(&manifest.slug)
                .bind(&manifest.name)
                .bind(&manifest.description)
                .bind(&manifest.version)
                .bind(manifest.abi)
                .bind(&manifest_json)
                .bind(&now)
                .bind(&now)
                .execute(self.db())
                .await?
                .rows_affected();
                // ON CONFLICT covers two ensure calls racing: the loser's
                // insert is a no-op and must not double-emit.
                if inserted > 0 {
                    self.emit(
                        "flow.registered",
                        "system",
                        json!({"id": id, "slug": manifest.slug, "kind": "builtin"}),
                    )
                    .await?;
                }
            }
            Some(row) => {
                // A new release can ship a richer builtin manifest; heal the
                // stored copy so old orgs surface the current ops.
                let stored: String = row.try_get("manifest")?;
                if stored != manifest_json {
                    let id: String = row.try_get("id")?;
                    crate::pgq::query(
                        "UPDATE flows SET name = ?, description = ?, version = ?, abi = ?, manifest = ?, updated_at = ? WHERE id = ?",
                    )
                    .bind(&manifest.name)
                    .bind(&manifest.description)
                    .bind(&manifest.version)
                    .bind(manifest.abi)
                    .bind(&manifest_json)
                    .bind(now_iso())
                    .bind(&id)
                    .execute(self.db())
                    .await?;
                    self.emit(
                        "flow.updated",
                        "system",
                        json!({"id": id, "slug": manifest.slug}),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// Every registered flow (disabled included — the list is the management
    /// surface; tools/list filters on `enabled` itself).
    pub async fn flows_list(&self) -> Result<Vec<Flow>> {
        let rows = crate::pgq::query("SELECT * FROM flows ORDER BY slug")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_flow).collect()
    }

    pub async fn flows_get(&self, slug: &str) -> Result<Option<Flow>> {
        let row = crate::pgq::query("SELECT * FROM flows WHERE slug = ?")
            .bind(slug)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_flow).transpose()
    }

    /// Register (or re-register) a flow from its manifest. Upserts by slug:
    /// re-installing a module updates version/manifest/module in place and
    /// keeps the row's enabled state and provenance. The F3 install path
    /// (module upload → artifact storage → this call) and tests use it;
    /// builtins go through [`Store::flows_ensure_builtins`] instead.
    pub async fn flows_register(
        &self,
        manifest: FlowManifest,
        kind: FlowKind,
        module_sha256: Option<&str>,
        actor: &str,
    ) -> Result<Flow, FlowWriteError> {
        validate_manifest(&manifest)?;
        if kind == FlowKind::Wasm && module_sha256.is_none() {
            return Err(FlowWriteError::Invalid(
                "a wasm flow needs module_sha256 (the content address of its module)".into(),
            ));
        }
        let manifest_json = super::to_json(&manifest);
        let now = now_iso();
        let existing: Option<String> =
            crate::pgq::query_scalar::<String>("SELECT id FROM flows WHERE slug = ?")
                .bind(&manifest.slug)
                .fetch_optional(self.db())
                .await
                .context("flows_register probe")?;
        let (id, event) = match existing {
            Some(id) => {
                crate::pgq::query(
                    "UPDATE flows SET name = ?, description = ?, version = ?, abi = ?, kind = ?, module_sha256 = ?, manifest = ?, updated_at = ? WHERE id = ?",
                )
                .bind(&manifest.name)
                .bind(&manifest.description)
                .bind(&manifest.version)
                .bind(manifest.abi)
                .bind(kind.as_str())
                .bind(module_sha256)
                .bind(&manifest_json)
                .bind(&now)
                .bind(&id)
                .execute(self.db())
                .await
                .context("flows_register update")?;
                (id, "flow.updated")
            }
            None => {
                let id = new_id("flow");
                crate::pgq::query(
                    "INSERT INTO flows (id, slug, name, description, version, abi, kind, module_sha256, manifest, enabled, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, TRUE, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&manifest.slug)
                .bind(&manifest.name)
                .bind(&manifest.description)
                .bind(&manifest.version)
                .bind(manifest.abi)
                .bind(kind.as_str())
                .bind(module_sha256)
                .bind(&manifest_json)
                .bind(actor)
                .bind(&now)
                .bind(&now)
                .execute(self.db())
                .await
                .context("flows_register insert")?;
                (id, "flow.registered")
            }
        };
        self.emit(
            event,
            actor,
            json!({"id": id, "slug": manifest.slug, "kind": kind.as_str()}),
        )
        .await?;
        let flow = self
            .flows_get(&manifest.slug)
            .await?
            .context("registered flow vanished")?;
        Ok(flow)
    }

    /// Flip a flow on or off. A disabled flow keeps its row and its runs;
    /// its tools simply stop being served (docs/FLOWS.md D6).
    pub async fn flows_set_enabled(
        &self,
        slug: &str,
        enabled: bool,
        actor: &str,
    ) -> Result<Option<Flow>> {
        let changed =
            crate::pgq::query("UPDATE flows SET enabled = ?, updated_at = ? WHERE slug = ?")
                .bind(enabled)
                .bind(now_iso())
                .bind(slug)
                .execute(self.db())
                .await?
                .rows_affected();
        if changed == 0 {
            return Ok(None);
        }
        self.emit(
            if enabled {
                "flow.enabled"
            } else {
                "flow.disabled"
            },
            actor,
            json!({"slug": slug}),
        )
        .await?;
        self.flows_get(slug).await
    }

    /// The registry-driven MCP section: one tool per operation of every
    /// enabled flow, named `flow_<slug>_<op>`, schema served as declared
    /// (docs/FLOWS.md D6). Deterministic order: slug, then manifest order.
    pub async fn flows_mcp_tools(&self) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        for flow in self.flows_list().await? {
            if !flow.enabled {
                continue;
            }
            for op in &flow.manifest.operations {
                let title = if op.title.is_empty() {
                    format!("{}: {}", flow.name, op.name)
                } else {
                    op.title.clone()
                };
                let description = if op.description.is_empty() {
                    flow.description.clone()
                } else {
                    op.description.clone()
                };
                let schema = op.input_schema.clone().unwrap_or_else(|| {
                    json!({
                        "type": "object",
                        "properties": {},
                        "$schema": "http://json-schema.org/draft-07/schema#"
                    })
                });
                tools.push(json!({
                    "name": format!("flow_{}_{}", flow.slug, op.name),
                    "title": title,
                    "description": format!(
                        "{description} (operation `{}` of the installed `{}` flow, v{})",
                        op.name, flow.slug, flow.version
                    ),
                    "inputSchema": schema,
                    "execution": {"taskSupport": "forbidden"}
                }));
            }
        }
        Ok(tools)
    }

    /// Record one execution. Runs are written after the fact in this slice
    /// (started/finished both known); the F3 host moves to write-then-finish
    /// when runs get long enough to watch.
    #[allow(clippy::too_many_arguments)]
    pub async fn flow_run_record(
        &self,
        flow_id: &str,
        op: &str,
        trigger: &str,
        status: &str,
        input: &Value,
        output: Option<&Value>,
        error: Option<&str>,
        started_at: &str,
        actor: &str,
    ) -> Result<FlowRun> {
        let run = FlowRun {
            id: new_id("frun"),
            flow_id: flow_id.to_string(),
            op: op.to_string(),
            trigger: trigger.to_string(),
            status: status.to_string(),
            input: input.clone(),
            output: output.cloned(),
            error: error.map(String::from),
            started_at: started_at.to_string(),
            finished_at: now_iso(),
            created_by: actor.to_string(),
            created_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO flow_runs (id, flow_id, op, trigger, status, input, output, error, started_at, finished_at, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run.id)
        .bind(&run.flow_id)
        .bind(&run.op)
        .bind(&run.trigger)
        .bind(&run.status)
        .bind(run.input.to_string())
        .bind(run.output.as_ref().map(Value::to_string))
        .bind(&run.error)
        .bind(&run.started_at)
        .bind(&run.finished_at)
        .bind(&run.created_by)
        .bind(&run.created_at)
        .execute(self.db())
        .await?;
        Ok(run)
    }

    /// Run history for one flow, newest first.
    pub async fn flow_runs_list(&self, flow_id: &str, limit: i64) -> Result<Vec<FlowRun>> {
        let rows = crate::pgq::query(
            "SELECT * FROM flow_runs WHERE flow_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(flow_id)
        .bind(limit)
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_run).collect()
    }
}

fn row_to_flow(r: &sqlx::postgres::PgRow) -> Result<Flow> {
    let manifest_raw: String = r.try_get("manifest")?;
    let slug: String = r.try_get("slug")?;
    let manifest: FlowManifest = serde_json::from_str(&manifest_raw)
        .with_context(|| format!("flow '{slug}' has an unparseable stored manifest"))?;
    Ok(Flow {
        id: r.try_get("id")?,
        slug,
        name: r.try_get("name")?,
        description: r.try_get("description")?,
        version: r.try_get("version")?,
        abi: r.try_get("abi")?,
        kind: FlowKind::from_str_lossy(r.try_get::<String, _>("kind")?.as_str()),
        module_sha256: r.try_get("module_sha256")?,
        manifest,
        enabled: r.try_get("enabled")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

fn row_to_run(r: &sqlx::postgres::PgRow) -> Result<FlowRun> {
    let input: String = r.try_get("input")?;
    let output: Option<String> = r.try_get("output")?;
    Ok(FlowRun {
        id: r.try_get("id")?,
        flow_id: r.try_get("flow_id")?,
        op: r.try_get("op")?,
        trigger: r.try_get("trigger")?,
        status: r.try_get("status")?,
        input: serde_json::from_str(&input).unwrap_or(Value::Null),
        output: output.map(|o| serde_json::from_str(&o).unwrap_or(Value::Null)),
        error: r.try_get("error")?,
        started_at: r.try_get("started_at")?,
        finished_at: r.try_get("finished_at")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wasm_manifest(slug: &str) -> FlowManifest {
        FlowManifest {
            slug: slug.to_string(),
            name: "Test Flow".to_string(),
            description: "a test".to_string(),
            version: "1.2.3".to_string(),
            abi: FLOW_ABI,
            operations: vec![FlowOpDef {
                name: "run_once".to_string(),
                title: String::new(),
                description: String::new(),
                input_schema: None,
                admin: false,
            }],
            triggers: Vec::new(),
            allowed_hosts: Vec::new(),
        }
    }

    #[test]
    fn slug_and_op_charsets_keep_tool_names_parseable() {
        assert!(valid_slug("wire"));
        assert!(valid_slug("watch-the-wire"));
        assert!(!valid_slug("has_underscore"));
        assert!(!valid_slug("Caps"));
        assert!(!valid_slug(""));
        assert!(valid_op("mark_read"));
        assert!(!valid_op("has-hyphen"));
        assert!(!valid_op("9leading"));
    }

    #[tokio::test]
    async fn builtins_seed_once_and_heal_their_manifest() {
        let test_db = crate::db::test_pool().await;
        let store = Store::new(test_db.pool.clone());

        store.flows_ensure_builtins().await.unwrap();
        store.flows_ensure_builtins().await.unwrap();
        let flows = store.flows_list().await.unwrap();
        assert_eq!(flows.len(), 1, "ensure is idempotent");
        let wire = &flows[0];
        assert_eq!(wire.slug, "wire");
        assert_eq!(wire.kind, FlowKind::Builtin);
        let ops: Vec<&str> = wire
            .manifest
            .operations
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        assert_eq!(ops, vec!["recent", "emit", "poll"]);

        // A drifted stored manifest (an older release's) heals to current.
        crate::pgq::query("UPDATE flows SET manifest = '{\"slug\":\"wire\",\"name\":\"Old\"}' WHERE slug = 'wire'")
            .execute(store.db())
            .await
            .unwrap();
        store.flows_ensure_builtins().await.unwrap();
        let wire = store.flows_get("wire").await.unwrap().unwrap();
        assert_eq!(wire.manifest.operations.len(), 3, "manifest healed");
    }

    #[tokio::test]
    async fn register_validates_and_upserts_by_slug() {
        let test_db = crate::db::test_pool().await;
        let store = Store::new(test_db.pool.clone());

        // Bad slug (underscore) and bad ABI both refuse cleanly.
        let mut bad = wasm_manifest("bad_slug");
        match store
            .flows_register(
                bad.clone(),
                FlowKind::Wasm,
                Some("aa".repeat(32).as_str()),
                "nate",
            )
            .await
        {
            Err(FlowWriteError::Invalid(msg)) => assert!(msg.contains("slug"), "{msg}"),
            other => panic!("expected invalid slug, got {other:?}"),
        }
        bad.slug = "ok-slug".to_string();
        bad.abi = 99;
        match store
            .flows_register(bad, FlowKind::Wasm, Some("aa".repeat(32).as_str()), "nate")
            .await
        {
            Err(FlowWriteError::Invalid(msg)) => assert!(msg.contains("abi"), "{msg}"),
            other => panic!("expected invalid abi, got {other:?}"),
        }
        // A wasm flow without a module address refuses.
        match store
            .flows_register(wasm_manifest("no-module"), FlowKind::Wasm, None, "nate")
            .await
        {
            Err(FlowWriteError::Invalid(msg)) => assert!(msg.contains("module_sha256"), "{msg}"),
            other => panic!("expected missing module error, got {other:?}"),
        }

        let sha = "ab".repeat(32);
        let flow = store
            .flows_register(wasm_manifest("scout"), FlowKind::Wasm, Some(&sha), "nate")
            .await
            .unwrap();
        assert_eq!(flow.kind, FlowKind::Wasm);
        assert_eq!(flow.module_sha256.as_deref(), Some(sha.as_str()));

        // Re-register (a new module version) updates in place, keeps identity.
        let mut v2 = wasm_manifest("scout");
        v2.version = "2.0.0".to_string();
        let again = store
            .flows_register(v2, FlowKind::Wasm, Some(&sha), "nate")
            .await
            .unwrap();
        assert_eq!(again.id, flow.id, "upsert by slug keeps the row");
        assert_eq!(again.version, "2.0.0");
        assert_eq!(store.flows_list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mcp_tools_derive_from_enabled_flows_only() {
        let test_db = crate::db::test_pool().await;
        let store = Store::new(test_db.pool.clone());
        store.flows_ensure_builtins().await.unwrap();
        store
            .flows_register(
                wasm_manifest("scout"),
                FlowKind::Wasm,
                Some("cd".repeat(32).as_str()),
                "nate",
            )
            .await
            .unwrap();

        let names: Vec<String> = store
            .flows_mcp_tools()
            .await
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "flow_scout_run_once",
                "flow_wire_recent",
                "flow_wire_emit",
                "flow_wire_poll"
            ]
        );

        // Disabling a flow withdraws its tools; the row and identity remain.
        store
            .flows_set_enabled("scout", false, "nate")
            .await
            .unwrap()
            .unwrap();
        let names: Vec<String> = store
            .flows_mcp_tools()
            .await
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with("flow_scout")));
        assert_eq!(names.len(), 3);
    }

    #[tokio::test]
    async fn runs_record_and_list() {
        let test_db = crate::db::test_pool().await;
        let store = Store::new(test_db.pool.clone());
        store.flows_ensure_builtins().await.unwrap();
        let wire = store.flows_get("wire").await.unwrap().unwrap();

        let started = now_iso();
        store
            .flow_run_record(
                &wire.id,
                "recent",
                "mcp",
                "ok",
                &json!({"limit": 5}),
                Some(&json!({"count": 0})),
                None,
                &started,
                "pia",
            )
            .await
            .unwrap();
        store
            .flow_run_record(
                &wire.id,
                "emit",
                "mcp",
                "error",
                &json!({}),
                None,
                Some("kind required"),
                &started,
                "pia",
            )
            .await
            .unwrap();

        let runs = store.flow_runs_list(&wire.id, 10).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].op, "emit", "newest first");
        assert_eq!(runs[0].status, "error");
        assert_eq!(runs[1].input, json!({"limit": 5}));
        assert_eq!(runs[1].output, Some(json!({"count": 0})));
    }
}
