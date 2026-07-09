//! End-to-end sync against a REAL Stalwart server — the DIRECTION.md mail
//! milestone, verbatim: multi-page backfill → counts + FTS + cursor, delta
//! (creates + a destroy leaving search within one cycle), a poisoned state
//! string forcing full reconciliation, and a backfill re-run from zero
//! producing zero duplicates.
//!
//! Self-skipping: without `HIVE_TEST_STALWART_URL` this is a no-op so plain
//! `cargo test --workspace` stays green. CI wires the real thing up in the
//! `mail-e2e` job (.github/workflows/ci.yml): the pinned stalwartlabs image
//! with ci/stalwart/config.toml + provision.sh. To run locally:
//!
//! ```sh
//! # (:ro,z — the z is for SELinux hosts, ignored elsewhere)
//! podman run -d --name hive-e2e-stalwart -p 8080:8080 \
//!   -v ./ci/stalwart/config.toml:/config.toml:ro,z --entrypoint sh \
//!   docker.io/stalwartlabs/stalwart:v0.15.5 \
//!   -c 'mkdir -p /opt/stalwart/etc && cp /config.toml /opt/stalwart/etc/config.toml \
//!       && exec /usr/local/bin/stalwart --config /opt/stalwart/etc/config.toml'
//! ci/stalwart/provision.sh
//! HIVE_TEST_STALWART_URL=http://localhost:8080 cargo test -p hive-mail --test stalwart_e2e -- --nocapture
//! ```
//!
//! Fixture mail is seeded over raw JMAP `Email/set` with staggered
//! `receivedAt` (jmap-sync deliberately exposes no set operations — a tiny
//! reqwest helper below speaks the wire shape directly, which also keeps the
//! seeding path independent of the code under test).

use serde_json::{json, Map, Value};
use sqlx::PgPool;

use hive_api::store::Store;
use hive_mail::MailDaemon;

const ADDRESS: &str = "mailtest@example.test";

/// Raw JMAP client for seeding/destroying fixture mail: session discovery,
/// then single-method calls against the advertised apiUrl with basic auth.
struct Jmap {
    http: reqwest::Client,
    api_url: String,
    account_id: String,
    user: String,
    pass: String,
}

impl Jmap {
    async fn connect(base: &str, user: &str, pass: &str) -> Jmap {
        let http = reqwest::Client::new();
        let session: Value = http
            .get(format!("{}/.well-known/jmap", base.trim_end_matches('/')))
            .basic_auth(user, Some(pass))
            .send()
            .await
            .expect("session discovery request")
            .error_for_status()
            .expect("session discovery status")
            .json()
            .await
            .expect("session json");
        let account_id = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
            .as_str()
            .unwrap_or_else(|| panic!("no primary mail account in session: {session}"))
            .to_string();
        let api_url = session["apiUrl"]
            .as_str()
            .expect("apiUrl in session")
            .to_string();
        Jmap {
            http,
            api_url,
            account_id,
            user: user.to_string(),
            pass: pass.to_string(),
        }
    }

    async fn call(&self, method: &str, args: Value) -> Value {
        let body = json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [[method, args, "0"]],
        });
        let resp: Value = self
            .http
            .post(&self.api_url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&body)
            .send()
            .await
            .expect("jmap call")
            .json()
            .await
            .expect("jmap response json");
        let m = &resp["methodResponses"][0];
        assert_eq!(
            m[0].as_str(),
            Some(method),
            "unexpected response to {method}: {resp}"
        );
        m[1].clone()
    }

    async fn inbox_id(&self) -> String {
        let r = self
            .call(
                "Mailbox/get",
                json!({"accountId": self.account_id, "properties": ["id", "role"]}),
            )
            .await;
        r["list"]
            .as_array()
            .expect("mailbox list")
            .iter()
            .find(|m| m["role"].as_str() == Some("inbox"))
            .and_then(|m| m["id"].as_str())
            .expect("server-side inbox mailbox")
            .to_string()
    }

    /// Create messages `start..start+count` in one `Email/set`, with
    /// receivedAt staggered by the index so newest-first backfill pagination
    /// is deterministic. Returns server ids in seed order.
    async fn seed(&self, inbox_id: &str, start: usize, count: usize) -> Vec<String> {
        let mut create = Map::new();
        for i in start..start + count {
            let mut mailbox_ids = Map::new();
            mailbox_ids.insert(inbox_id.to_string(), json!(true));
            create.insert(
                format!("m{i}"),
                json!({
                    "mailboxIds": mailbox_ids,
                    "from": [{"email": "seeder@example.test", "name": "Seeder"}],
                    "to": [{"email": ADDRESS}],
                    "subject": format!("e2e seed {i}"),
                    "receivedAt": format!("2026-06-01T00:{:02}:00Z", i),
                    "bodyStructure": {"partId": "p1", "type": "text/plain"},
                    "bodyValues": {"p1": {"value": format!("fixture body {i} with honeycomb")}},
                }),
            );
        }
        let r = self
            .call(
                "Email/set",
                json!({"accountId": self.account_id, "create": create}),
            )
            .await;
        (start..start + count)
            .map(|i| {
                r["created"][&format!("m{i}")]["id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("m{i} not created: {r}"))
                    .to_string()
            })
            .collect()
    }

    async fn destroy(&self, id: &str) {
        let r = self
            .call(
                "Email/set",
                json!({"accountId": self.account_id, "destroy": [id]}),
            )
            .await;
        let destroyed = r["destroyed"]
            .as_array()
            .map(|a| a.iter().any(|v| v.as_str() == Some(id)))
            .unwrap_or(false);
        assert!(destroyed, "server refused to destroy {id}: {r}");
    }

    /// Destroy every message in the account. The assertions below are
    /// absolute counts, so re-runs against a lived-in local server must
    /// start from zero (CI's container is fresh anyway; the hive side is a
    /// fresh schema either way).
    async fn purge(&self) {
        loop {
            let q = self
                .call("Email/query", json!({"accountId": self.account_id}))
                .await;
            let ids = q["ids"].as_array().cloned().unwrap_or_default();
            if ids.is_empty() {
                return;
            }
            self.call(
                "Email/set",
                json!({"accountId": self.account_id, "destroy": ids}),
            )
            .await;
        }
    }
}

async fn scalar_i64(pool: &PgPool, sql: &str, bind: &str) -> i64 {
    hive_api::pgq::query_scalar::<i64>(sql)
        .bind(bind)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

struct Counts {
    live: i64,
    tombstoned: i64,
    search: i64,
    duplicates: i64,
}

async fn counts(pool: &PgPool, account_id: &str) -> Counts {
    Counts {
        live: scalar_i64(
            pool,
            "SELECT COUNT(*) FROM mail_messages WHERE account_id = ? AND deleted_at IS NULL",
            account_id,
        )
        .await,
        tombstoned: scalar_i64(
            pool,
            "SELECT COUNT(*) FROM mail_messages WHERE account_id = ? AND deleted_at IS NOT NULL",
            account_id,
        )
        .await,
        search: scalar_i64(
            pool,
            "SELECT COUNT(*) FROM search s WHERE s.kind = 'mail' AND EXISTS \
             (SELECT 1 FROM mail_messages m WHERE m.id = s.ref_id AND m.account_id = ?)",
            account_id,
        )
        .await,
        // THE zero-duplicates probe: replays/reruns must be absorbed by
        // UNIQUE(account_id, jmap_id), never duplicated.
        duplicates: scalar_i64(
            pool,
            "SELECT COUNT(*) FROM (SELECT account_id, jmap_id FROM mail_messages \
             WHERE account_id = ? GROUP BY account_id, jmap_id HAVING COUNT(*) > 1) d",
            account_id,
        )
        .await,
    }
}

async fn backfill_status(pool: &PgPool, account_id: &str) -> String {
    hive_api::pgq::query_scalar::<String>("SELECT backfill_status FROM mail_accounts WHERE id = ?")
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("backfill_status")
}

async fn email_state(pool: &PgPool, account_id: &str) -> Option<String> {
    hive_api::pgq::query_scalar::<String>("SELECT email_state FROM mail_accounts WHERE id = ?")
        .bind(account_id)
        .fetch_optional(pool)
        .await
        .expect("email_state")
}

/// One supervised sync cycle. `run_account_once` never propagates sync
/// errors (the daemon's supervision marks the account failed and backs off
/// instead), so a passing call proves nothing — check the account's own
/// last_status and surface last_error when a cycle silently failed.
async fn run_cycle(daemon: &MailDaemon, pool: &PgPool, account_id: &str, what: &str) {
    daemon
        .run_account_once(account_id)
        .await
        .unwrap_or_else(|e| panic!("{what}: {e:#}"));
    let status = hive_api::pgq::query_scalar::<String>(
        "SELECT COALESCE(last_status, '') FROM mail_accounts WHERE id = ?",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await
    .expect("last_status");
    if status != "ok" {
        let err = hive_api::pgq::query_scalar::<String>(
            "SELECT COALESCE(last_error, '') FROM mail_accounts WHERE id = ?",
        )
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("last_error");
        panic!("{what}: cycle marked {status:?}: {err}");
    }
}

/// Drive sync cycles until backfill reports complete. The page budget
/// (50 pages/cycle) means small fixtures finish in one cycle, but the loop
/// mirrors how the real daemon converges across ticks.
async fn run_until_backfill_complete(daemon: &MailDaemon, pool: &PgPool, account_id: &str) {
    for cycle in 1..=20 {
        run_cycle(daemon, pool, account_id, &format!("backfill cycle {cycle}")).await;
        if backfill_status(pool, account_id).await == "complete" {
            return;
        }
    }
    panic!(
        "backfill not complete after 20 cycles (status {})",
        backfill_status(pool, account_id).await
    );
}

#[tokio::test]
async fn backfill_delta_forced_resync_and_rerun_produce_zero_duplicates() {
    let Ok(url) = std::env::var("HIVE_TEST_STALWART_URL") else {
        eprintln!(
            "skipping stalwart_e2e: HIVE_TEST_STALWART_URL is not set \
             (see mail/tests/stalwart_e2e.rs header to run it locally)"
        );
        return;
    };
    let user = std::env::var("HIVE_TEST_STALWART_USER").unwrap_or_else(|_| ADDRESS.to_string());
    let pass =
        std::env::var("HIVE_TEST_STALWART_PASS").unwrap_or_else(|_| "mailtest-pass".to_string());

    // Vault key for the account credential (CI passes its own); page size 10
    // forces the 25-message backfill across multiple pages; the 1s doorbell
    // keeps each cycle from parking on the 300s poll default.
    if std::env::var_os("HIVE_CRED_KEY").is_none() {
        std::env::set_var("HIVE_CRED_KEY", "stalwart-e2e-test-key");
    }
    std::env::set_var("HIVE_MAIL_PAGE_SIZE", "10");
    std::env::set_var("HIVE_MAIL_POLL_SECS", "1");

    let pool = hive_api::db::test_pool().await;
    let store = Store::new(pool.clone());
    let daemon = MailDaemon::new(pool.clone());

    // Connect the account through the store fn (the route's session-discovery
    // validation is out of scope here; the daemon discovers + persists the
    // JMAP account id on its first cycle).
    let acct = store
        .mail_account_create("nate", ADDRESS, &url, Some(&user), "", &pass)
        .await
        .expect("mail_account_create");

    let jmap = Jmap::connect(&url, &user, &pass).await;
    jmap.purge().await;
    let inbox = jmap.inbox_id().await;
    let seeded = jmap.seed(&inbox, 1, 25).await;
    eprintln!("seeded 25 messages into server inbox {inbox}");

    // Cycle 1: mailboxes sync; nothing is opted into ingest yet, so backfill
    // completes vacuously and only captures the state baseline.
    run_cycle(&daemon, &pool, &acct.id, "first sync cycle").await;
    let c = counts(&pool, &acct.id).await;
    assert_eq!(c.live, 0, "no mailbox is ingest-enabled yet");

    // Opt the Inbox in (resets backfill to pending), then backfill for real:
    // 25 messages at page size 10 = a multi-page run.
    let inbox_row = store
        .mail_mailboxes_list(&acct.id)
        .await
        .expect("mailboxes")
        .into_iter()
        .find(|b| b.role.as_deref() == Some("inbox"))
        .expect("synced inbox mailbox row");
    store
        .mail_mailbox_set_ingest(&inbox_row.id, true)
        .await
        .expect("enable ingest");
    assert_eq!(backfill_status(&pool, &acct.id).await, "pending");

    run_until_backfill_complete(&daemon, &pool, &acct.id).await;
    let c = counts(&pool, &acct.id).await;
    assert_eq!(c.live, 25, "backfill stored every seeded message");
    assert_eq!(c.search, 25, "every ingested message is FTS-searchable");
    assert_eq!(c.duplicates, 0);
    let state = email_state(&pool, &acct.id).await.expect("email_state set");
    assert!(!state.is_empty(), "backfill left a delta cursor behind");
    eprintln!("backfill complete: 25 rows + 25 search rows, email_state={state}");

    // Delta: 3 new arrivals + 1 server-side destroy, one poll cycle later.
    let _extra = jmap.seed(&inbox, 26, 3).await;
    jmap.destroy(&seeded[0]).await;
    run_cycle(&daemon, &pool, &acct.id, "delta cycle").await;
    let c = counts(&pool, &acct.id).await;
    assert_eq!(c.live, 27, "3 created, 1 destroyed");
    assert_eq!(c.tombstoned, 1, "the destroy tombstoned, not deleted");
    assert_eq!(
        c.search, 27,
        "the destroyed message left search in the same cycle"
    );
    assert_eq!(c.duplicates, 0);
    let dead_id = hive_api::pgq::query_scalar::<String>(
        "SELECT id FROM mail_messages WHERE account_id = ? AND jmap_id = ? AND deleted_at IS NOT NULL",
    )
    .bind(&acct.id)
    .bind(&seeded[0])
    .fetch_optional(&pool)
    .await
    .expect("tombstone lookup")
    .expect("destroyed message is tombstoned");
    let dead_search = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = ?",
        &dead_id,
    )
    .await;
    assert_eq!(dead_search, 0, "tombstoned message has no search row");
    eprintln!("delta OK: 27 live + 1 tombstone, dead search row gone");

    // Forced resync: the sentinel poisons the state string; the next changes
    // call must route into full reconciliation (Stalwart rejects the garbage
    // state) and reconcile must change NOTHING — same counts, no duplicates,
    // and a fresh usable state string.
    assert!(store
        .mail_account_force_resync(&acct.id)
        .await
        .expect("force_resync"));
    run_cycle(&daemon, &pool, &acct.id, "forced resync cycle").await;
    let c = counts(&pool, &acct.id).await;
    assert_eq!(
        (c.live, c.tombstoned, c.search),
        (27, 1, 27),
        "reconcile is a no-op on synced data"
    );
    assert_eq!(c.duplicates, 0, "reconcile re-created nothing");
    let state = email_state(&pool, &acct.id)
        .await
        .expect("state after resync");
    assert!(
        !state.is_empty() && state != "force-resync",
        "reconcile replaced the poisoned state, got {state:?}"
    );
    eprintln!("forced resync OK: counts unchanged, state re-captured");

    // The DIRECTION milestone, verbatim: re-running backfill from zero over
    // an already-synced account produces zero duplicates.
    hive_api::pgq::query(
        "UPDATE mail_accounts SET backfill_status = 'pending', backfill_cursor = NULL WHERE id = ?",
    )
    .bind(&acct.id)
    .execute(&pool)
    .await
    .expect("reset backfill");
    run_until_backfill_complete(&daemon, &pool, &acct.id).await;
    let c = counts(&pool, &acct.id).await;
    assert_eq!(
        (c.live, c.tombstoned, c.search),
        (27, 1, 27),
        "re-run changed nothing"
    );
    assert_eq!(
        c.duplicates, 0,
        "backfill re-run from zero produced zero duplicates"
    );
    eprintln!("backfill re-run from zero OK: zero duplicates — milestone holds");
}
