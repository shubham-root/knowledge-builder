//! Axum HTTP router and handler stubs.
//!
//! Exposes the local ops API on `127.0.0.1:7878` (loopback only).
//!
//! Endpoints (full implementation: T28 + T29):
//! - `GET  /healthz`
//! - `GET  /stats`
//! - `GET  /files`
//! - `GET  /files/:id`
//! - `GET  /files/by-path`
//! - `POST /files/:id/requeue`
//! - `POST /files/:id/reset`
//! - `POST /scan`
//! - `GET  /events`
//! - `GET  /tail` (SSE — see [`super::sse`])

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

/// Build the axum router with all registered endpoints.
pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        // TODO (T29): add remaining endpoints.
}

/// `GET /healthz` — basic liveness check.
async fn healthz() -> Json<Value> {
    let uptime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Json(json!({ "ok": true, "uptime_secs": uptime }))
}

/// Bind and serve the HTTP API.
///
/// Blocks until the server exits (i.e. until the tokio runtime shuts down).
pub async fn serve(bind_addr: &str) -> kb_core::Result<()> {
    let app = router();
    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "HTTP ops server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
