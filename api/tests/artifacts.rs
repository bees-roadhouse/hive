// File artifacts over HTTP: bytes in, bytes out, ranged, scoped, deduped.
//
// The assertions that matter are byte-level. A content route that returns the
// right status and the wrong bytes is worse than one that fails, so every read
// here compares the payload itself, and the range cases compare against the
// exact slice they asked for.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use hive_api::store::Store;
use hive_core::artifact_storage::{
    local_storage_at, ArtifactStorage, ArtifactWrite, StagedArtifact, Stored,
};
use serde_json::Value;
use tokio::sync::oneshot;
use tower::ServiceExt;

/// One artifact root for the whole test binary: `storage()` resolves
/// `HIVE_DATA_DIR` once per process, so it has to be set before the first
/// request and cannot change afterwards. Tests stay isolated regardless — each
/// gets its own Postgres schema, hence its own org, hence its own subtree.
fn data_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "hive-artifacts-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("test data root");
        std::env::set_var("HIVE_DATA_DIR", &dir);
        // Same one-shot reason: the driver resolves its ceiling once, at first
        // use. Small enough to cross in a test, comfortably above every payload
        // the other tests in this binary upload.
        std::env::set_var("HIVE_MAX_ARTIFACT_BYTES", MAX_UPLOAD.to_string());
        dir
    })
}

/// The upload ceiling this test binary runs under.
const MAX_UPLOAD: usize = 256 * 1024;

/// The ceiling is resolved once per process. If this ever stops holding, the
/// oversize test below would silently be measuring the 512 MiB default instead
/// — a test that passes by uploading half a gigabyte proves nothing.
#[test]
fn the_test_binary_runs_under_a_small_ceiling() {
    data_root();
    assert_eq!(
        hive_core::artifact_storage::max_artifact_bytes(),
        MAX_UPLOAD as u64
    );
}

async fn app() -> (Router, hive_api::db::TestDb) {
    std::env::set_var("HIVE_EMBED", "hash");
    data_root();
    let test_db = hive_api::db::test_pool().await;
    let s = hive_api::store::Store::new(test_db.pool.clone());
    s.onboarding_complete("Test Hive", "Nate", "nate@example.com", "hunter22-strong")
        .await
        .unwrap();
    (hive_api::routes::router(s), test_db)
}

/// An admin PAT, so requests get past the auth gate.
async fn admin_token(app: &Router) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "nate@example.com", "password": "hunter22-strong"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("login");
    assert_eq!(res.status(), StatusCode::OK, "admin login");
    res.headers()
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

const BOUNDARY: &str = "----hivetestboundary";

fn multipart(filename: &str, mime: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(app: &Router, cookie: &str, filename: &str, mime: &str, bytes: &[u8]) -> Value {
    let req = Request::post("/api/artifacts")
        .header(header::COOKIE, cookie)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart(filename, mime, bytes)))
        .unwrap();
    let res = app.clone().oneshot(req).await.expect("upload");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::CREATED, "upload: {json}");
    json
}

struct Fetched {
    status: StatusCode,
    bytes: Vec<u8>,
    content_range: Option<String>,
    content_length: Option<String>,
    accept_ranges: Option<String>,
    content_type: Option<String>,
}

async fn fetch(app: &Router, cookie: &str, path: &str, range: Option<&str>) -> Fetched {
    let mut req = Request::get(path).header(header::COOKIE, cookie);
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("fetch");
    let status = res.status();
    let header_of = |h: header::HeaderName| {
        res.headers()
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let content_range = header_of(header::CONTENT_RANGE);
    let content_length = header_of(header::CONTENT_LENGTH);
    let accept_ranges = header_of(header::ACCEPT_RANGES);
    let content_type = header_of(header::CONTENT_TYPE);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Fetched {
        status,
        bytes,
        content_range,
        content_length,
        accept_ranges,
        content_type,
    }
}

/// Where a given artifact's bytes should be on disk.
fn object_path(artifact: &Value) -> PathBuf {
    let org = artifact["orgId"].as_str().unwrap().replace('-', "");
    let sha = artifact["sha256"].as_str().unwrap();
    data_root()
        .join("artifacts")
        .join(org)
        .join(&sha[..2])
        .join(sha)
}

/// Big enough that a range is a real seek and small enough to keep the test
/// fast; every byte is distinct mod 251 so a misaligned slice cannot pass.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn bytes_go_in_and_come_out_byte_identical() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;
    let body = payload(64 * 1024 + 7);

    let a = upload(&app, &cookie, "scan.pdf", "application/pdf", &body).await;
    assert_eq!(a["bytes"].as_u64(), Some(body.len() as u64));
    assert_eq!(a["mime"], "application/pdf");
    assert_eq!(a["filename"], "scan.pdf");
    assert_eq!(a["createdBy"], "nate");
    assert_eq!(
        a["sha256"].as_str().unwrap(),
        hex_sha256(&body),
        "the row records the content address of what was actually stored"
    );

    // Metadata round-trips.
    let id = a["id"].as_str().unwrap();
    let meta = fetch(&app, &cookie, &format!("/api/artifacts/{id}"), None).await;
    assert_eq!(meta.status, StatusCode::OK);
    let meta: Value = serde_json::from_slice(&meta.bytes).unwrap();
    assert_eq!(meta["id"], a["id"]);

    // The whole object, byte for byte.
    let got = fetch(&app, &cookie, &format!("/api/artifacts/{id}/content"), None).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.bytes, body, "content must be byte-identical");
    assert_eq!(got.accept_ranges.as_deref(), Some("bytes"));
    assert_eq!(
        got.content_length.as_deref(),
        Some(body.len().to_string().as_str())
    );
    assert_eq!(got.content_type.as_deref(), Some("application/pdf"));
    assert!(got.content_range.is_none(), "a full read is not partial");
}

#[tokio::test]
async fn ranges_return_exactly_the_requested_bytes() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;
    let body = payload(10_000);
    let a = upload(&app, &cookie, "clip.bin", "application/octet-stream", &body).await;
    let path = format!("/api/artifacts/{}/content", a["id"].as_str().unwrap());
    let total = body.len();

    // A closed range in the middle: the classic seek a media player makes.
    let got = fetch(&app, &cookie, &path, Some("bytes=1000-1999")).await;
    assert_eq!(got.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(got.bytes, body[1000..2000], "exactly the requested slice");
    assert_eq!(
        got.content_range.as_deref(),
        Some(format!("bytes 1000-1999/{total}").as_str())
    );
    assert_eq!(got.content_length.as_deref(), Some("1000"));

    // Open-ended: byte N to the end.
    let got = fetch(&app, &cookie, &path, Some("bytes=9990-")).await;
    assert_eq!(got.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(got.bytes, body[9990..]);
    assert_eq!(
        got.content_range.as_deref(),
        Some(format!("bytes 9990-9999/{total}").as_str())
    );

    // Suffix: the final N bytes.
    let got = fetch(&app, &cookie, &path, Some("bytes=-16")).await;
    assert_eq!(got.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(got.bytes, body[total - 16..]);

    // The very first byte, and a range clamped to the end.
    let got = fetch(&app, &cookie, &path, Some("bytes=0-0")).await;
    assert_eq!(got.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(got.bytes, body[0..1]);
    let got = fetch(&app, &cookie, &path, Some("bytes=9995-99999")).await;
    assert_eq!(got.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(got.bytes, body[9995..]);

    // Past the end is 416 and says how long the thing actually is.
    let got = fetch(&app, &cookie, &path, Some("bytes=20000-20010")).await;
    assert_eq!(got.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        got.content_range.as_deref(),
        Some(format!("bytes */{total}").as_str())
    );

    // An unparseable Range is ignored, not rejected.
    let got = fetch(&app, &cookie, &path, Some("furlongs=1-2")).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.bytes, body);
}

#[tokio::test]
async fn duplicate_uploads_share_bytes_and_delete_refcounts() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;
    let body = payload(4096);

    let first = upload(&app, &cookie, "invoice.pdf", "application/pdf", &body).await;
    let second = upload(&app, &cookie, "invoice-copy.pdf", "application/pdf", &body).await;

    // One row per upload, one stored file: the per-upload facts survive.
    assert_ne!(first["id"], second["id"], "each upload is its own artifact");
    assert_eq!(first["sha256"], second["sha256"], "one content address");
    assert_eq!(first["filename"], "invoice.pdf");
    assert_eq!(second["filename"], "invoice-copy.pdf");
    let path = object_path(&first);
    assert_eq!(object_path(&second), path, "both rows point at one file");
    assert!(path.is_file(), "bytes landed at {}", path.display());

    // Deleting one must not pull the bytes out from under the other.
    let del = app
        .clone()
        .oneshot(
            Request::delete(format!("/api/artifacts/{}", first["id"].as_str().unwrap()))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    assert!(
        path.is_file(),
        "the surviving row still references these bytes"
    );

    let still = fetch(
        &app,
        &cookie,
        &format!("/api/artifacts/{}/content", second["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(still.status, StatusCode::OK);
    assert_eq!(still.bytes, body);

    // The deleted row is gone.
    let gone = fetch(
        &app,
        &cookie,
        &format!("/api/artifacts/{}", first["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // Deleting the last reference unlinks the bytes.
    let del = app
        .clone()
        .oneshot(
            Request::delete(format!("/api/artifacts/{}", second["id"].as_str().unwrap()))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    assert!(!path.exists(), "the last reference took the bytes with it");
}

#[tokio::test]
async fn unknown_and_malformed_ids_are_404_not_500() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;

    for path in [
        "/api/artifacts/not-a-uuid",
        "/api/artifacts/00000000-0000-0000-0000-000000000000",
        "/api/artifacts/00000000-0000-0000-0000-000000000000/content",
    ] {
        let got = fetch(&app, &cookie, path, None).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_body_with_no_file_part_is_a_400() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"caption\"\r\n\r\nno file here\r\n--{BOUNDARY}--\r\n"
    );
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/artifacts")
                .header(header::COOKIE, &cookie)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// The upload endpoint is gated like every other non-public API path.
#[tokio::test]
async fn upload_requires_authentication() {
    let (app, _test_db) = app().await;
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/artifacts")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(multipart("x.txt", "text/plain", b"hi")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

// ---- the upload ceiling ----

/// A well-formed multipart body delivered chunk by chunk, counting how many
/// chunks the server actually pulled off the wire. Nothing declares a
/// Content-Length, so the only thing that can stop it early is the server
/// deciding it has read enough.
fn streamed_multipart(chunk: usize, chunks: usize, pulled: Arc<AtomicUsize>) -> Body {
    use axum::body::Bytes;
    use futures::StreamExt;

    let head = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"flood.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    );
    // Each chunk goes Pending before it goes Ready, the way bytes off a socket
    // do. Without that the multipart reader drains the whole body inside a
    // single poll — its buffer loop only stops on Pending or EOF — and there is
    // no "mid-stream" left to measure.
    let data = futures::stream::unfold(0usize, move |i| {
        let pulled = pulled.clone();
        async move {
            if i >= chunks {
                return None;
            }
            tokio::task::yield_now().await;
            pulled.fetch_add(1, Ordering::SeqCst);
            Some((
                Ok::<_, std::io::Error>(Bytes::from(vec![0u8; chunk])),
                i + 1,
            ))
        }
    });
    let tail = futures::stream::once(async move {
        Ok::<_, std::io::Error>(Bytes::from(format!("\r\n--{BOUNDARY}--\r\n")))
    });
    Body::from_stream(
        futures::stream::once(async move { Ok(Bytes::from(head)) })
            .chain(data)
            .chain(tail),
    )
}

/// The cap has to bite WHILE the payload streams. Enforcing it after the body
/// is in hand is not enforcement: the body is the attack, and an unbounded one
/// fills the disk long before anyone gets to check its size.
#[tokio::test]
async fn an_oversize_upload_is_cut_off_mid_stream() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;

    // Learn the acting org's subtree from a normal upload. The ceiling itself
    // is accepted; one byte more is not.
    let small = upload(
        &app,
        &cookie,
        "exact.bin",
        "application/octet-stream",
        &payload(MAX_UPLOAD),
    )
    .await;
    let tmp_dir = data_root()
        .join("artifacts")
        .join(small["orgId"].as_str().unwrap().replace('-', ""))
        .join("tmp");
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/artifacts")
                .header(header::COOKIE, &cookie)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(multipart(
                    "over.bin",
                    "application/octet-stream",
                    &payload(MAX_UPLOAD + 1),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // 16 MiB of body, offered 64 KiB at a time, against a 256 KiB ceiling.
    let chunk = 64 * 1024;
    let offered = 256;
    let pulled = Arc::new(AtomicUsize::new(0));
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/artifacts")
                .header(header::COOKIE, &cookie)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(streamed_multipart(chunk, offered, pulled.clone()))
                .unwrap(),
        )
        .await
        .expect("oversize upload");

    let status = res.status();
    let why = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "pulled {} chunks: {why}",
        pulled.load(Ordering::SeqCst)
    );

    let read = pulled.load(Ordering::SeqCst) * chunk;
    assert!(
        read < 4 * MAX_UPLOAD,
        "rejected after reading {read} bytes against a {MAX_UPLOAD} byte cap — that is buffering, not streaming"
    );

    // And the partial file went with the rejection.
    wait_until_empty(&tmp_dir).await;
    let leftovers = std::fs::read_dir(&tmp_dir)
        .map(|d| d.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(
        leftovers, 0,
        "a rejected upload must not leave a .part file"
    );
}

// ---- the delete/upload race ----

/// A write that stops the world immediately after resolving its content
/// address against what is already stored, and before its row exists.
///
/// That is the exact gap the reported data loss lived in. The old code did the
/// dedup check and the rename in `commit()`, OUTSIDE the advisory lock, and
/// only then locked and inserted — so an upload could dedup-hit a file, a
/// concurrent delete of the last row could count zero and unlink it, and the
/// upload would then commit a row addressing bytes that no longer existed.
struct PausedWrite {
    inner: Box<dyn ArtifactWrite>,
    landed: Option<oneshot::Sender<()>>,
    resume: Option<oneshot::Receiver<()>>,
}

#[async_trait::async_trait]
impl ArtifactWrite for PausedWrite {
    async fn write(&mut self, chunk: &[u8]) -> anyhow::Result<()> {
        self.inner.write(chunk).await
    }

    async fn finish(self: Box<Self>) -> anyhow::Result<Box<dyn StagedArtifact>> {
        let PausedWrite {
            inner,
            landed,
            resume,
        } = *self;
        Ok(Box::new(PausedStaged {
            inner: inner.finish().await?,
            landed,
            resume,
        }))
    }
}

struct PausedStaged {
    inner: Box<dyn StagedArtifact>,
    landed: Option<oneshot::Sender<()>>,
    resume: Option<oneshot::Receiver<()>>,
}

#[async_trait::async_trait]
impl StagedArtifact for PausedStaged {
    fn sha256(&self) -> &str {
        self.inner.sha256()
    }

    fn bytes(&self) -> u64 {
        self.inner.bytes()
    }

    async fn land(self: Box<Self>) -> anyhow::Result<Stored> {
        let PausedStaged {
            inner,
            mut landed,
            mut resume,
        } = *self;
        let stored = inner.land().await?;
        let _ = landed.take().expect("landed once").send(());
        let _ = resume.take().expect("resumed once").await;
        Ok(stored)
    }
}

async fn store_only() -> (Store, hive_api::db::TestDb, tempfile::TempDir, uuid::Uuid) {
    std::env::set_var("HIVE_EMBED", "hash");
    let test_db = hive_api::db::test_pool().await;
    let s = Store::new(test_db.pool.clone());
    s.onboarding_complete("Race Hive", "Nate", "nate@example.com", "hunter22-strong")
        .await
        .unwrap();
    let root = tempfile::tempdir().expect("storage root");
    (s, test_db, root, hive_api::db::DEFAULT_ORG_ID)
}

async fn put(
    s: &Store,
    org: uuid::Uuid,
    storage: &dyn ArtifactStorage,
    name: &str,
    bytes: &[u8],
) -> hive_shared::Artifact {
    let mut w = storage.begin(org).await.unwrap();
    w.write(bytes).await.unwrap();
    s.artifacts_create(org, w, "application/octet-stream", Some(name), Some("nate"))
        .await
        .unwrap()
}

/// The one that matters. A dedup-hit upload is held at the instant it has
/// resolved its content address and has no row; a delete of the LAST row
/// referencing those bytes runs concurrently. Whatever order they finish in,
/// the row that survives has to still serve its bytes.
///
/// Against the old ordering — publish, then lock, then insert — this fails: the
/// delete counts zero rows, unlinks, and the upload commits a row addressing
/// nothing. Verified by reverting the ordering and watching it go red.
#[tokio::test]
async fn a_dedup_hit_racing_the_last_delete_keeps_its_bytes() {
    let (s, _test_db, root, org) = store_only().await;
    let storage = local_storage_at(root.path());
    let body = payload(9_001);

    // Row 1 holds the bytes.
    let first = put(&s, org, &storage, "first.bin", &body).await;
    let sha = first.sha256.clone();
    assert_eq!(
        storage.size(org, &sha).await.unwrap(),
        Some(body.len() as u64)
    );

    // Row 2's upload: the same bytes, held the moment its dedup hit resolves.
    let (landed_tx, landed_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let mut paused: Box<dyn ArtifactWrite> = Box::new(PausedWrite {
        inner: storage.begin(org).await.unwrap(),
        landed: Some(landed_tx),
        resume: Some(resume_rx),
    });
    paused.write(&body).await.unwrap();

    let uploader = s.clone();
    let upload = tokio::spawn(async move {
        uploader
            .artifacts_create(
                org,
                paused,
                "application/octet-stream",
                Some("second.bin"),
                Some("nate"),
            )
            .await
    });
    landed_rx.await.expect("upload reached the gap");

    // Now delete the last row referencing those bytes, concurrently. In
    // isolation this is a correct delete: nothing committed references the
    // bytes, so it counts zero and takes them.
    let deleter = s.clone();
    let delete_storage = local_storage_at(root.path());
    let first_id = uuid::Uuid::parse_str(&first.id).unwrap();
    let delete = tokio::spawn(async move {
        deleter
            .artifacts_delete(org, first_id, &delete_storage)
            .await
    });

    // Long enough for an unsynchronised delete to run all the way through and
    // unlink. Under the fix it is parked on the blob lock the upload holds.
    tokio::time::sleep(Duration::from_millis(250)).await;
    resume_tx.send(()).expect("uploader still waiting");

    let second = upload.await.unwrap().expect("upload committed");
    assert!(
        delete.await.unwrap().unwrap().is_some(),
        "the row was deleted"
    );
    assert_eq!(second.sha256, sha, "same content address");

    // The claim: a committed row's bytes are there. Before the fix this is a
    // row with no bytes, and GET /content 404s forever.
    let size = storage.size(org, &sha).await.unwrap();
    assert_eq!(
        size,
        Some(body.len() as u64),
        "the surviving row's bytes were unlinked out from under it"
    );
    let mut reader = storage
        .read_range(org, &sha, 0, body.len() as u64)
        .await
        .unwrap()
        .expect("bytes present");
    let mut got = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut got)
        .await
        .unwrap();
    assert_eq!(got, body, "and they are the right bytes");
}

/// The reverse interleave: the upload takes the lock first, so the delete's
/// refcount has to see the row it is about to be raced by.
#[tokio::test]
async fn a_delete_racing_a_dedup_hit_leaves_the_new_row_intact() {
    let (s, _test_db, root, org) = store_only().await;
    let storage = local_storage_at(root.path());
    let body = payload(4_096);

    let first = put(&s, org, &storage, "first.bin", &body).await;
    let second = put(&s, org, &storage, "second.bin", &body).await;
    assert_eq!(first.sha256, second.sha256);

    // Both rows deleted, one after the other: only the second takes the bytes.
    let one = s
        .artifacts_delete(org, uuid::Uuid::parse_str(&first.id).unwrap(), &storage)
        .await
        .unwrap();
    assert!(one.is_some());
    assert!(
        storage.size(org, &first.sha256).await.unwrap().is_some(),
        "a surviving row still needs these bytes"
    );

    let two = s
        .artifacts_delete(org, uuid::Uuid::parse_str(&second.id).unwrap(), &storage)
        .await
        .unwrap();
    assert!(two.is_some());
    assert!(storage.size(org, &first.sha256).await.unwrap().is_none());
}

/// Two deletes of one id used to both SELECT before either locked: both
/// answered 204, and the second issued a spurious unlink. `DELETE … RETURNING`
/// makes exactly one of them the one that removed the row.
#[tokio::test]
async fn only_one_of_two_concurrent_deletes_owns_the_row() {
    let (s, _test_db, root, org) = store_only().await;
    let storage = local_storage_at(root.path());
    let body = payload(2_048);
    let a = put(&s, org, &storage, "once.bin", &body).await;
    let id = uuid::Uuid::parse_str(&a.id).unwrap();

    let (left, right) = tokio::join!(
        s.artifacts_delete(org, id, &storage),
        s.artifacts_delete(org, id, &storage)
    );
    let deleted = [left.unwrap(), right.unwrap()];
    assert_eq!(
        deleted.iter().filter(|d| d.is_some()).count(),
        1,
        "exactly one caller removed the row"
    );
    assert!(storage.size(org, &a.sha256).await.unwrap().is_none());
}

// ---- the sweeper ----

/// Nothing else reconciles the filesystem against the table. Each of these is a
/// leak the old code could produce and never collect.
#[tokio::test]
async fn the_sweeper_reconciles_bytes_against_rows_in_both_directions() {
    let (s, _test_db, root, org) = store_only().await;
    let storage = local_storage_at(root.path());
    let now = Duration::ZERO;

    // 1. An object whose row never committed: bytes at a content address that
    //    nothing points at.
    let orphan_bytes = payload(777);
    let mut w = storage.begin(org).await.unwrap();
    w.write(&orphan_bytes).await.unwrap();
    let orphan = w.finish().await.unwrap().land().await.unwrap();
    assert!(storage.size(org, &orphan.sha256).await.unwrap().is_some());

    // 2. A live artifact, which must survive every sweep.
    let live = put(&s, org, &storage, "keep.bin", &payload(555)).await;

    // 3. Bytes a delete moved aside and never came back for, while a row still
    //    references them — a commit that failed, or a process that died.
    let stranded = storage.trash(org, &live.sha256).await.unwrap().unwrap();
    assert!(
        storage.size(org, &live.sha256).await.unwrap().is_none(),
        "set up: the live row's bytes are currently aside"
    );

    // 4. Bytes moved aside with nothing referencing them: litter.
    let mut w = storage.begin(org).await.unwrap();
    w.write(&payload(333)).await.unwrap();
    let doomed = w.finish().await.unwrap().land().await.unwrap();
    let doomed_trash = storage.trash(org, &doomed.sha256).await.unwrap().unwrap();

    // 5. A partial upload nobody finished.
    let tmp_dir = root
        .path()
        .join("artifacts")
        .join(org.simple().to_string())
        .join("tmp");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let part = format!("{}.part", uuid::Uuid::new_v4().simple());
    std::fs::write(tmp_dir.join(&part), b"half an upload").unwrap();

    let swept = s.artifacts_sweep(org, &storage, now).await.unwrap();
    assert_eq!(swept.temps_removed, 1, "{swept:?}");
    assert_eq!(swept.trash_restored, 1, "{swept:?}");
    assert_eq!(swept.trash_purged, 1, "{swept:?}");
    assert_eq!(swept.orphans_removed, 1, "{swept:?}");

    // The live row got its bytes back...
    assert_eq!(
        storage.size(org, &live.sha256).await.unwrap(),
        Some(555),
        "a row that still needs its bytes gets them back"
    );
    assert!(
        storage
            .read_range(org, &live.sha256, 0, 555)
            .await
            .unwrap()
            .is_some(),
        "and they are readable again"
    );
    // ...and everything nothing referenced is gone.
    let trash_dir = root
        .path()
        .join("artifacts")
        .join(org.simple().to_string())
        .join("trash");
    assert!(storage.size(org, &orphan.sha256).await.unwrap().is_none());
    assert!(!tmp_dir.join(&part).exists());
    assert!(!trash_dir.join(&doomed_trash.key).exists());
    assert!(
        !trash_dir.join(&stranded.key).exists(),
        "the restored bytes left the trash rather than being copied out of it"
    );

    // Idempotent: a second pass has nothing left to do, and the live row is
    // still untouched.
    let again = s.artifacts_sweep(org, &storage, now).await.unwrap();
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(storage.size(org, &live.sha256).await.unwrap(), Some(555));
}

/// The lock keeps the sweeper off anything in flight, but a request still
/// streaming its body holds no lock yet — hence the age floor.
#[tokio::test]
async fn the_sweeper_leaves_fresh_litter_alone() {
    let (s, _test_db, root, org) = store_only().await;
    let storage = local_storage_at(root.path());

    let mut w = storage.begin(org).await.unwrap();
    w.write(&payload(100)).await.unwrap();
    let orphan = w.finish().await.unwrap().land().await.unwrap();
    let tmp_dir = root
        .path()
        .join("artifacts")
        .join(org.simple().to_string())
        .join("tmp");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let part = format!("{}.part", uuid::Uuid::new_v4().simple());
    std::fs::write(tmp_dir.join(&part), b"still uploading").unwrap();

    let swept = s
        .artifacts_sweep(org, &storage, Duration::from_secs(3600))
        .await
        .unwrap();
    assert!(
        swept.is_empty(),
        "nothing this new is litter yet: {swept:?}"
    );
    assert!(storage.size(org, &orphan.sha256).await.unwrap().is_some());
    assert!(tmp_dir.join(&part).exists());
}

/// The sweeper finds orgs from the storage, not the `orgs` table: bytes left
/// behind by an org whose rows are all gone are exactly what needs reclaiming.
#[tokio::test]
async fn the_sweeper_finds_its_own_orgs() {
    let (s, _test_db, root, org) = store_only().await;
    let storage = local_storage_at(root.path());

    let live = put(&s, org, &storage, "keep.bin", &payload(64)).await;
    let mut w = storage.begin(org).await.unwrap();
    w.write(&payload(128)).await.unwrap();
    let orphan = w.finish().await.unwrap().land().await.unwrap();

    let swept = s
        .artifacts_sweep_all(&storage, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(swept.orphans_removed, 1, "{swept:?}");
    assert!(storage.size(org, &orphan.sha256).await.unwrap().is_none());
    assert_eq!(storage.size(org, &live.sha256).await.unwrap(), Some(64));
}

/// A download that races a delete is the artifact going away, not the server
/// breaking.
#[tokio::test]
async fn bytes_that_vanish_under_a_reader_are_a_404() {
    let (app, _test_db) = app().await;
    let cookie = admin_token(&app).await;
    let body = payload(1_500);
    let a = upload(&app, &cookie, "gone.bin", "application/octet-stream", &body).await;
    let path = object_path(&a);
    assert!(path.is_file());

    // The row survives; the bytes do not. Exactly the state a delete that races
    // a read leaves behind between its two steps.
    std::fs::remove_file(&path).unwrap();

    let got = fetch(
        &app,
        &cookie,
        &format!("/api/artifacts/{}/content", a["id"].as_str().unwrap()),
        Some("bytes=0-99"),
    )
    .await;
    assert_eq!(got.status, StatusCode::NOT_FOUND);
}

/// `Drop` hands its unlink to `spawn_blocking` rather than stalling the
/// executor, so the temp goes away a beat after the request does.
async fn wait_until_empty(dir: &std::path::Path) {
    for _ in 0..200 {
        let n = std::fs::read_dir(dir)
            .map(|d| d.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        if n == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
