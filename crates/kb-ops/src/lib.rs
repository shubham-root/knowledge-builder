//! `kb-ops` — Observability and HTTP API layer for Knowledge Builder.
//!
//! Provides:
//! - [`AppState`]          — shared state passed to every HTTP handler via `Arc`.
//! - [`EventBroadcaster`]  — real-time broadcast channel for [`AuditEvent`]s.
//! - [`start_server`]      — bind the axum server and return its join handle.
//! - [`api`]               — route wiring and all HTTP handlers.
//! - [`events`]            — `EventBroadcaster` and SSE `GET /tail` handler.
//!
//! # Usage
//!
//! ```no_run
//! use kb_ops::{AppState, EventBroadcaster, start_server};
//! use kb_core::StateStore;
//! use tokio_util::sync::CancellationToken;
//! use std::time::Instant;
//! use std::path::Path;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let broadcaster = EventBroadcaster::new(1024);
//! let store = StateStore::new(Path::new("/tmp/kb.db"), &[5, 30, 120]).await?;
//! // Wire broadcaster so every recorded event is also broadcast to SSE clients.
//! store.set_event_broadcaster(broadcaster.sender()).await?;
//! let state = AppState {
//!     state_store: store,
//!     start_time: Instant::now(),
//!     scanner_trigger: None,
//!     event_broadcaster: broadcaster,
//! };
//! let shutdown = CancellationToken::new();
//! let _handle = start_server("127.0.0.1:7878", state, shutdown).await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`AuditEvent`]: kb_core::AuditEvent

pub mod api;
pub mod events;
pub mod sse;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use kb_core::StateStore;

pub use events::EventBroadcaster;

// ── AppState ──────────────────────────────────────────────────────────────────

/// Shared application state injected into every axum handler via [`Arc`].
///
/// Clone is derived so the `Arc<AppState>` axum extension can be cheaply
/// extracted in each request handler.  The inner fields that are themselves
/// `Clone` (like [`StateStore`]) can be cloned from the extracted reference.
///
/// `Debug` is implemented manually because [`StateStore`] does not implement
/// [`std::fmt::Debug`].
#[derive(Clone)]
pub struct AppState {
    /// Handle to the single-writer SQLite state actor.
    pub state_store: StateStore,

    /// Daemon start instant — used to compute `/healthz` `uptime_secs`.
    pub start_time: Instant,

    /// Optional channel to trigger an immediate full vault scan.
    ///
    /// Send `()` on this sender to request the [`PeriodicScanner`] to run
    /// one scan cycle immediately (used by `POST /scan`).  `None` when the
    /// scanner is not running (e.g. in unit tests).
    pub scanner_trigger: Option<mpsc::Sender<()>>,

    /// Broadcast channel for pushing live [`AuditEvent`]s to SSE subscribers.
    ///
    /// Every call to [`StateStore::record_event`] (including internal daemon
    /// events) causes the new event to be sent on this channel.  SSE clients
    /// subscribe via [`EventBroadcaster::subscribe`].
    ///
    /// [`AuditEvent`]: kb_core::AuditEvent
    /// [`StateStore::record_event`]: kb_core::StateStore::record_event
    pub event_broadcaster: EventBroadcaster,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("start_time", &self.start_time)
            .field("scanner_trigger", &self.scanner_trigger.as_ref().map(|_| "<Sender<()>>"))
            .field("event_broadcaster", &self.event_broadcaster)
            .finish_non_exhaustive()
    }
}

// ── start_server ──────────────────────────────────────────────────────────────

/// Bind the axum HTTP server and start serving requests.
///
/// # Arguments
///
/// - `bind_addr` — TCP address to listen on (e.g. `"127.0.0.1:7878"`).
/// - `app_state` — Shared daemon state; wrapped in [`Arc`] internally so all
///   handlers see the same instance.
/// - `shutdown` — [`CancellationToken`] that signals graceful shutdown.  When
///   cancelled, the server stops accepting new connections and waits for
///   in-flight requests to complete.
///
/// # Returns
///
/// A [`JoinHandle`] for the server task.  Await it (or drop it) to join
/// the server after cancellation.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound.
pub async fn start_server(
    bind_addr: &str,
    app_state: AppState,
    shutdown: CancellationToken,
) -> Result<JoinHandle<()>> {
    let shared_state = Arc::new(app_state);

    // Build the full router (all routes wired, TraceLayer for request logging).
    let app = api::router(shared_state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(%local_addr, "HTTP ops server listening");

    let handle = tokio::spawn(async move {
        let serve_future = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.cancelled().await;
                tracing::info!("HTTP ops server shutting down gracefully");
            });

        if let Err(e) = serve_future.await {
            tracing::error!(error = %e, "HTTP ops server error");
        }
        tracing::info!("HTTP ops server stopped");
    });

    Ok(handle)
}
