// Solid.js SPA static serving with index.html fallback. Owned by the SPA workstream.
//
// Replaces the nginx web container: the vite build of packages/web is served
// straight from this process. This router is merged LAST in routes::mod, so it
// only sees requests no API route matched. nginx parity:
//   - real files (assets, index.html) serve from the dist dir;
//   - client-routed paths (/consent, /login, /journal, ...) fall back to
//     index.html (`try_files $uri /index.html`);
//   - unknown /api/* paths keep the JSON 404 shape — never index.html.
// The auth middleware does not gate non-API paths, so the SPA loads
// unauthenticated (nginx served it ungated too).

use std::path::{Path, PathBuf};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::store::Store;

/// Where the vite build lives: $HIVE_WEB_DIST wins; otherwise try
/// `packages/web/dist` (dev, relative to CWD) then `/app/web` (container).
fn resolve_dist() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match std::env::var("HIVE_WEB_DIST") {
        Ok(dir) => vec![PathBuf::from(dir)],
        Err(_) => vec![
            PathBuf::from("packages/web/dist"),
            PathBuf::from("/app/web"),
        ],
    };
    candidates.into_iter().find(|p| has_index(p))
}

fn has_index(dist: &Path) -> bool {
    dist.join("index.html").is_file()
}

pub fn router() -> Router<Store> {
    let dist = resolve_dist();
    match &dist {
        Some(dir) => tracing::info!(dist = %dir.display(), "serving SPA"),
        None => tracing::warn!(
            "SPA dist not found (set HIVE_WEB_DIST or build packages/web); \
             non-API paths will 404"
        ),
    }
    Router::new().fallback(move |req: Request| serve_spa(dist.clone(), req))
}

async fn serve_spa(dist: Option<PathBuf>, req: Request) -> Response {
    let path = req.uri().path();
    // An /api/* path reaching the fallback means no API route matched —
    // answer with the API's JSON 404, not index.html.
    if path == "/api" || path.starts_with("/api/") {
        return super::json_404();
    }
    let Some(dist) = dist else {
        return (
            StatusCode::NOT_FOUND,
            "hive web UI not found: set HIVE_WEB_DIST to the built packages/web/dist \
             directory (default candidates: ./packages/web/dist, /app/web)",
        )
            .into_response();
    };
    let serve = ServeDir::new(&dist).fallback(ServeFile::new(dist.join("index.html")));
    match serve.oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(infallible) => match infallible {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_builds_without_dist() {
        // Test CWD is the crate dir (api/), so no dist candidate resolves;
        // the router must still build rather than panic.
        let _router: Router<Store> = router();
    }

    #[test]
    fn has_index_detects_a_real_dist() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!has_index(dir.path()));
        std::fs::write(dir.path().join("index.html"), "<!doctype html>").expect("write");
        assert!(has_index(dir.path()));
    }
}
