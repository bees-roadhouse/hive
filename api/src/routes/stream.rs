// GET /api/stream — the SSE live-push stream (server.ts handleStream). Every
// mutation calls Store::emit → broadcast → here. Self-authenticating: the
// middleware attaches AuthCtx but does not gate this path (Node served it from
// the raw http server), so the 401 shape is produced here. Clients reconnect
// automatically via EventSource; a heartbeat comment every 25 s keeps idle
// connections alive through proxies.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Extension, Router};
use serde_json::json;

use crate::error::ApiResult;
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new().route("/api/stream", get(stream))
}

async fn stream(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if s.onboarding_required().await? || ctx.actor.is_none() {
        return Ok((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthenticated"})),
        )
            .into_response());
    }
    // The bus is one in-process channel for every org, and the response body is
    // polled long after this handler's acting scope is gone — so the org is
    // captured here and the subscription filters on it. Without an org there is
    // nothing to subscribe to.
    let Some(org) = ctx.org else {
        return Ok((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "no_acting_org"})),
        )
            .into_response());
    };

    let rx = s.subscribe(org);
    let events = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default().comment("connected"));
        for await ev in rx {
            // Node's BusEvent shape: {kind, actor, payload, at} — `at`,
            // not created_at, and no id.
            let data = json!({
                "kind": ev.kind,
                "actor": ev.actor,
                "payload": ev.payload,
                "at": ev.created_at,
            });
            yield Ok(Event::default().data(data.to_string()));
        }
    };

    Ok(Sse::new(events)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(25))
                .text("heartbeat"),
        )
        .into_response())
}
