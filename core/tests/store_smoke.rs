// Store-level smoke — the surviving assertions from the retired
// api/tests/parity_smoke.rs (journal emergence, search, recall, dashboard,
// actor merge previews, inbox semantics, custom entities), rewritten to drive
// `Store` directly. The router/auth/onboarding/token/OAuth/ACL halves of that
// suite died with their subjects in the PR 1.3 teardown.

mod common;

use std::sync::OnceLock;

use hive_core::store::custom_entities::{EntityFilter, EntityWriteError};
use hive_core::store::recall::RecallOptions;
use hive_core::store::tasks::TaskFilter;
use hive_core::store::Store;
use hive_shared::{ActorKind, NewJournalEntry, TaskPatch, TaskStatus};
use serde_json::{json, Map, Value};

/// Latch the deterministic hash provider before any embed call (the provider
/// choice is once-per-process).
fn hash_setup() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| std::env::set_var("HIVE_EMBED", "hash"));
    assert_eq!(hive_embed::embed_dim(), 256, "hash provider must be active");
}

async fn test_store() -> Store {
    hash_setup();
    common::test_store().await
}

fn journal_input(body: &str) -> NewJournalEntry {
    serde_json::from_value(json!({"body": body})).expect("journal input")
}

#[tokio::test]
async fn journal_emergence_search_recall_dashboard() {
    let store = test_store().await;
    store.people_ensure("pia", ActorKind::Ai).await.unwrap();

    // Journal append: mentions fan the inbox, anchors emerge a task.
    let entry_body = "Kickoff with @pia. We must ship the rust rewrite this week.";
    let task_start = entry_body.find("We must").unwrap() as i64;
    let input: NewJournalEntry = serde_json::from_value(json!({
        "body": entry_body,
        "tags": ["rewrite"],
        "anchors": [{
            "start": task_start,
            "end": entry_body.len(),
            "kind": "task",
            "fields": {"title": "Ship the rust rewrite", "assignees": ["pia"], "priority": "high"}
        }]
    }))
    .unwrap();
    let view = store
        .journal_append(input, Some("nate"), Some("nate"))
        .await
        .unwrap();
    assert_eq!(view.entry.author, "nate");
    assert_eq!(view.entry.mentions, vec!["pia".to_string()]);
    assert_eq!(view.anchors.len(), 1);
    let task_id = view.anchors[0].anchor.ref_id.clone();

    // The anchored task exists with the anchor fields applied.
    let task = store.tasks_get(&task_id).await.unwrap().expect("task");
    assert_eq!(task.title, "Ship the rust rewrite");
    assert_eq!(task.priority.as_str(), "high");
    assert_eq!(task.assignees, vec!["pia".to_string()]);
    assert_eq!(
        task.origin_entry_id.as_deref(),
        Some(view.entry.id.as_str())
    );

    // Mention + assignment landed in pia's inbox.
    let inbox = store.inbox_list("pia", true).await.unwrap();
    assert!(!inbox.is_empty(), "pia inbox should have entries");

    // Task workflow update bumps status.
    let patch = TaskPatch {
        status: Some(TaskStatus::Doing),
        ..Default::default()
    };
    let task = store
        .tasks_update(&task_id, patch, "nate")
        .await
        .unwrap()
        .expect("updated task");
    assert_eq!(task.status, TaskStatus::Doing);
    let doing = store
        .tasks_list(TaskFilter {
            status: Some("doing".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(doing.len(), 1);

    // FTS search finds the entry; semantic mode works on the hash provider.
    let hits = store.search("rewrite", 25).await.unwrap();
    assert!(!hits.is_empty(), "fts hits: {hits:?}");
    let sem = store
        .semantic_search("rust rewrite", Default::default())
        .await
        .unwrap();
    drop(sem); // hash provider path must not error

    // Recall exposes the structured shape: journal hit metadata nests under
    // `hit`, while author/created_at sit beside it; the brief is injectable.
    store.backfill_embeddings().await.unwrap();
    let recall = store
        .recall(
            "pia",
            RecallOptions {
                peer: Some("nate".into()),
                query: Some("Kickoff".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        recall.brief.contains("Recall for pia"),
        "recall brief should be ready to inject: {}",
        recall.brief
    );
    assert!(!recall.data.journal.is_empty(), "recall journal hits");
    assert_eq!(recall.data.journal[0].hit.kind, "journal");
    assert!(recall.data.journal[0].hit.title.contains("Kickoff"));
    assert!(!recall.data.journal[0].author.is_empty());

    // Dashboard composes.
    let dash = store.dashboard().await.unwrap();
    assert_eq!(dash.entries, 1);
    assert_eq!(dash.tasks.doing, 1);

    // Actor merge dryRun reports counts without mutating.
    store.people_ensure("cera", ActorKind::Ai).await.unwrap();
    let preview = store.actors_merge_preview("pia", "cera").await.unwrap();
    assert!(preview.dry_run);
    let pia = store.people_get("pia").await.unwrap();
    assert!(pia.is_some(), "dryRun must not delete the actor");
}

#[tokio::test]
async fn inbox_roundtrip_and_self_notification_skip() {
    let store = test_store().await;

    // Self-notification is silently skipped.
    let none = store
        .inbox_add(
            "nate",
            "nate",
            hive_shared::InboxReason::Mention,
            hive_shared::EntityKind::Journal.as_str(),
            "jrnl_x",
            None,
            "self ping",
        )
        .await
        .unwrap();
    assert!(none.is_none(), "don't notify yourself");

    let item = store
        .inbox_add(
            "pia",
            "nate",
            hive_shared::InboxReason::Mention,
            hive_shared::EntityKind::Journal.as_str(),
            "jrnl_y",
            None,
            "a snippet",
        )
        .await
        .unwrap()
        .expect("delivered");
    assert_eq!(store.inbox_unread_count("pia").await.unwrap(), 1);

    // Mark by id; a second mark and a missing id both report zero rows.
    assert_eq!(store.inbox_mark_read(&item.id).await.unwrap(), 1);
    assert_eq!(store.inbox_mark_read(&item.id).await.unwrap(), 0);
    assert_eq!(store.inbox_mark_read("inb_missing").await.unwrap(), 0);
    assert_eq!(store.inbox_unread_count("pia").await.unwrap(), 0);

    // Mark-all clears the remaining unread.
    store
        .inbox_add(
            "pia",
            "nate",
            hive_shared::InboxReason::Mention,
            hive_shared::EntityKind::Journal.as_str(),
            "jrnl_z",
            None,
            "another",
        )
        .await
        .unwrap();
    assert_eq!(store.inbox_mark_all_read("pia").await.unwrap(), 1);
}

/// Writes still stamp `user_scope` (storage-shape stability for the 1.6
/// cutover / 1.7 importer) even though single-user reads no longer filter:
/// every entry is readable regardless of its stored scope.
#[tokio::test]
async fn journal_writes_stamp_user_scope_and_reads_are_unscoped() {
    let store = test_store().await;

    let scoped = store
        .journal_append(
            journal_input("Pia remembers a plugin setup detail."),
            Some("pia"),
            Some("nate"),
        )
        .await
        .unwrap();
    assert_eq!(scoped.entry.user_scope.as_deref(), Some("nate"));

    let global = store
        .journal_append(
            journal_input("A continuous-history note."),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(global.entry.user_scope, None);

    let all = store.journal_list(50, 0).await.unwrap();
    assert_eq!(all.len(), 2, "reads see every entry, scoped or not");
    let one = store.journal_get(&scoped.entry.id).await.unwrap();
    assert!(one.is_some(), "scoped entries read back unscoped");
}

/// Recall's kinds filter runs INSIDE semantic_search (post-filtering would let
/// other kinds crowd the 8-hit pool toward empty), and an unknown-kind
/// embeddings row (written by a newer binary) must not hold result slots: it
/// fails hydration and is dropped BEFORE the final cut. Both survived the
/// PR 1.3 teardown from the retired scoped-search test.
#[tokio::test]
async fn recall_filters_kinds_in_search_and_unknown_kinds_drop() {
    let store = test_store().await;

    store
        .journal_append(
            journal_input("alpha hive inspection notes from the west garden"),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    // Ten tasks that outscore every journal entry for this query used to fill
    // semantic_search's 8-hit pool before recall's journal post-filter ran,
    // emptying the brief (DIRECTION.md D9).
    for _ in 0..10 {
        store
            .tasks_create(
                hive_core::store::tasks::TaskCreate {
                    title: "queen brood frame audit notes".to_string(),
                    body: "queen brood frame audit notes".to_string(),
                    assignees: vec!["nate".to_string()],
                    ..Default::default()
                },
                "nate",
            )
            .await
            .expect("task create");
    }
    store.backfill_embeddings().await.unwrap();
    let recall = store
        .recall(
            "nate",
            RecallOptions {
                query: Some("queen brood frame audit notes".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("recall");
    assert!(
        !recall.data.journal.is_empty(),
        "task noise crowded journal out of the recall brief"
    );
    assert!(recall.data.journal.iter().all(|h| h.hit.kind == "journal"));

    // A top-scoring embeddings row of a kind this build doesn't know must not
    // starve the result: it fails hydration and drops before the final cut.
    let alien = hive_embed::embed_query("alpha hive inspection notes");
    hive_core::pgq::query(
        "INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at) \
         VALUES ('document', 'doc_alien', ?, ?, ?, 'alien', ?)",
    )
    .bind(hive_embed::embed_model())
    .bind(alien.len() as i64)
    .bind(hive_embed::to_blob(&alien))
    .bind(hive_core::store::now_iso())
    .execute(store.db())
    .await
    .expect("alien embedding row");
    let hits = store
        .semantic_search(
            "alpha hive inspection notes",
            hive_core::store::semantic::SemanticOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !hits.is_empty(),
        "unknown-kind row starved the result: {hits:?}"
    );
    assert!(hits.iter().all(|h| h.id != "doc_alien"));
}

// ---- identity artifacts (from the retired api/tests/artifacts.rs store half;
// the HTTP sync-endpoint + ownership-gating half died with REST/auth) ----

#[tokio::test]
async fn artifact_upsert_is_idempotent_by_actor_kind_name() {
    let store = test_store().await;
    let a = store
        .artifacts_upsert("pia", "skill", "journal", "v1", "first", true)
        .await
        .unwrap();
    let b = store
        .artifacts_upsert("pia", "skill", "journal", "v2", "second", false)
        .await
        .unwrap();

    // Same logical row: id + created_at preserved, content/flags refreshed.
    assert_eq!(a.id, b.id, "upsert must reuse the (actor,kind,name) row");
    assert_eq!(a.created_at, b.created_at);
    assert_eq!(b.content, "v2");
    assert_eq!(b.description, "second");
    assert!(!b.enabled);

    // Exactly one row for that key.
    assert_eq!(store.artifacts_list("pia").await.unwrap().len(), 1);
}

#[tokio::test]
async fn artifact_sync_excludes_disabled_and_other_actors() {
    let store = test_store().await;
    store
        .artifacts_upsert("pia", "skill", "on", "x", "", true)
        .await
        .unwrap();
    store
        .artifacts_upsert("pia", "agent", "off", "x", "", false)
        .await
        .unwrap();
    store
        .artifacts_upsert("apis", "skill", "other", "x", "", true)
        .await
        .unwrap();

    let synced = store.artifacts_for_actor("pia").await.unwrap();
    assert_eq!(synced.len(), 1, "only pia's ENABLED artifacts");
    assert_eq!(synced[0].name, "on");

    // Management listing still sees the disabled one.
    assert_eq!(store.artifacts_list("pia").await.unwrap().len(), 2);
}

fn fields(v: Value) -> Map<String, Value> {
    v.as_object().expect("fields object").clone()
}

#[tokio::test]
async fn custom_entity_types_full_flow() {
    let store = test_store().await;
    let person = store.people_ensure("nate", ActorKind::Human).await.unwrap();

    // Reserved + malformed slugs 400 with bad_slug.
    for bad_slug in ["mail", "task", "Bad Slug", "x"] {
        let input: hive_shared::NewEntityType =
            serde_json::from_value(json!({"name": "X", "slug": bad_slug})).unwrap();
        match store.entity_types_create(input, "nate").await {
            Err(hive_core::store::entity_types::TypeWriteError::Issues(issues)) => {
                assert_eq!(issues[0].code, "bad_slug", "slug {bad_slug}");
            }
            Ok(v) => panic!("slug {bad_slug} must fail, created {}", v.slug),
            Err(_) => panic!("slug {bad_slug} must fail with bad_slug issues"),
        }
    }

    // Create the recipe type; the kind-config contract holds from birth.
    let recipe_type: hive_shared::NewEntityType = serde_json::from_value(json!({
        "name": "Recipe",
        "slug": "recipe",
        "description": "Household recipes",
        "board_field": "status",
        "fields": [
            {"label": "Status", "slug": "status", "field_type": "choice", "options": ["idea", "tested", "keeper"]},
            {"label": "Servings", "slug": "servings", "field_type": "number"},
            {"label": "Cuisine", "slug": "cuisine", "field_type": "text", "required": true},
            {"label": "Author", "slug": "author", "field_type": "ref", "ref_kind": "person"},
        ],
    }))
    .unwrap();
    let ty = store
        .entity_types_create(recipe_type, "nate")
        .await
        .unwrap_or_else(|_| panic!("type create failed"));
    assert_eq!(ty.slug, "recipe");
    assert_eq!(ty.board_field.as_deref(), Some("status"));
    assert_eq!(ty.fields.len(), 4);
    assert_eq!(ty.fields[0].slug, "status");

    // Duplicate slug refused.
    let dup: hive_shared::NewEntityType =
        serde_json::from_value(json!({"name": "Recipe Again", "slug": "recipe"})).unwrap();
    assert!(store.entity_types_create(dup, "nate").await.is_err());

    // A valid instance.
    let sourdough = store
        .custom_entities_create(
            hive_shared::NewCustomEntity {
                type_slug: "recipe".into(),
                title: "Sourdough".into(),
                fields: fields(json!({
                    "status": "keeper", "servings": 4, "cuisine": "bread", "author": person.id
                })),
                scope: None,
            },
            "maggie",
            Some("maggie"),
        )
        .await
        .unwrap_or_else(|_| panic!("create failed"));
    assert_eq!(sourdough.type_slug, "recipe");
    assert!(sourdough.id.starts_with("ent_"));
    assert_eq!(sourdough.user_scope, None, "default scope is global");

    // The validation matrix: unknown key, bad choice, wrong type, missing
    // required, dangling ref — each a structured issue list.
    for (payload, want_code) in [
        (json!({"nope": 1, "cuisine": "a"}), "unknown_field"),
        (json!({"status": "meh", "cuisine": "a"}), "bad_choice"),
        (json!({"servings": "four", "cuisine": "a"}), "wrong_type"),
        (json!({"status": "idea"}), "required"),
        (
            json!({"cuisine": "a", "author": "person_missing"}),
            "ref_not_found",
        ),
    ] {
        let res = store
            .custom_entities_create(
                hive_shared::NewCustomEntity {
                    type_slug: "recipe".into(),
                    title: "X".into(),
                    fields: fields(payload),
                    scope: None,
                },
                "maggie",
                None,
            )
            .await;
        match res {
            Err(EntityWriteError::Issues(issues)) => {
                let codes: Vec<&str> = issues.iter().map(|i| i.code).collect();
                assert!(
                    codes.contains(&want_code),
                    "wanted {want_code} in {codes:?}"
                );
            }
            _ => panic!("expected {want_code} issues"),
        }
    }
    // Unknown type is its own error.
    let res = store
        .custom_entities_create(
            hive_shared::NewCustomEntity {
                type_slug: "gadget".into(),
                title: "X".into(),
                fields: Map::new(),
                scope: None,
            },
            "maggie",
            None,
        )
        .await;
    assert!(matches!(res, Err(EntityWriteError::UnknownType)));

    // A scoped instance still STAMPS user_scope (write value preserved) but
    // is readable in the unscoped list.
    let secret = store
        .custom_entities_create(
            hive_shared::NewCustomEntity {
                type_slug: "recipe".into(),
                title: "Secret Sauce".into(),
                fields: fields(json!({"cuisine": "secret"})),
                scope: Some("me".into()),
            },
            "nate",
            Some("nate"),
        )
        .await
        .unwrap_or_else(|_| panic!("scoped create failed"));
    assert_eq!(secret.user_scope.as_deref(), Some("nate"));
    store
        .custom_entities_create(
            hive_shared::NewCustomEntity {
                type_slug: "recipe".into(),
                title: "Pad Thai".into(),
                fields: fields(json!({"status": "keeper", "servings": 2, "cuisine": "thai"})),
                scope: None,
            },
            "maggie",
            None,
        )
        .await
        .unwrap_or_else(|_| panic!("create failed"));

    // List: equality filter + ascending sort by servings; unscoped list sees
    // the scoped row too (single user).
    let listed = store
        .custom_entities_list(&EntityFilter {
            type_slug: "recipe".into(),
            limit: 100,
            offset: 0,
            sort: Some("servings".into()),
            desc: false,
            fields: vec![("status".into(), "keeper".into())],
        })
        .await
        .unwrap_or_else(|_| panic!("list failed"));
    let titles: Vec<&str> = listed.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, vec!["Pad Thai", "Sourdough"], "filtered+sorted");
    let all = store
        .custom_entities_list(&EntityFilter {
            type_slug: "recipe".into(),
            limit: 100,
            offset: 0,
            sort: None,
            desc: true,
            fields: vec![],
        })
        .await
        .unwrap_or_else(|_| panic!("list failed"));
    assert!(
        all.iter().any(|e| e.title == "Secret Sauce"),
        "unscoped list includes scoped rows"
    );

    // Keyword search reaches instances under the slug kind — scoped ones too.
    let hits = store.search("secret", 25).await.unwrap();
    assert!(hits.iter().any(|h| h.kind == "recipe"), "fts: {hits:?}");
    let hits = store.search("thai", 25).await.unwrap();
    assert!(hits.iter().any(|h| h.kind == "recipe"), "fts: {hits:?}");

    // Unknown sort field fails closed.
    let res = store
        .custom_entities_list(&EntityFilter {
            type_slug: "recipe".into(),
            limit: 100,
            offset: 0,
            sort: Some("bogus".into()),
            desc: false,
            fields: vec![],
        })
        .await;
    assert!(matches!(res, Err(EntityWriteError::Issues(_))));

    // Ref mirroring: sourdough carries a field:author link to the person.
    let links = store.links_for_entity(&sourdough.id).await.unwrap();
    assert!(
        links
            .iter()
            .any(|l| l.rel == "field:author" && l.source_kind == "recipe"),
        "mirror link missing: {links:?}"
    );

    // Patch: null clears a key; the cleared ref's mirror link goes away.
    let patched = store
        .custom_entities_update(
            &sourdough.id,
            hive_shared::CustomEntityPatch {
                title: None,
                fields: Some(fields(json!({"author": null, "servings": 6}))),
                scope: None,
            },
            "maggie",
            None,
        )
        .await
        .unwrap_or_else(|_| panic!("patch failed"))
        .expect("patched");
    assert!(patched.fields.get("author").is_none());
    assert_eq!(patched.fields["servings"], json!(6));
    let links = store.links_for_entity(&sourdough.id).await.unwrap();
    assert!(links.iter().all(|l| l.rel != "field:author"));

    // Registry evolution: archive a field; archived accepted-if-present,
    // never required.
    let patch: hive_shared::EntityTypePatch =
        serde_json::from_value(json!({"update_fields": [{"slug": "cuisine", "archived": true}]}))
            .unwrap();
    store
        .entity_types_update("recipe", patch, "nate")
        .await
        .unwrap_or_else(|_| panic!("field archive failed"))
        .expect("type exists");
    store
        .custom_entities_create(
            hive_shared::NewCustomEntity {
                type_slug: "recipe".into(),
                title: "No Cuisine Needed".into(),
                fields: fields(json!({"status": "idea"})),
                scope: None,
            },
            "maggie",
            None,
        )
        .await
        .unwrap_or_else(|_| panic!("archived field must not be required"));

    // Type lifecycle: delete-with-instances refuses; archive blocks creates.
    assert_eq!(
        store.entity_types_delete("recipe", "nate").await.unwrap(),
        Some(false),
        "delete with instances must refuse"
    );
    let archive: hive_shared::EntityTypePatch =
        serde_json::from_value(json!({"archived": true})).unwrap();
    store
        .entity_types_update("recipe", archive, "nate")
        .await
        .unwrap_or_else(|_| panic!("archive failed"))
        .expect("type exists");
    let res = store
        .custom_entities_create(
            hive_shared::NewCustomEntity {
                type_slug: "recipe".into(),
                title: "Too Late".into(),
                fields: Map::new(),
                scope: None,
            },
            "maggie",
            None,
        )
        .await;
    assert!(matches!(res, Err(EntityWriteError::ArchivedType)));

    // Instance delete drops the row and its search presence.
    store
        .custom_entities_delete(&sourdough.id, "maggie")
        .await
        .unwrap()
        .expect("deleted");
    assert!(store
        .custom_entities_get(&sourdough.id)
        .await
        .unwrap()
        .is_none());
    let hits = store.search("sourdough", 25).await.unwrap();
    assert!(hits.iter().all(|h| h.id != sourdough.id));
}
