//! The hive-mail daemon (DIRECTION.md D4): a third long-lived binary that
//! drives jmap-sync's backfill/delta/reconcile loops per account. It is NOT a
//! worker tick stage — a permanently open EventSource can't live in the
//! worker's abort-on-error cycle, and a multi-year backfill inside a stage
//! would starve heartbeat, feeds, and outbox for hours.
//!
//! Single daemon instance per deployment (matching the outbox single-writer
//! assumption); one task per due account, re-spawned each tick.

mod sink;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hive_api::store::mail::MailAccountSync;
use hive_api::store::Store;
use hive_shared::InboxReason;
use jmap_sync::{BackfillOutcome, BackfillState, CursorStore, DoorbellWake, SyncConfig, Syncer};
use sink::{StoreCursor, StoreSink};

pub fn mail_enabled() -> bool {
    std::env::var("HIVE_MAIL_ENABLED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Backfill pages consumed per account task run; the cursor commits per page,
/// so hitting the budget just resumes on the next tick. Keeps one giant
/// mailbox from monopolizing a task for hours.
const PAGE_BUDGET: u64 = 50;

pub struct MailDaemon {
    store: Store,
}

impl MailDaemon {
    pub fn new(pool: sqlx::PgPool) -> Self {
        MailDaemon {
            store: Store::new(pool),
        }
    }

    pub async fn run(self) -> Result<()> {
        let tick = Duration::from_secs(env_u64("HIVE_MAIL_TICK", 15));
        let mut tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        tracing::info!(tick_secs = tick.as_secs(), "hive-mail daemon up");
        loop {
            tasks.retain(|_, handle| !handle.is_finished());
            match self.store.mail_accounts_due().await {
                Ok(due) => {
                    for acct in due {
                        if tasks.contains_key(&acct.id) {
                            continue;
                        }
                        let store = self.store.clone();
                        let id = acct.id.clone();
                        tasks.insert(id, tokio::spawn(account_task(store, acct)));
                    }
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "account scan failed"),
            }
            tokio::time::sleep(tick).await;
        }
    }

    /// One sequential pass over every due account — the CI/e2e entry point.
    pub async fn run_once(&self) -> Result<()> {
        for acct in self.store.mail_accounts_due().await? {
            account_task(self.store.clone(), acct).await;
        }
        Ok(())
    }

    /// Drive one specific account for tests, bypassing the due filter.
    pub async fn run_account_once(&self, account_id: &str) -> Result<()> {
        let acct = self
            .store
            .mail_accounts_due()
            .await?
            .into_iter()
            .find(|a| a.id == account_id)
            .ok_or_else(|| anyhow!("account {account_id} not due or not enabled"))?;
        account_task(self.store.clone(), acct).await;
        Ok(())
    }
}

/// Top-level per-account supervision: success resets the backoff; failure
/// applies it, and the 8th consecutive failure disables the account and
/// notifies its owner (fail loud, never retry-forever silently).
async fn account_task(store: Store, acct: MailAccountSync) {
    let id = acct.id.clone();
    let owner = acct.owner.clone();
    let address = acct.address.clone();
    match sync_account(&store, acct).await {
        Ok(()) => {
            if let Err(e) = store.mail_account_mark_ok(&id).await {
                tracing::error!(account = %id, error = %format!("{e:#}"), "mark_ok failed");
            }
        }
        Err(e) => {
            let error = format!("{e:#}");
            tracing::warn!(account = %id, %address, %error, "account sync failed");
            match store.mail_account_mark_failed(&id, &error).await {
                Ok(true) => {
                    tracing::error!(account = %id, %address, "disabled after repeated failures");
                    let _ = store
                        .inbox_add(
                            &owner,
                            "hive-mail",
                            InboxReason::Mail,
                            "mail_account",
                            &id,
                            None,
                            &format!(
                                "mail account {address} disabled after repeated sync failures"
                            ),
                        )
                        .await;
                    let _ = store
                        .emit(
                            "mail.account.disabled",
                            &owner,
                            serde_json::json!({"id": id}),
                        )
                        .await;
                }
                Ok(false) => {}
                Err(e2) => {
                    tracing::error!(account = %id, error = %format!("{e2:#}"), "mark_failed failed")
                }
            }
        }
    }
}

/// One sync cycle: connect → refresh mailboxes → backfill (page-budgeted) →
/// delta drain → one doorbell wait → drain again on a wake. The task then
/// ends; the next tick re-spawns it. Reconnecting per cycle costs one session
/// discovery call every poll interval and keeps supervision trivial.
async fn sync_account(store: &Store, acct: MailAccountSync) -> Result<()> {
    let cred_id = acct
        .cred_id
        .as_deref()
        .ok_or_else(|| anyhow!("account has no stored credential"))?;
    let secret = store
        .cc_cred_decrypt_by_id(cred_id)
        .await
        .context("credential vault")?
        .ok_or_else(|| anyhow!("credential row {cred_id} is gone"))?;

    let username = acct
        .jmap_username
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| acct.address.clone());
    let mut cfg = SyncConfig::new(&acct.jmap_url, username, secret);
    if !acct.jmap_account_id.is_empty() {
        cfg.account_id = Some(acct.jmap_account_id.clone());
    }
    cfg.max_body_bytes = env_u64("HIVE_MAIL_MAX_BODY_BYTES", cfg.max_body_bytes as u64) as usize;
    // Mostly a test knob: the Stalwart e2e sets 10 to force a multi-page
    // backfill out of a small fixture mailbox.
    cfg.page_size = env_u64("HIVE_MAIL_PAGE_SIZE", cfg.page_size as u64) as usize;

    let (ingest, _) = store.mail_mailbox_sets(&acct.id).await?;
    cfg.ingest_mailbox_ids = ingest.into_iter().collect();
    let page_sleep = Duration::from_millis(cfg.page_sleep_ms);

    let mut syncer = Syncer::connect(cfg).await?;
    if acct.jmap_account_id.is_empty() {
        store
            .mail_account_set_jmap_id(&acct.id, syncer.account_id())
            .await?;
    }

    // Mailboxes refresh every cycle (cheap at household N); new rows arrive
    // with ingest=FALSE — opting in is operator intent via Settings.
    let (boxes, mailbox_state) = syncer.list_mailboxes().await?;
    let rows: Vec<(String, String, Option<String>, i64)> = boxes
        .into_iter()
        .map(|b| (b.jmap_id, b.name, b.role, b.sort_order))
        .collect();
    store.mail_sync_mailboxes(&acct.id, &rows).await?;

    let cursor_store = StoreCursor {
        store: store.clone(),
        account_id: acct.id.clone(),
    };
    let mut cursor = cursor_store.load().await?;
    cursor.mailbox_state = Some(mailbox_state);
    cursor_store.save(&cursor).await?;

    // The ingest set may have just gained its first mailboxes.
    let (ingest_ids, inbox_ids) = store.mail_mailbox_sets(&acct.id).await?;
    let backfilling = cursor.backfill != BackfillState::Complete;
    let sink = StoreSink {
        store: store.clone(),
        account_id: acct.id.clone(),
        owner: acct.owner.clone(),
        ingest_ids,
        inbox_ids,
        // Suppression holds for this whole cycle even when backfill completes
        // mid-cycle: the first delta drain replays whatever changed during
        // backfill, and notifying on that replay would still storm.
        suppress_events: backfilling,
    };

    if backfilling {
        let mut pages = 0u64;
        loop {
            match syncer.run_backfill(&cursor_store, &sink).await? {
                BackfillOutcome::Complete => {
                    store
                        .emit(
                            "mail.backfill.completed",
                            &acct.owner,
                            serde_json::json!({"account_id": acct.id}),
                        )
                        .await?;
                    break;
                }
                BackfillOutcome::Page { fetched } => {
                    pages += 1;
                    tracing::debug!(account = %acct.id, pages, fetched, "backfill page");
                    if pages % 50 == 0 {
                        store
                            .emit(
                                "mail.backfill.progress",
                                &acct.owner,
                                serde_json::json!({"account_id": acct.id, "pages": pages}),
                            )
                            .await?;
                    }
                    if pages >= PAGE_BUDGET {
                        // Cursor is committed per page — resume next tick.
                        return Ok(());
                    }
                    tokio::time::sleep(page_sleep).await;
                }
            }
        }
    }

    let outcome = syncer.run_delta(&cursor_store, &sink).await?;
    if outcome.resynced {
        tracing::info!(account = %acct.id, created = outcome.created, destroyed = outcome.destroyed, "full reconciliation ran");
    }

    let poll = Duration::from_secs(env_u64("HIVE_MAIL_POLL_SECS", 300));
    if syncer.wait_doorbell(poll).await == DoorbellWake::Change {
        syncer.run_delta(&cursor_store, &sink).await?;
    }
    Ok(())
}
