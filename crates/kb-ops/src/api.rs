//! Axum HTTP router and all handler implementations.
//!
//! Exposes the local ops API on `127.0.0.1:7878` (loopback only; no auth
//! required per PLAN.md §9.7).
//!
//! # Endpoints
//!
//! | Method | Path                  | Status     | Description                                    |
//! |--------|-----------------------|------------|------------------------------------------------|
//! | GET    | `/healthz`            | ✅ Impl    | Liveness check + uptime                        |
//! | GET    | `/stats`              | ✅ Impl    | Counts per status, queue depth, last error     |
//! | GET    | `/files`              | ✅ Impl    | Paginated file rows (`?status=&limit=&offset=`)|
//! | GET    | `/files/by-path`      | ✅ Impl    | Look up a row by path (`?path=`)               |
//! | GET    | `/files/:id`          | ✅ Impl    | Row + outputs + recent events                  |
//! | POST   | `/files/:id/requeue`  | ✅ Impl    | Reset to `queued`, attempts=0                  |
//! | POST   | `/files/:id/reset`    | ✅ Impl    | Delete row + outputs                           |
//! | POST   | `/scan`               | ✅ Impl    | Trigger immediate full scan                    |
//! | GET    | `/events`             | ✅ Impl    | Recent audit events (`?since=&level=&kind=`)   |
//! | GET    | `/tail`               | 🔄 T30    | SSE stream of new events                       |

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use kb_core::{AuditEvent, FileRow, OutputRecord, Status};

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
        .route("/stats", get(get_stats))
        // ── File operations ───────────────────────────────────────────────────
        //
        // IMPORTANT: `/files/by-path` MUST be registered BEFORE `/files/:id`
        // so that axum's router matches the literal segment `by-path` first and
        // does not attempt to parse it as a numeric id.
        .route("/files", get(list_files))
        .route("/files/by-path", get(files_by_path))
        .route("/files/{id}", get(get_file))
        .route("/files/{id}/requeue", post(requeue_file))
        .route("/files/{id}/reset", post(reset_file))
        // ── Actions ───────────────────────────────────────────────────────────
        .route("/scan", post(scan))
        // ── Audit log ─────────────────────────────────────────────────────────
        .route("/events", get(get_events))
        // ── SSE tail ──────────────────────────────────────────────────────────
        .route("/tail", get(tail_sse_handler))
        // ── Prometheus metrics ───────────────────────────────────────────────
        .route("/metrics", get(metrics_handler))
        // ── Shared state ─────────────────────────────────────────────────────
        .with_state(state)
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Unified API error type that maps to HTTP status codes and JSON error bodies.
enum ApiError {
    /// 404 — the requested resource does not exist.
    NotFound(String),
    /// 400 — the request parameters are invalid or missing.
    BadRequest(String),
    /// 500 — an unexpected internal error occurred.
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "not_found", "message": msg })),
            )
                .into_response(),
            ApiError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_request", "message": msg })),
            )
                .into_response(),
            ApiError::Internal(err) => {
                tracing::error!(error = %err, "Internal API error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal_error", "message": err.to_string() })),
                )
                    .into_response()
            }
        }
    }
}

/// Convenient conversion so `?` works on `anyhow::Error` inside handlers.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
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

// ── /stats ────────────────────────────────────────────────────────────────────

/// Response body for `GET /stats`.
#[derive(Serialize)]
struct StatsResponse {
    /// Counts broken down by status string.
    counts_per_status: serde_json::Value,
    /// Total of `queued + processing` rows.
    queue_depth: i64,
    /// `last_error` from the most recently failed row, if any.
    last_error: Option<String>,
    /// Number of rows currently being processed.
    in_flight: i64,
    /// Age in seconds of the oldest pending entry; `null` when queue is empty.
    oldest_pending_age_secs: Option<i64>,
}

/// `GET /stats` — daemon-wide statistics.
///
/// Returns counts per status, queue depth, the last error message, the number
/// of in-flight jobs, and the age of the oldest pending item.
async fn get_stats(State(state): State<Arc<AppState>>) -> Result<Json<StatsResponse>, ApiError> {
    let s = state.state_store.stats().await?;

    let counts_per_status = json!({
        "seen":       s.seen,
        "queued":     s.queued,
        "processing": s.processing,
        "done":       s.done,
        "failed":     s.failed,
        "skipped":    s.skipped,
    });

    Ok(Json(StatsResponse {
        counts_per_status,
        queue_depth: s.queue_depth,
        last_error: s.last_error,
        in_flight: s.processing,
        oldest_pending_age_secs: s.oldest_pending_age_secs,
    }))
}

// ── /files ────────────────────────────────────────────────────────────────────

/// Query parameters for `GET /files`.
#[derive(Debug, Deserialize)]
struct FilesQuery {
    /// Optional status filter (`seen`, `queued`, `processing`, `done`,
    /// `failed`, `skipped`).
    status: Option<String>,
    /// Maximum rows to return (default: 100, capped at 1000).
    limit: Option<i64>,
    /// Number of rows to skip (default: 0).
    offset: Option<i64>,
}

/// `GET /files?status=<s>&limit=<n>&offset=<n>` — paginated file list.
///
/// Returns a JSON array of [`FileRow`] objects.  Applies an optional status
/// filter and pagination via `limit`/`offset`.
async fn list_files(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FilesQuery>,
) -> Result<Json<Vec<FileRow>>, ApiError> {
    // Parse and validate the optional status filter.
    let status_filter = params
        .status
        .as_deref()
        .map(|s| {
            Status::from_str(s).map_err(|_| {
                ApiError::BadRequest(format!(
                    "invalid status '{s}'; must be one of: seen, queued, processing, done, failed, skipped"
                ))
            })
        })
        .transpose()?;

    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows = state
        .state_store
        .list_files(status_filter, limit, offset)
        .await?;

    Ok(Json(rows))
}

// ── /files/by-path ────────────────────────────────────────────────────────────

/// Query parameters for `GET /files/by-path`.
#[derive(Debug, Deserialize)]
struct ByPathQuery {
    /// The filesystem path to look up.
    path: Option<String>,
}

/// `GET /files/by-path?path=<path>` — look up a file row by filesystem path.
///
/// Returns the same `{file, outputs, events}` structure as `GET /files/:id`.
async fn files_by_path(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ByPathQuery>,
) -> Result<Json<FileDetailResponse>, ApiError> {
    let path_str = params
        .path
        .ok_or_else(|| ApiError::BadRequest("missing required query parameter: path".to_string()))?;

    if path_str.is_empty() {
        return Err(ApiError::BadRequest("path must not be empty".to_string()));
    }

    let path = PathBuf::from(&path_str);
    let file = state
        .state_store
        .find_by_path(path)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no file found at path '{path_str}'")))?;

    build_file_detail(&state, file).await
}

// ── /files/:id ────────────────────────────────────────────────────────────────

/// `GET /files/:id` — full detail for a single source file.
///
/// Returns `{"file": FileRow, "outputs": [...], "events": [...]}`.
async fn get_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<FileDetailResponse>, ApiError> {
    let file = state
        .state_store
        .get_file_by_id(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no file found with id {id}")))?;

    build_file_detail(&state, file).await
}

/// Shared detail builder used by both `/files/:id` and `/files/by-path`.
///
/// Fetches outputs and the most recent events for the given file row, then
/// wraps them in a [`FileDetailResponse`].
async fn build_file_detail(
    state: &AppState,
    file: FileRow,
) -> Result<Json<FileDetailResponse>, ApiError> {
    let file_id = file.id;

    let outputs = state
        .state_store
        .get_outputs_for_file(file_id)
        .await?;

    // `get_events` has no per-file filter; fetch a generous batch and
    // filter client-side.  200 is enough for any reasonable file history.
    let all_events = state
        .state_store
        .get_events(None, None, None, 200)
        .await?;

    let events: Vec<AuditEvent> = all_events
        .into_iter()
        .filter(|e| e.file_id == Some(file_id))
        .take(20)
        .collect();

    Ok(Json(FileDetailResponse {
        file,
        outputs,
        events,
    }))
}

/// Response body for `GET /files/:id` and `GET /files/by-path`.
#[derive(Serialize)]
struct FileDetailResponse {
    file: FileRow,
    outputs: Vec<OutputRecord>,
    events: Vec<AuditEvent>,
}

// ── /files/:id/requeue ────────────────────────────────────────────────────────

/// Response body for `POST /files/:id/requeue`.
#[derive(Serialize)]
struct RequeueResponse {
    ok: bool,
    previous_status: String,
}

/// `POST /files/:id/requeue` — reset a file's status to `queued`.
///
/// Resets `status → queued` and `attempts → 0`, allowing the file to be
/// picked up by a worker on the next claim cycle.
///
/// Returns `{"ok": true, "previous_status": "<status>"}`.
async fn requeue_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<RequeueResponse>, ApiError> {
    // Verify the file exists before attempting to requeue.
    state
        .state_store
        .get_file_by_id(id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no file found with id {id}")))?;

    let previous_status = state.state_store.requeue(id).await?;

    Ok(Json(RequeueResponse {
        ok: true,
        previous_status: previous_status.as_str().to_string(),
    }))
}

// ── /files/:id/reset ──────────────────────────────────────────────────────────

/// Response body for `POST /files/:id/reset`.
#[derive(Serialize)]
struct ResetResponse {
    ok: bool,
    outputs_removed: usize,
}

/// `POST /files/:id/reset` — hard-delete a file row and all its outputs.
///
/// The actual output files on disk are **not** removed; only the DB records
/// are deleted.  The file will be re-discovered and re-queued on the next
/// scan or watcher event.
///
/// Returns `{"ok": true, "outputs_removed": N}`.
async fn reset_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<ResetResponse>, ApiError> {
    // reset_file returns Err if id does not exist.
    let (_path, outputs_removed) = state
        .state_store
        .reset_file(id)
        .await
        .map_err(|e| {
            // Distinguish "not found" from other errors by inspecting the message.
            let msg = e.to_string();
            if msg.contains("not found") || msg.contains("no file") || msg.contains("No file") {
                ApiError::NotFound(format!("no file found with id {id}"))
            } else {
                ApiError::Internal(e)
            }
        })?;

    Ok(Json(ResetResponse {
        ok: true,
        outputs_removed,
    }))
}

// ── /scan ─────────────────────────────────────────────────────────────────────

/// Response body for `POST /scan`.
#[derive(Serialize)]
struct ScanResponse {
    ok: bool,
    message: String,
}

/// `POST /scan` — trigger an immediate full vault scan.
///
/// Sends `()` on the scanner trigger channel if one is configured.  When the
/// daemon is running without a scanner (e.g. in unit tests), returns a 200
/// with a note that no scanner is attached.
async fn scan(State(state): State<Arc<AppState>>) -> Json<ScanResponse> {
    match &state.scanner_trigger {
        Some(tx) => {
            // Best-effort: if the scanner task has exited, the send fails
            // silently rather than returning an error to the caller.
            if let Err(e) = tx.try_send(()) {
                tracing::warn!(error = %e, "Failed to send scan trigger; scanner may not be running");
                Json(ScanResponse {
                    ok: true,
                    message: "Scan trigger sent (channel busy or full — scan may already be running)".to_string(),
                })
            } else {
                Json(ScanResponse {
                    ok: true,
                    message: "Scan triggered".to_string(),
                })
            }
        }
        None => Json(ScanResponse {
            ok: true,
            message: "Scan triggered (no scanner attached in this mode)".to_string(),
        }),
    }
}

// ── /events ───────────────────────────────────────────────────────────────────

/// Query parameters for `GET /events`.
#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Only return events with `ts > since` (Unix epoch seconds).
    since: Option<i64>,
    /// Filter by severity level: `"info"`, `"warn"`, or `"error"`.
    level: Option<String>,
    /// Filter by event kind (see [`kb_core::event_kind`] constants).
    kind: Option<String>,
    /// Maximum rows to return (default: 100, capped at 1000).
    limit: Option<i64>,
}

/// `GET /events?since=<ts>&level=<l>&kind=<k>&limit=<n>` — audit event log.
///
/// Returns a JSON array of [`AuditEvent`] objects, ordered newest-first.
async fn get_events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<EventsQuery>,
) -> Result<Json<Vec<AuditEvent>>, ApiError> {
    // Validate level if provided.
    if let Some(ref lvl) = params.level {
        let valid = ["info", "warn", "error"];
        if !valid.contains(&lvl.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "invalid level '{lvl}'; must be one of: info, warn, error"
            )));
        }
    }

    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    let events = state
        .state_store
        .get_events(params.since, params.level, params.kind, limit)
        .await?;

    Ok(Json(events))
}

// ── /tail ─────────────────────────────────────────────────────────────────────

/// `GET /tail` — live SSE stream of audit events.
///
/// Delegates all logic to [`crate::events::tail_sse`], which handles:
/// - `Last-Event-ID` reconnect catchup from the DB
/// - Live broadcast subscription
/// - Optional `?level=` / `?kind=` filtering
async fn tail_sse_handler(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<crate::events::TailParams>,
) -> impl IntoResponse {
    crate::events::tail_sse(state, headers, query).await
}

// ── /metrics ────────────────────────────────────────────────────────────────

/// `GET /metrics` — Prometheus text-format metrics exposition.
///
/// Returns all registered metrics in the
/// [Prometheus text exposition format](https://prometheus.io/docs/instrumenting/exposition_formats/).
///
/// The gauge metrics (`kb_queue_depth`, `kb_in_flight`) are refreshed from
/// the live state store on every request so they always reflect the current
/// queue state.
///
/// Returns `503 Service Unavailable` when the Prometheus recorder was not
/// initialised (e.g. when running in test mode with `metrics_handle: None`).
async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    // ── Refresh point-in-time gauges from the DB ─────────────────────────
    //
    // Gauges for queue_depth and in_flight are derived from the live DB state
    // rather than maintained incrementally, because the DB is the source of
    // truth for these counts.  Updating them here (at scrape time) gives
    // accurate point-in-time values without the complexity of instrumenting
    // every state transition.
    match state.state_store.stats().await {
        Ok(stats) => {
            metrics::gauge!(crate::metrics::QUEUE_DEPTH).set(stats.queued as f64);
            metrics::gauge!(crate::metrics::IN_FLIGHT).set(stats.processing as f64);
        }
        Err(e) => {
            tracing::warn!(error = %e, "metrics: failed to refresh gauge values from state store");
        }
    }

    // ── Render and respond ──────────────────────────────────────────────────
    match &state.metrics_handle {
        Some(handle) => {
            let body = handle.render();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Metrics collector not initialized",
        )
            .into_response(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use std::time::Instant;
    use tower::ServiceExt; // for `oneshot`

    /// Build a minimal [`AppState`] backed by a temp-file SQLite DB for testing.
    async fn test_state() -> Arc<AppState> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        // Keep `dir` alive via Box::leak — acceptable in tests.
        Box::leak(Box::new(dir));
        let store = kb_core::StateStore::new(&db_path, &[5u64, 30, 120])
            .await
            .expect("test state store");
        let broadcaster = crate::EventBroadcaster::new(256);
        Arc::new(AppState {
            state_store: store,
            start_time: Instant::now(),
            scanner_trigger: None,
            event_broadcaster: broadcaster,
            metrics_handle: None,
        })
    }

    // ── /healthz ───────────────────────────────────────────────────────────────

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
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["uptime_secs"].is_number());
    }

    #[tokio::test]
    async fn healthz_uptime_increases_over_time() {
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
            event_broadcaster: crate::EventBroadcaster::new(256),
            metrics_handle: None,
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["uptime_secs"].as_u64().unwrap() >= 10);
    }

    // ── /stats ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stats_returns_200_with_structure() {
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

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["counts_per_status"].is_object());
        assert!(json["queue_depth"].is_number());
        assert!(json["in_flight"].is_number());
        // last_error and oldest_pending_age_secs may be null on empty DB
        assert!(json.get("last_error").is_some());
        assert!(json.get("oldest_pending_age_secs").is_some());
    }

    #[tokio::test]
    async fn stats_counts_per_status_has_all_keys() {
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

        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cps = &json["counts_per_status"];
        for key in &["seen", "queued", "processing", "done", "failed", "skipped"] {
            assert!(cps[key].is_number(), "missing key '{key}' in counts_per_status");
        }
    }

    // ── /files ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_files_returns_empty_array_on_fresh_db() {
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

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_files_bad_status_returns_400() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files?status=badvalue")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── /files/by-path ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn files_by_path_missing_param_returns_400() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files/by-path")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn files_by_path_not_found_returns_404() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files/by-path?path=/nonexistent/file.pdf")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /files/:id ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_file_not_found_returns_404() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/files/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /files/:id/requeue ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn requeue_not_found_returns_404() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/files/9999/requeue")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /files/:id/reset ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn reset_not_found_returns_404() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/files/9999/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /scan ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn scan_returns_ok_without_scanner() {
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

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["message"].is_string());
    }

    #[tokio::test]
    async fn scan_with_trigger_channel_returns_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        Box::leak(Box::new(dir));
        let store = kb_core::StateStore::new(&db_path, &[5u64, 30, 120])
            .await
            .expect("test state store");

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let state = Arc::new(AppState {
            state_store: store,
            start_time: Instant::now(),
            scanner_trigger: Some(tx),
            event_broadcaster: crate::EventBroadcaster::new(256),
            metrics_handle: None,
        });
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

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["ok"], true);
        // Verify the trigger was actually sent.
        assert!(rx.try_recv().is_ok());
    }

    // ── /events ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn events_returns_empty_array_on_fresh_db() {
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

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.is_array());
    }

    #[tokio::test]
    async fn events_bad_level_returns_400() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/events?level=critical")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_accepts_valid_query_params() {
        let state = test_state().await;
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/events?level=info&limit=10&since=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── /tail ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tail_returns_sse_stream() {
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

        // Real SSE handler responds with 200 + text/event-stream
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("text/event-stream"),
            "expected text/event-stream content-type, got: {content_type}",
        );
    }

    #[tokio::test]
    async fn tail_with_filters_returns_sse_stream() {
        let state = test_state().await;
        let app = router(state);

        // Filtering params should still produce a valid SSE response.
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/tail?level=warn&kind=failed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── Integration: file lifecycle ────────────────────────────────────────────

    /// Seed a file row in the DB, then exercise /files, /files/:id,
    /// /files/by-path, /files/:id/requeue, and /files/:id/reset end-to-end.
    #[tokio::test]
    async fn file_lifecycle_endpoints() {
        let state = test_state().await;

        // Seed a file row via StateStore directly.
        let path = PathBuf::from("/tmp/test_vault/sources/doc.pdf");
        let file_row = state
            .state_store
            .register_seen(path.clone(), None, None, None)
            .await
            .expect("register_seen");
        let file_id: i64 = file_row.id;

        // ── GET /files should return 1 row ──
        let app = router(state.clone());
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
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 16384).await.unwrap();
        let arr: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 1);

        // ── GET /files/:id ──
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/files/{file_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 16384).await.unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(detail["file"]["id"], file_id);
        assert!(detail["outputs"].is_array());
        assert!(detail["events"].is_array());

        // ── GET /files/by-path ──
        let app = router(state.clone());
        // Percent-encode the path so it round-trips correctly through the
        // query-string parser (forward slashes must be encoded).
        let encoded = path.to_str().unwrap().replace('/', "%2F");
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/files/by-path?path={encoded}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 16384).await.unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(detail["file"]["id"], file_id);

        // ── POST /files/:id/requeue ──
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/files/{file_id}/requeue"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["previous_status"].is_string());

        // ── POST /files/:id/reset ──
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/files/{file_id}/reset"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["outputs_removed"].is_number());

        // After reset, the file should no longer be in the DB.
        let app = router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/files/{file_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /metrics ──────────────────────────────────────────────────────────────────

    /// Without a metrics handle the endpoint returns 503.
    #[tokio::test]
    async fn metrics_without_handle_returns_503() {
        let state = test_state().await; // metrics_handle = None
        let app = router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
