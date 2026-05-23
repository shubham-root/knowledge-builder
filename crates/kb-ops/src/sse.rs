//! Server-Sent Events (SSE) tail endpoint.
//!
//! `GET /tail` streams new rows from the `events` audit-log table as they
//! are inserted, allowing `kb tail` and any browser client to follow the
//! daemon's activity in real time.
//!
//! Full implementation: T30.

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures_util::stream;

/// `GET /tail` — SSE stream of audit events.
///
/// Sends a keepalive comment every 15 s to prevent proxy timeouts.
pub async fn tail_sse() -> impl IntoResponse {
    // TODO (T30): poll `events` table and broadcast new rows.
    let placeholder = stream::empty::<Result<Event, std::convert::Infallible>>();
    Sse::new(placeholder).keep_alive(KeepAlive::default())
}
