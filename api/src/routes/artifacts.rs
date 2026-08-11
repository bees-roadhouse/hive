// Artifacts: stored FILES — photos, scans, PDFs, audio, mail attachments.
// Bytes in, bytes out, ranged, scoped to an org.
//
//   POST   /api/artifacts             multipart upload -> row + stored bytes
//   GET    /api/artifacts             newest-first listing for the acting org
//   GET    /api/artifacts/{id}        metadata
//   GET    /api/artifacts/{id}/content  streams bytes, Range-aware
//   DELETE /api/artifacts/{id}        row + unlink
//
// (Claude Code skills / agents / slash-commands are `identity_artifacts` and
// live in routes/identity_artifacts.rs. The bare word means files now.)
//
// Nothing is buffered in either direction. Upload chunks go straight from the
// multipart field to the storage driver's temp file; downloads hand the
// driver's AsyncRead to the response body as a stream. A 200 MB video costs a
// buffer, not 200 MB of RSS.
//
// Ranges are here from the first commit rather than retrofitted, because
// docs/ARTIFACTS.md's streaming tier is built on them: metadata and thumbnails
// sync eagerly while original bytes are fetched on demand, and "on demand"
// means a video player asking for the middle of a file.
//
// Not here, deliberately: OCR, captions, speech-to-text, thumbnails, EXIF.
// Those are the derived pipeline. They attach to `artifacts.id` as their parent
// (docs/ARTIFACTS.md Part 3) and nothing in this file needs to change for them.

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use hive_core::artifact_storage::storage;
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::error::{err, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

const DEFAULT_MIME: &str = "application/octet-stream";
/// Cap on a stored mime / filename string, so a hostile multipart header can't
/// push an unbounded value into a column.
const MAX_META_LEN: usize = 255;

pub fn router() -> Router<Store> {
    Router::new()
        .route(
            "/api/artifacts",
            post(upload)
                // The payload streams to disk chunk by chunk and is never held
                // in memory, so axum's 2 MiB default is the wrong shape of
                // limit here — the ceiling that matters is disk, and a byte
                // quota belongs with org quotas rather than the body layer.
                .layer(DefaultBodyLimit::disable())
                .get(list),
        )
        .route("/api/artifacts/{id}", get(metadata).delete(remove))
        .route("/api/artifacts/{id}/content", get(content))
}

// ---- upload ----

async fn upload(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    mut form: Multipart,
) -> ApiResult {
    // The acting org comes off the CREDENTIAL (AuthCtx.org), never a header,
    // query, or body ... docs/WEB-APP.md: one org per session, no switching.
    let Some(org) = ctx.org else {
        return Ok(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };

    while let Some(mut field) = form.next_field().await? {
        // The file part is the one carrying a filename; a plain text field
        // (a caption, a csrf token) has none and is skipped.
        let Some(filename) = field.file_name().map(clean_filename) else {
            continue;
        };
        let mime = field
            .content_type()
            .map(|m| truncate(m, MAX_META_LEN))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MIME.to_string());

        let mut write = storage().begin(org).await?;
        while let Some(chunk) = field.chunk().await? {
            write.write(&chunk).await?;
        }
        let artifact = s
            .artifacts_create(
                org,
                write,
                &mime,
                filename.as_deref(),
                ctx.namespace_owner().or(ctx.actor.as_deref()),
            )
            .await?;
        return Ok((StatusCode::CREATED, Json(artifact)).into_response());
    }

    Ok(err(
        StatusCode::BAD_REQUEST,
        "multipart body with a file part required",
    ))
}

// ---- metadata ----

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    // The acting org comes off the CREDENTIAL (AuthCtx.org), never a header,
    // query, or body ... docs/WEB-APP.md: one org per session, no switching.
    let Some(org) = ctx.org else {
        return Ok(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };
    Ok(Json(s.artifacts_list(org, q.limit.unwrap_or(100)).await?).into_response())
}

async fn metadata(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    // The acting org comes off the CREDENTIAL (AuthCtx.org), never a header,
    // query, or body ... docs/WEB-APP.md: one org per session, no switching.
    let Some(org) = ctx.org else {
        return Ok(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };
    let Some(id) = parse_id(&id) else {
        return Ok(not_found());
    };
    match s.artifacts_get(org, id).await? {
        Some(a) => Ok(Json(a).into_response()),
        None => Ok(not_found()),
    }
}

async fn remove(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    // The acting org comes off the CREDENTIAL (AuthCtx.org), never a header,
    // query, or body ... docs/WEB-APP.md: one org per session, no switching.
    let Some(org) = ctx.org else {
        return Ok(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };
    let Some(id) = parse_id(&id) else {
        return Ok(not_found());
    };
    match s.artifacts_delete(org, id, storage()).await? {
        Some(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        None => Ok(not_found()),
    }
}

// ---- content ----

async fn content(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    // The acting org comes off the CREDENTIAL (AuthCtx.org), never a header,
    // query, or body ... docs/WEB-APP.md: one org per session, no switching.
    let Some(org) = ctx.org else {
        return Ok(err(StatusCode::UNAUTHORIZED, "authentication required"));
    };
    let Some(id) = parse_id(&id) else {
        return Ok(not_found());
    };
    let Some(artifact) = s.artifacts_get(org, id).await? else {
        return Ok(not_found());
    };

    // The file on disk is the authority for range arithmetic; the row's `bytes`
    // is what it was at upload. They agree unless the store was tampered with,
    // and if the object is gone the row is not servable.
    let store = storage();
    let Some(total) = store.size(org, &artifact.sha256).await? else {
        return Ok(not_found());
    };

    let requested = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|raw| parse_range(raw, total))
        .unwrap_or(RangeSpec::Full);

    let (status, start, len) = match requested {
        RangeSpec::Full => (StatusCode::OK, 0, total),
        RangeSpec::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        RangeSpec::Unsatisfiable => {
            let mut res = err(StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable");
            insert_header(&mut res, header::CONTENT_RANGE, &format!("bytes */{total}"));
            insert_header(&mut res, header::ACCEPT_RANGES, "bytes");
            return Ok(res);
        }
    };

    let reader = store.read_range(org, &artifact.sha256, start, len).await?;
    let mut res = Response::builder()
        .status(status)
        .body(Body::from_stream(ReaderStream::new(reader)))?;

    insert_header(&mut res, header::CONTENT_TYPE, &artifact.mime);
    insert_header(&mut res, header::CONTENT_LENGTH, &len.to_string());
    insert_header(&mut res, header::ACCEPT_RANGES, "bytes");
    // Whatever the row claims the type is, that is what it is served as.
    insert_header(&mut res, "x-content-type-options", "nosniff");
    insert_header(
        &mut res,
        header::CONTENT_DISPOSITION,
        &disposition(&artifact.mime, artifact.filename.as_deref()),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        insert_header(
            &mut res,
            header::CONTENT_RANGE,
            &format!("bytes {start}-{}/{total}", start + len - 1),
        );
    }
    Ok(res)
}

// ---- helpers ----

fn parse_id(raw: &str) -> Option<Uuid> {
    Uuid::parse_str(raw).ok()
}

fn insert_header(res: &mut Response, name: impl axum::http::header::IntoHeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        res.headers_mut().insert(name, v);
    }
}

/// What a browser should do with these bytes. Inline, so a photo grid and a PDF
/// preview work — except for the handful of types that execute script when a
/// browser NAVIGATES to them. Those are same-origin here, so an uploaded
/// `.html` served inline is stored XSS; `attachment` costs nothing (an
/// `<img src>` still renders an SVG, it just cannot be navigated to).
fn disposition(mime: &str, filename: Option<&str>) -> String {
    const SCRIPTABLE: &[&str] = &[
        "text/html",
        "application/xhtml+xml",
        "image/svg+xml",
        "application/xml",
        "text/xml",
    ];
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let kind = if SCRIPTABLE.contains(&base.as_str()) {
        "attachment"
    } else {
        "inline"
    };
    match filename {
        // ASCII fallback plus RFC 6266 `filename*` for everything else.
        Some(name) => {
            let ascii: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_graphic() && c != '"' && c != '\\' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!(
                "{kind}; filename=\"{ascii}\"; filename*=UTF-8''{}",
                urlencoding::encode(name)
            )
        }
        None => kind.to_string(),
    }
}

/// An upload's name is for display and download, never for a path: the bytes
/// are addressed by their hash. Take the basename and drop anything that would
/// make it look like one.
fn clean_filename(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(truncate(&cleaned, MAX_META_LEN))
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    Full,
    /// Inclusive on both ends, the way `Content-Range` states it.
    Partial {
        start: u64,
        end: u64,
    },
    Unsatisfiable,
}

/// A single byte range, per RFC 9110 §14.
///
/// Deliberate simplifications, all of them legal: a multi-range request is
/// answered with the whole representation rather than `multipart/byteranges`
/// (a server MAY do this, and nothing hive serves needs the multipart form),
/// and a syntactically broken `Range` is ignored rather than rejected (a
/// recipient MUST ignore one it cannot parse).
fn parse_range(raw: &str, total: u64) -> RangeSpec {
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let spec = spec.trim();
    let Some((from, to)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };
    let (from, to) = (from.trim(), to.trim());

    // A zero-length object satisfies no byte range at all.
    if total == 0 {
        return RangeSpec::Unsatisfiable;
    }
    let last = total - 1;

    match (from.is_empty(), to.is_empty()) {
        // "-N": the final N bytes.
        (true, false) => match to.parse::<u64>() {
            Ok(0) => RangeSpec::Unsatisfiable,
            Ok(n) => RangeSpec::Partial {
                start: total.saturating_sub(n),
                end: last,
            },
            Err(_) => RangeSpec::Full,
        },
        // "N-": from N to the end.
        (false, true) => match from.parse::<u64>() {
            Ok(start) if start <= last => RangeSpec::Partial { start, end: last },
            Ok(_) => RangeSpec::Unsatisfiable,
            Err(_) => RangeSpec::Full,
        },
        // "N-M": clamped to the end of the object.
        (false, false) => match (from.parse::<u64>(), to.parse::<u64>()) {
            (Ok(start), Ok(end)) if start > end => RangeSpec::Unsatisfiable,
            (Ok(start), Ok(_)) if start > last => RangeSpec::Unsatisfiable,
            (Ok(start), Ok(end)) => RangeSpec::Partial {
                start,
                end: end.min(last),
            },
            _ => RangeSpec::Full,
        },
        // "bytes=-" is neither.
        (true, true) => RangeSpec::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_the_way_content_range_states_them() {
        // A whole-object read, and the shapes that are not ranges at all.
        assert_eq!(
            parse_range("bytes=0-", 100),
            RangeSpec::Partial { start: 0, end: 99 }
        );
        assert_eq!(parse_range("items=0-5", 100), RangeSpec::Full);
        assert_eq!(parse_range("bytes=-", 100), RangeSpec::Full);
        assert_eq!(parse_range("bytes=abc-def", 100), RangeSpec::Full);

        // Closed, open, and suffix forms.
        assert_eq!(
            parse_range("bytes=0-0", 100),
            RangeSpec::Partial { start: 0, end: 0 }
        );
        assert_eq!(
            parse_range("bytes=10-19", 100),
            RangeSpec::Partial { start: 10, end: 19 }
        );
        assert_eq!(
            parse_range("bytes=90-", 100),
            RangeSpec::Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range("bytes=-10", 100),
            RangeSpec::Partial { start: 90, end: 99 }
        );

        // Clamping and the unsatisfiable cases.
        assert_eq!(
            parse_range("bytes=95-1000", 100),
            RangeSpec::Partial { start: 95, end: 99 }
        );
        assert_eq!(
            parse_range("bytes=-1000", 100),
            RangeSpec::Partial { start: 0, end: 99 }
        );
        assert_eq!(parse_range("bytes=100-", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=100-200", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=20-10", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-0", 0), RangeSpec::Unsatisfiable);

        // Multi-range: answered whole rather than as multipart/byteranges.
        assert_eq!(parse_range("bytes=0-9,20-29", 100), RangeSpec::Full);
    }

    #[test]
    fn an_upload_name_can_never_become_a_path() {
        assert_eq!(
            clean_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            clean_filename(r"C:\Users\nate\scan.pdf").as_deref(),
            Some("scan.pdf")
        );
        assert_eq!(clean_filename("plain.jpg").as_deref(), Some("plain.jpg"));
        assert_eq!(clean_filename("  "), None);
        assert_eq!(clean_filename(".."), None);
        assert_eq!(clean_filename("a/.."), None);
        assert_eq!(
            clean_filename(&"x".repeat(400)).map(|s| s.len()),
            Some(MAX_META_LEN)
        );
    }

    #[test]
    fn script_capable_types_download_instead_of_rendering() {
        assert!(disposition("text/html", Some("evil.html")).starts_with("attachment;"));
        assert!(disposition("image/svg+xml; charset=utf-8", None).starts_with("attachment"));
        assert!(disposition("image/jpeg", Some("photo.jpg")).starts_with("inline;"));
        assert!(disposition("application/pdf", Some("scan.pdf")).starts_with("inline;"));
        // A quote in the name cannot break out of the quoted-string.
        let d = disposition("image/jpeg", Some("we\"ird.jpg"));
        assert!(d.contains("filename=\"we_ird.jpg\""), "{d}");
        assert!(d.contains("filename*=UTF-8''"), "{d}");
    }
}
