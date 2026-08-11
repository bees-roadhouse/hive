// File artifacts over HTTP: bytes in, bytes out, ranged, scoped, deduped.
//
// The assertions that matter are byte-level. A content route that returns the
// right status and the wrong bytes is worse than one that fails, so every read
// here compares the payload itself, and the range cases compare against the
// exact slice they asked for.

use std::path::PathBuf;
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::Value;
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
        dir
    })
}

async fn app() -> Router {
    std::env::set_var("HIVE_EMBED", "hash");
    data_root();
    let pool = hive_api::db::test_pool().await;
    let s = hive_api::store::Store::new(pool);
    s.onboarding_complete("Test Hive", "Nate", "nate@example.com", "hunter22-strong")
        .await
        .unwrap();
    hive_api::routes::router(s)
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
    let app = app().await;
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
    let app = app().await;
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
    let app = app().await;
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
    let app = app().await;
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
    let app = app().await;
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
    let app = app().await;
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
