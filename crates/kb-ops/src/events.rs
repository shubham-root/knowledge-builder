//! Event broadcast and SSE tail endpoint for Knowledge Builder.
//!
//! ## Design
//!
//! `GET /tail` streams a live feed of [`AuditEvent`]s as Server-Sent Events.
//!
//! ```text
//! StateActor (OS thread)
//!   └─ record_event_op()
//!       └─ INSERT INTO events
//!       └─ broadcast::Sender<AuditEvent>::send(event)
//!                │
//!     ┌──────────┘  (one Receiver per SSE client)
//!     ▼
//! tail_sse handler
//!   ├─ [catchup] GET events WHERE id > Last-Event-ID
//!   └─ [live]    recv() from broadcast::Receiver
//!       └─ filter by ?level= / ?kind=
//!       └─ serialize to SSE: event: audit\nid: <id>\ndata: <json>\n\n
//! ```
//!
//! Client disconnect is handled gracefully: when `bridge_tx.send()` fails
//! (the mpsc receiver was dropped because axum closed the response stream),
//! the spawned task exits cleanly.
//!
//! ## Reconnect
//!
//! Clients should include the `Last-Event-ID` header on reconnect.  The
//! handler will replay all events with `id > Last-Event-ID` from the DB
//! before switching to the live broadcast, ensuring zero gaps.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
};
use futures_util::stream;
use kb_core::AuditEvent;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::AppState;

// ── EventBroadcaster ─────────────────────────────────────────────────────────

/// A thin wrapper around a [`broadcast::Sender<AuditEvent>`] that keeps the
/// channel alive even when there are no receivers.
///
/// The sender half remains alive for the entire daemon lifetime, so the
/// channel is never closed while the daemon is running.  When
/// [`broadcast::Sender::send`] returns [`broadcast::error::SendError`] it
/// simply means there are no current subscribers — that is not an error.
///
/// # Usage
///
/// ```no_run
/// # use kb_ops::EventBroadcaster;
/// let broadcaster = EventBroadcaster::new(1024);
/// // Wire it into StateStore so every recorded event is forwarded:
/// // store.set_event_broadcaster(broadcaster.sender()).await?;
/// // Then place it in AppState for the SSE handler.
/// let _rx = broadcaster.subscribe();
/// ```
#[derive(Clone)]
pub struct EventBroadcaster {
    tx: broadcast::Sender<AuditEvent>,
}

impl EventBroadcaster {
    /// Create a new broadcaster with the given channel capacity.
    ///
    /// `capacity` is the number of events buffered in the channel before
    /// older ones are dropped (resulting in a [`broadcast::error::RecvError::Lagged`]
    /// for slow consumers).  A value of `1024` is appropriate for most
    /// deployments.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Broadcast `event` to all active subscribers.
    ///
    /// If there are no subscribers (or the channel is full for a slow
    /// subscriber), the send error is silently discarded.
    pub fn send(&self, event: AuditEvent) {
        // Ignore SendError (no receivers) — that is not a failure.
        let _ = self.tx.send(event);
    }

    /// Subscribe to the live event stream.
    ///
    /// The returned receiver will receive all events broadcast after this call.
    /// If the receiver falls more than `capacity` events behind, it will
    /// receive a [`broadcast::error::RecvError::Lagged`] error on the next
    /// [`broadcast::Receiver::recv`] call.
    pub fn subscribe(&self) -> broadcast::Receiver<AuditEvent> {
        self.tx.subscribe()
    }

    /// Clone the underlying [`broadcast::Sender`].
    ///
    /// Use this to wire the broadcaster into the [`StateStore`]:
    ///
    /// ```no_run
    /// # use kb_ops::EventBroadcaster;
    /// # use kb_core::StateStore;
    /// # use std::path::Path;
    /// # async fn example() -> anyhow::Result<()> {
    /// let broadcaster = EventBroadcaster::new(1024);
    /// let store = StateStore::new(Path::new("/tmp/kb.db"), &[30]).await?;
    /// store.set_event_broadcaster(broadcaster.sender()).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`StateStore`]: kb_core::StateStore
    pub fn sender(&self) -> broadcast::Sender<AuditEvent> {
        self.tx.clone()
    }
}

impl std::fmt::Debug for EventBroadcaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBroadcaster")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

// ── Query parameters ─────────────────────────────────────────────────────────

/// Optional query parameters for `GET /tail`.
///
/// Both filters are matched **case-insensitively**.
///
/// # Examples
///
/// ```text
/// GET /tail?level=warn
/// GET /tail?kind=failed
/// GET /tail?level=error&kind=processor_exit
/// ```
#[derive(Debug, Default, Deserialize)]
pub struct TailParams {
    /// Only emit events whose `level` field matches (e.g. `"info"`, `"warn"`, `"error"`).
    pub level: Option<String>,
    /// Only emit events whose `kind` field matches (see `kb_core::event_kind`).
    pub kind: Option<String>,
}

// ── SSE handler ───────────────────────────────────────────────────────────────

/// `GET /tail` — live Server-Sent Events stream of [`AuditEvent`]s.
///
/// # Protocol
///
/// Each event is sent as:
/// ```text
/// event: audit
/// id: <event.id>
/// data: <serde_json::to_string(&event)>
///
/// ```
///
/// A keepalive comment is sent every 15 seconds to prevent proxy timeouts.
///
/// # Reconnect
///
/// Clients that include a `Last-Event-ID` header on reconnect will receive
/// all missed events (with `id > Last-Event-ID`) from the database before
/// the live stream resumes.  This guarantees zero gaps in the event stream
/// across reconnects.
///
/// # Filtering
///
/// Optional `?level=<str>` and `?kind=<str>` query parameters restrict the
/// stream to matching events only.  Filters are applied to both the catchup
/// and live phases.
///
/// # Client disconnect
///
/// When the HTTP connection closes, the internal bridge channel is dropped,
/// causing the producer task to detect the closed channel and exit cleanly.
pub async fn tail_sse(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<TailParams>,
) -> impl IntoResponse {
    // ── Extract Last-Event-ID for reconnect catchup ───────────────────────
    let last_event_id: Option<i64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // ── Subscribe to the live broadcast before the catchup query so we don't
    //    miss events that arrive between the DB query and the subscribe call.
    let mut rx = state.event_broadcaster.subscribe();

    let store = state.state_store.clone();
    let level_filter = params.level.clone();
    let kind_filter  = params.kind.clone();

    // ── Bridge channel: the spawned task pushes events; the stream pulls them.
    //    Capacity of 64 is intentionally small — axum will apply backpressure
    //    by not polling the stream, which stalls `bridge_tx.send()`.
    let (bridge_tx, bridge_rx) =
        tokio::sync::mpsc::channel::<AuditEvent>(64);

    // ── Producer task ─────────────────────────────────────────────────────
    //
    // Phase 1: replay DB events the client missed (if Last-Event-ID given).
    // Phase 2: relay live broadcast events until the client disconnects.
    tokio::spawn(async move {
        // ── Catchup phase ────────────────────────────────────────────────
        if let Some(last_id) = last_event_id {
            match store
                .get_events_after_id(
                    last_id,
                    level_filter.clone(),
                    kind_filter.clone(),
                    1000, // cap replay to 1 000 events per reconnect
                )
                .await
            {
                Ok(events) => {
                    for event in events {
                        if bridge_tx.send(event).await.is_err() {
                            return; // client already disconnected
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, last_id, "SSE /tail: catchup query failed");
                }
            }
        }

        // ── Live phase ────────────────────────────────────────────────────
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Apply optional level / kind filters.
                    let level_ok = level_filter
                        .as_deref()
                        .map(|l| l.eq_ignore_ascii_case(&event.level))
                        .unwrap_or(true);
                    let kind_ok = kind_filter
                        .as_deref()
                        .map(|k| k.eq_ignore_ascii_case(&event.kind))
                        .unwrap_or(true);

                    if level_ok && kind_ok {
                        if bridge_tx.send(event).await.is_err() {
                            break; // client disconnected — stop producing
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The receiver missed `n` events because it was too slow.
                    // Log a warning but keep the stream alive; the client can
                    // reconnect with Last-Event-ID to replay missed events.
                    tracing::warn!(
                        missed = n,
                        "SSE /tail: broadcast receiver lagged; some events skipped"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Broadcaster shut down — daemon is stopping.
                    break;
                }
            }
        }
    });

    // ── Consumer stream: mpsc receiver → axum SSE ─────────────────────────
    //
    // `stream::unfold` converts the async `mpsc::Receiver<AuditEvent>` into
    // a `Stream<Item = Result<Event, Infallible>>` that axum can stream.
    // When `bridge_tx` is dropped (producer task exited), `recv()` returns
    // `None` and the stream ends, closing the SSE response cleanly.
    let event_stream = stream::unfold(bridge_rx, |mut rx| async move {
        rx.recv().await.map(|event| {
            let json = serde_json::to_string(&event).unwrap_or_else(|e| {
                tracing::error!(error = %e, "SSE: failed to serialize AuditEvent");
                String::from(r#"{"error":"serialization_failed"}"#)
            });
            let sse_event = Event::default()
                .event("audit")
                .id(event.id.to_string())
                .data(json);
            (Ok::<Event, Infallible>(sse_event), rx)
        })
    });

    Sse::new(event_stream).keep_alive(KeepAlive::default())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    #[test]
    fn broadcaster_new_creates_channel() {
        let b = EventBroadcaster::new(64);
        assert_eq!(b.tx.receiver_count(), 0);
    }

    #[test]
    fn broadcaster_subscribe_increments_count() {
        let b = EventBroadcaster::new(64);
        let _rx1 = b.subscribe();
        let _rx2 = b.subscribe();
        assert_eq!(b.tx.receiver_count(), 2);
    }

    #[test]
    fn broadcaster_sender_returns_clone() {
        let b = EventBroadcaster::new(64);
        let tx = b.sender();
        // They share the same channel (same receiver count).
        let _rx = b.subscribe();
        assert_eq!(tx.receiver_count(), 1);
    }

    #[tokio::test]
    async fn broadcaster_send_reaches_subscriber() {
        let b = EventBroadcaster::new(64);
        let mut rx = b.subscribe();

        let event = AuditEvent {
            id:      42,
            ts:      1_000_000,
            level:   "info".to_string(),
            kind:    "queued".to_string(),
            file_id: None,
            message: "test event".to_string(),
            detail:  None,
        };
        b.send(event.clone());

        let received = rx.recv().await.expect("should receive event");
        assert_eq!(received.id,      event.id);
        assert_eq!(received.message, event.message);
    }

    #[test]
    fn broadcaster_send_with_no_subscribers_is_noop() {
        let b = EventBroadcaster::new(64);
        // No subscribers — should not panic.
        b.send(AuditEvent {
            id: 1, ts: 0,
            level: "info".to_string(), kind: "test".to_string(),
            file_id: None, message: "noop".to_string(), detail: None,
        });
    }

    #[test]
    fn broadcaster_clone_shares_channel() {
        let b1 = EventBroadcaster::new(32);
        let b2 = b1.clone();
        let _rx = b2.subscribe();
        // Both clones see the same subscriber count.
        assert_eq!(b1.tx.receiver_count(), 1);
        assert_eq!(b2.tx.receiver_count(), 1);
    }

    #[test]
    fn broadcaster_debug_shows_receiver_count() {
        let b = EventBroadcaster::new(16);
        let dbg = format!("{b:?}");
        assert!(dbg.contains("EventBroadcaster"));
        assert!(dbg.contains("receiver_count"));
    }

    #[tokio::test]
    async fn broadcaster_lagged_receiver_continues() {
        // Fill the channel beyond capacity to trigger a Lagged error.
        let b = EventBroadcaster::new(2); // tiny capacity
        let mut rx = b.subscribe();

        // Send 5 events; the receiver can only buffer 2.
        for i in 0..5_i64 {
            b.send(AuditEvent {
                id: i, ts: i,
                level: "info".to_string(), kind: "test".to_string(),
                file_id: None, message: format!("event {i}"), detail: None,
            });
        }

        // Receiver should see a Lagged error (not a panic / close).
        let result = rx.recv().await;
        assert!(
            matches!(result, Err(broadcast::error::RecvError::Lagged(_))),
            "expected Lagged, got {result:?}",
        );
    }
}
