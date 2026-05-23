//! Axum HTTP router and all handler stubs.
//!
//! Exposes the local ops API on `127.0.0.1:7878` (loopback only; no auth
//! required per PLAN.md §9.7).
//!
//! # Endpoints
//!
//! | Method | Path                  | Status     | Description                                    |
//! |--------|-----------------------|------------|------------------------------------------------|
//! | GET    | `/healthz`            | ✅ Impl    | Liveness check + uptime                        |
//! | GET    | `/stats`              | 🔄 T29    | Counts per status, queue depth, last error     |
//! | GET    | `/files`              | 🔄 T29    | Paginated file rows (`?status=&limit=&offset=`)|
//! | GET    | `/files/by-path`      | 🔄 T29    | Look up a row by path (`?path=`)               |
//! | GET    | `/files/:id`          | 🔄 T29    | Row + outputs + recent events                  |
//! | POST   | `/files/:id/requeue`  | 🔄 T29    | Reset to `queued`, attempts=0                  |
//! | POST   | `/files/:id/reset`    | 🔄 T29    | Delete row + outputs                           |
//! | POST   | `/scan`               | 🔄 T29    | Trigger immediate full scan                    |
//! | GET    | `/events`             | 🔄 T29    | Recent audit events (`?since=&level=&kind=`)   |
//! | GET    | `/tail`               | 🔄 T30    | SSE stream of new events                       |

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::json;

use crate::AppState;

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the axum [`Router`] with all registered routes.
///
/// `state` is an [`Arc<AppState>`] that will be cloned into each handler via
/// axum's `State` extractor.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // ── Liveness ──────────────────────────────────────────────────────────
        .route("/healthz", get(healthz))
        // ── Statistics ────────────────────────────────────────────────────────
        .route("/stats", get(stats_stub))
        // ── File operations ───────────────────────────────────────────────────
        //
        // IMPORTANT: `/files/by-path` MUST be registered BEFORE `/files/:id`
        // so that axum's router matches the literal segment `by-path` first and
        // does not attempt to parse it as a numeric id.
        .route("/files", get(list_files_stub))
        .route("/files/by-path", get(files_by_path_stub))
        .route("/files/:id", get(get_file_stub))
        .route("/files/:id/requeue", post(requeue_file_stub))
        .route("/files/:id/reset", post(reset_file_stub))
        // ── Actions ───────────────────────────────────────────────────────────
        .route("/scan", post(scan_stub))
        // ── Audit log ─────────────────────────────────────────────────────────
        .route("/events", get(events_stub))
        // ── SSE tail ──────────────────────────────────────────────────────────
        .route("/tail", get(tail_stub))
        // ── Shared state ─────────────────────────────────────────────────────
        .with_state(state)
}

// ── /healthz ──────────────────────────────────────────────────────────────────

/// Response body for `GET /healthz`.
#[derive(Serialize)]
struct HealthzResponse {
    ok: bool,
    uptime_secs: u64,
}

/// `GET /healthz` — liveness check.
///
/// Returns `{"ok": true, "uptime_secs": <elapsed>}`.
/// Never returns an error code; if the server is alive, this endpoint succeeds.
async fn healthz(State(state): State<Arc<AppState>>) -> Json<HealthzResponse> {
    let uptime_secs = state.start_time.elapsed().as_secs();
    Json(HealthzResponse {
        ok: true,
        uptime_secs,
    })
}

// ── 501 stub helper ───────────────────────────────────────────────────────────

/// Shorthand for a `501 Not Implemented` JSON response used by all stubs.
fn not_implemented(endpoint: &str) -> Response {
    let body = Json(json!({
        "error": "not_implemented",
        "message": format!("{endpoint} is not yet implemented (pending T29/T30)"),
    }));
    (StatusCode::NOT_IMPLEMENTED, body).into_response()
}

// ── /stats ────────────────────────────────────────────────────────────────────

/// `GET /stats` — daemon statistics (stub, T29).
async fn stats_stub(_state: State<Arc<AppState>>) -> Response {
    not_implemented("GET /stats")
}

// ── /files ────────────────────────────────────────────────────────────────────

/// `GET /files` — paginated file list (stub, T29).
async fn list_files_stub(_state: State<Arc<AppState>>) -> Response {
    not_implemented("GET /files")
}

/// `GET /files/by-path` — look up by filesystem path (stub, T29).
async fn files_by_path_stub(_state: State<Arc<AppState>>) -> Response {
    not_implemented("GET /files/by-path")
}

/// `GET /files/:id` — single file row + outputs + events (stub, T29).
async fn get_file_stub(_state: State<Arc<AppState>>, Path(_id): Path<i64>) -> Response {
    not_implemented("GET /files/:id")
}

/// `POST /files/:id/requeue` — reset status to queued (stub, T29).
async fn requeue_file_stub(_state: State<Arc<AppState>>, Path(_id): Path<i64>) -> Response {
    not_implemented("POST /files/:id/requeue")
}

/// `POST /files/:id/reset` — delete row + outputs (stub, T29).
async fn reset_file_stub(_state: State<Arc<AppState>>, Path(_id): Path<i64>) -> Response {
    not_implemented("POST /files/:id/reset")
}

// ── /scan ─────────────────────────────────────────────────────────────────────

/// `POST /scan` — trigger an immediate full vault scan (stub, T29).
async fn scan_stub(_state: State<Arc<AppState>>) -> Response {
    not_implemented("POST /scan")
}

// ── /events ───────────────────────────────────────────────────────────────────

/// `GET /events` — recent audit events (stub, T29).
async fn events_stub(_state: State<Arc<AppState>>) -> Response {
    not_implemented("GET /events")
}

// ── /tail ─────────────────────────────────────────────────────────────────────

/// `GET /tail` — SSE stream of new audit events (stub, T30).
///
/// Full SSE implementation lives in [`crate::sse`]; this stub delegates to
/// the placeholder stream until T30 is implemented.
async fn tail_stub(_state: State<Arc<AppState>>) -> Response {
    not_implemented("GET /tail")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use std::path::Path;
    use std::time::Instant;
    use tower::ServiceExt; // for `oneshot`

    /// Build a minimal AppState backed by a temp-file SQLite DB for testing.
    async fn test_state() -> Arc<AppState> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        // Keep `dir` alive via Box::leak — acceptable in tests.
        Box::leak(Box::new(dir));
        let store = kb_core::StateStore::new(&db_path, &[5u64, 30, 120])
            .await
            .expect("test state store");
        Arc::new(AppState {
            state_store: store,
            start_time: Instant::now(),
            scanner_trigger: None,
        })
    }

    #[tokio::test]
    async fn healthz_returns_ok_and_uptime() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(json["ok"], true);
        assert!(json["uptime_secs"].is_number());
    }

    #[tokio::test]
    async fn stats_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn list_files_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn files_by_path_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files/by-path?path=/some/file.pdf")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn get_file_by_id_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files/42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn requeue_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/files/1/requeue")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn reset_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/files/1/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn scan_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/scan")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn events_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn tail_returns_501() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/tail")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn healthz_uptime_increases_over_time() {
        // Verify start_time is actually used (not wall clock epoch).
        // We set start_time to 10 seconds in the past and verify uptime >= 10.
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        Box::leak(Box::new(dir));
        let store = kb_core::StateStore::new(&db_path, &[5u64, 30, 120])
            .await
            .expect("test state store");
        let state = Arc::new(AppState {
            state_store: store,
            start_time: Instant::now() - std::time::Duration::from_secs(10),
            scanner_trigger: None,
        });

        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["uptime_secs"].as_u64().unwrap() >= 10);
    }
}
