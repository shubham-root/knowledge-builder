//! HTTP client for communicating with a running Knowledge Builder daemon.
//!
//! Provides [`DaemonClient`] which wraps a `reqwest::Client` and exposes typed
//! methods mirroring the daemon's HTTP API (`/healthz`, `/stats`, `/files`,
//! `/scan`, `/tail`, …).
//!
//! # Connection detection
//!
//! [`DaemonClient::try_connect`] probes `GET /healthz` with a **1-second**
//! timeout.  If the daemon is not running (connection refused, timeout, any
//! error), it returns `None` without blocking the user.  All CLI commands
//! call this first and fall back to direct SQLite access when it returns
//! `None`.
//!
//! # Usage
//!
//! ```no_run
//! # async fn example() -> anyhow::Result<()> {
//! use kb_cli::client::DaemonClient;
//!
//! if let Some(client) = DaemonClient::try_connect("127.0.0.1:7878").await {
//!     let stats = client.get_stats().await?;
//!     println!("queue depth: {}", stats.queue_depth);
//! } else {
//!     println!("daemon not running — falling back to DB");
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use futures_util::{stream, Stream, StreamExt};
use kb_core::{AuditEvent, FileRow, OutputRecord};
use serde::Deserialize;
use tokio::time::Duration;

// ── Public HTTP response types ────────────────────────────────────────────────

/// Stats payload returned by `GET /stats`.
///
/// Mirrors the private `StatsResponse` struct in `kb-ops/src/api.rs`.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpStats {
    /// Per-status counts, e.g. `{"queued": 3, "done": 42, ...}`.
    pub counts_per_status: HashMap<String, i64>,
    /// Total `queued + processing` rows.
    pub queue_depth: i64,
    /// Most recent failure message, if any.
    pub last_error: Option<String>,
    /// Rows currently being processed (same as `counts_per_status["processing"]`).
    ///
    /// Exposed as a top-level field by the HTTP API for convenience.
    #[allow(dead_code)]
    pub in_flight: i64,
    /// Age in seconds of the oldest pending entry; `None` when idle.
    pub oldest_pending_age_secs: Option<i64>,
}

impl HttpStats {
    /// Convenience accessor for a named status count (returns 0 if absent).
    pub fn count(&self, status: &str) -> i64 {
        self.counts_per_status.get(status).copied().unwrap_or(0)
    }
}

/// Detailed file payload returned by `GET /files/:id` and `GET /files/by-path`.
///
/// Mirrors the private `FileDetailResponse` struct in `kb-ops/src/api.rs`.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpFileDetail {
    /// The file's current state (all columns from the `files` table).
    pub file: FileRow,
    /// Output artifacts produced by the processor.
    pub outputs: Vec<OutputRecord>,
    /// Recent audit events associated with this file.
    pub events: Vec<AuditEvent>,
}

// ── DaemonClient ──────────────────────────────────────────────────────────────

/// Typed HTTP client for a running Knowledge Builder daemon.
///
/// Obtained via [`DaemonClient::try_connect`]; if that returns `None`, the
/// daemon is not reachable and callers should fall back to direct DB access.
#[derive(Clone)]
pub struct DaemonClient {
    /// Base URL, e.g. `"http://127.0.0.1:7878"`.
    base_url: String,
    /// Shared reqwest client (connection pool, keep-alive, codec support).
    http_client: reqwest::Client,
}

impl DaemonClient {
    // ── Connection detection ──────────────────────────────────────────────────

    /// Try to connect to a daemon listening at `bind_addr`
    /// (e.g. `"127.0.0.1:7878"`).
    ///
    /// Probes `GET /healthz` with a **1-second** timeout.
    ///
    /// Returns `Some(client)` if the daemon is reachable, `None` on any
    /// connection failure (refused, timeout, OS error, etc.).
    pub async fn try_connect(bind_addr: &str) -> Option<Self> {
        let http_client = reqwest::Client::builder()
            .build()
            .ok()?;

        let base_url = format!("http://{}", bind_addr);
        let healthz_url = format!("{}/healthz", base_url);

        let result = http_client
            .get(&healthz_url)
            .timeout(Duration::from_secs(1))
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => Some(DaemonClient {
                base_url,
                http_client,
            }),
            _ => None,
        }
    }

    // ── Statistics ────────────────────────────────────────────────────────────

    /// Fetch aggregate queue statistics (`GET /stats`).
    pub async fn get_stats(&self) -> Result<HttpStats> {
        self.get_json("/stats").await
    }

    // ── File listing and lookup ───────────────────────────────────────────────

    /// List files with an optional status filter and pagination
    /// (`GET /files?status=&limit=&offset=`).
    pub async fn list_files(
        &self,
        status: Option<kb_core::Status>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<FileRow>> {
        let base = format!("{}/files", self.base_url);
        let mut req = self
            .http_client
            .get(&base)
            .query(&[("limit", limit.to_string()), ("offset", offset.to_string())]);

        if let Some(s) = status {
            req = req.query(&[("status", s.as_str())]);
        }

        let resp = req
            .send()
            .await
            .context("GET /files")?;

        if !resp.status().is_success() {
            let status_code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("GET /files returned {status_code}: {body}");
        }

        resp.json::<Vec<FileRow>>()
            .await
            .context("deserializing GET /files response")
    }

    /// Look up a file by numeric ID (`GET /files/:id`).
    ///
    /// Returns `None` when no file with that ID exists.
    pub async fn get_file_by_id(&self, id: i64) -> Result<Option<HttpFileDetail>> {
        let url = format!("{}/files/{}", self.base_url, id);
        self.get_json_optional(&url).await
    }

    /// Look up a file by its filesystem path (`GET /files/by-path?path=`).
    ///
    /// Returns `None` when the path is not tracked.
    pub async fn get_file_by_path(&self, path: &str) -> Result<Option<HttpFileDetail>> {
        let url = format!("{}/files/by-path", self.base_url);
        let resp = self
            .http_client
            .get(&url)
            .query(&[("path", path)])
            .send()
            .await
            .context("GET /files/by-path")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::BAD_REQUEST
        {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status_code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("GET /files/by-path returned {status_code}: {body}");
        }

        resp.json::<HttpFileDetail>()
            .await
            .map(Some)
            .context("deserializing GET /files/by-path response")
    }

    /// Resolve a `<path|id>` CLI argument to a full [`HttpFileDetail`].
    ///
    /// Resolution strategy (mirrors the offline `db::resolve_target`):
    /// 1. Parse as a **positive integer** → `GET /files/:id`.
    /// 2. Otherwise treat as a filesystem path (tilde-expanded, made absolute)
    ///    → `GET /files/by-path`.
    pub async fn resolve_target(&self, target: &str) -> Result<HttpFileDetail> {
        // ── Numeric ID ────────────────────────────────────────────────────────
        if let Ok(id) = target.parse::<i64>() {
            if id > 0 {
                return self
                    .get_file_by_id(id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no file found with ID {id}"));
            }
        }

        // ── Path lookup ───────────────────────────────────────────────────────
        let expanded = crate::commands::db::expand_tilde(target);
        let path = if expanded.is_absolute() {
            expanded
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(&expanded)
        };

        let path_str = path.to_string_lossy().to_string();

        // First attempt: path as-is.
        if let Some(detail) = self.get_file_by_path(&path_str).await? {
            return Ok(detail);
        }

        // Second attempt: canonicalize (resolves symlinks / `..`) and retry.
        if let Ok(canon) = std::fs::canonicalize(&path) {
            let canon_str = canon.to_string_lossy().to_string();
            if canon_str != path_str {
                if let Some(detail) = self.get_file_by_path(&canon_str).await? {
                    return Ok(detail);
                }
            }
        }

        bail!(
            "no file found for '{}'. \
             Use `kb list` to see tracked files, or provide a numeric ID.",
            target
        )
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    /// Re-queue a file for processing (`POST /files/:id/requeue`).
    ///
    /// Returns the file's **previous status** string (e.g. `"failed"`).
    pub async fn requeue(&self, id: i64) -> Result<String> {
        #[derive(Deserialize)]
        struct RequeueResp {
            previous_status: String,
        }
        let resp: RequeueResp = self
            .post_empty(&format!("/files/{}/requeue", id))
            .await?;
        Ok(resp.previous_status)
    }

    /// Delete a file's DB record (`POST /files/:id/reset`).
    ///
    /// Returns the number of associated output records that were removed.
    pub async fn reset(&self, id: i64) -> Result<u64> {
        #[derive(Deserialize)]
        struct ResetResp {
            outputs_removed: u64,
        }
        let resp: ResetResp = self
            .post_empty(&format!("/files/{}/reset", id))
            .await?;
        Ok(resp.outputs_removed)
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    /// Trigger an immediate full-vault scan (`POST /scan`).
    pub async fn trigger_scan(&self) -> Result<()> {
        self.post_empty::<serde_json::Value>("/scan").await?;
        Ok(())
    }

    // ── SSE tail ──────────────────────────────────────────────────────────────

    /// Subscribe to the live audit-event stream via SSE (`GET /tail`).
    ///
    /// Returns a [`Stream`] that yields [`AuditEvent`] items as they arrive.
    /// The stream ends naturally when the daemon closes the connection (e.g.
    /// on shutdown).
    ///
    /// An optional `kind` filter is passed as a query parameter so the daemon
    /// can filter server-side.  Level filtering is left to the caller because
    /// the CLI uses minimum-severity semantics that differ from the server's
    /// exact-match semantics.
    pub async fn tail(
        &self,
        kind: Option<&str>,
    ) -> Result<impl Stream<Item = anyhow::Result<AuditEvent>>> {
        let url = format!("{}/tail", self.base_url);
        let mut req = self.http_client.get(&url);
        if let Some(k) = kind {
            req = req.query(&[("kind", k)]);
        }

        let resp = req.send().await.context("GET /tail")?;

        if !resp.status().is_success() {
            let status_code = resp.status();
            bail!("GET /tail returned {status_code}");
        }

        Ok(sse_event_stream(resp))
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// `GET <base_url><path>` → deserialize JSON body as `T`.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        self.get_json_url(&url).await
    }

    /// `GET <url>` → deserialize JSON body as `T`, propagating all errors.
    async fn get_json_url<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .http_client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if !resp.status().is_success() {
            let status_code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url} returned {status_code}: {body}");
        }

        resp.json::<T>()
            .await
            .with_context(|| format!("deserializing response from GET {url}"))
    }

    /// `GET <url>` → `None` on 404, error on other failures, `Some(T)` on 200.
    async fn get_json_optional<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<Option<T>> {
        let resp = self
            .http_client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status_code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url} returned {status_code}: {body}");
        }

        Ok(Some(
            resp.json::<T>()
                .await
                .with_context(|| format!("deserializing response from GET {url}"))?,
        ))
    }

    /// `POST <base_url><path>` with an empty body → deserialize JSON response.
    async fn post_empty<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);

        let resp = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        if !resp.status().is_success() {
            let status_code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("POST {url} returned {status_code}: {body}");
        }

        resp.json::<T>()
            .await
            .with_context(|| format!("deserializing response from POST {url}"))
    }
}

// ── SSE stream helper ─────────────────────────────────────────────────────────

/// Consume a `reqwest::Response` containing an SSE stream and return a typed
/// `Stream` of [`AuditEvent`] items.
///
/// A background tokio task reads raw bytes from the response, buffers them,
/// and splits on the SSE double-newline event separator (`"\n\n"`).  Each
/// complete event block is passed to [`parse_sse_block`]; well-formed
/// `data:` lines are deserialized as `AuditEvent` and forwarded on a channel.
///
/// The stream ends when the daemon closes the connection or the receiver is
/// dropped.
fn sse_event_stream(
    response: reqwest::Response,
) -> impl Stream<Item = anyhow::Result<AuditEvent>> {
    // A bounded channel acts as backpressure so the background task does not
    // race ahead of the consumer indefinitely.
    let (tx, rx) = tokio::sync::mpsc::channel::<anyhow::Result<AuditEvent>>(256);

    tokio::spawn(async move {
        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    // Append the chunk to our line buffer.
                    match std::str::from_utf8(&bytes) {
                        Ok(s) => buffer.push_str(s),
                        Err(_) => {
                            // Non-UTF-8 chunk — skip silently (SSE is always
                            // text/event-stream encoded as UTF-8).
                            continue;
                        }
                    }

                    // Drain all complete SSE events (delimited by "\n\n").
                    loop {
                        if let Some(pos) = buffer.find("\n\n") {
                            let block = buffer[..pos].to_string();
                            // Remove the consumed block including the separator.
                            buffer.drain(..pos + 2);

                            if let Some(event) = parse_sse_block(&block) {
                                if tx.send(Ok(event)).await.is_err() {
                                    // Receiver was dropped — stop silently.
                                    return;
                                }
                            }
                        } else {
                            break; // Need more data before next complete event.
                        }
                    }
                }
                Err(e) => {
                    // Network error — forward and terminate.
                    let _ = tx
                        .send(Err(anyhow::anyhow!("SSE stream error: {e}")))
                        .await;
                    return;
                }
            }
        }
        // Stream ended (daemon closed connection) — channel drops, consumer
        // sees None on the next poll.
    });

    // Convert the mpsc::Receiver into a Stream<Item = Result<AuditEvent>>.
    stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

/// Parse one SSE event block (the text between two `"\n\n"` separators) and
/// return the deserialized [`AuditEvent`] if possible.
///
/// Returns `None` for comment-only blocks (keepalive pings starting with `:`),
/// blocks with no `data:` line, or blocks whose data is not valid
/// `AuditEvent` JSON.
fn parse_sse_block(block: &str) -> Option<AuditEvent> {
    let mut data: Option<String> = None;

    for line in block.lines() {
        // SSE comment / keepalive — e.g. `: keep-alive`.
        if line.starts_with(':') {
            continue;
        }
        // `data: <json>` — we only care about the data field.
        if let Some(value) = line.strip_prefix("data:") {
            data = Some(value.trim().to_string());
        }
        // `event:`, `id:`, `retry:` lines are intentionally ignored; we
        // identify events solely by their JSON content.
    }

    let data = data.filter(|s| !s.is_empty())?;
    serde_json::from_str::<AuditEvent>(&data).ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_block_valid_event() {
        let json = r#"{"id":1,"ts":1700000000,"level":"info","kind":"queued","file_id":42,"message":"Enqueued","detail":null}"#;
        let block = format!("event: audit\nid: 1\ndata: {json}");
        let event = parse_sse_block(&block).expect("should parse");
        assert_eq!(event.id, 1);
        assert_eq!(event.kind, "queued");
    }

    #[test]
    fn parse_sse_block_comment_only() {
        let block = ": keep-alive";
        assert!(parse_sse_block(block).is_none());
    }

    #[test]
    fn parse_sse_block_no_data_line() {
        let block = "event: audit\nid: 42";
        assert!(parse_sse_block(block).is_none());
    }

    #[test]
    fn parse_sse_block_malformed_json() {
        let block = "data: {not valid json}";
        assert!(parse_sse_block(block).is_none());
    }

    #[test]
    fn parse_sse_block_empty_data() {
        let block = "data: ";
        assert!(parse_sse_block(block).is_none());
    }

    #[test]
    fn parse_sse_block_data_with_leading_space() {
        // The SSE spec says the field value starts after the optional single
        // space following the colon.  Our parser uses `trim()` to be robust.
        let json = r#"{"id":2,"ts":0,"level":"warn","kind":"failed","file_id":null,"message":"x","detail":null}"#;
        let block = format!("data:  {json}"); // two spaces after colon
        let event = parse_sse_block(&block).expect("should parse");
        assert_eq!(event.level, "warn");
    }

    #[test]
    fn parse_sse_block_uses_last_data_line() {
        // When multiple `data:` lines appear (multi-line SSE data), our parser
        // picks the last one.  In practice the daemon emits single-line JSON.
        let json1 = r#"{"id":1,"ts":0,"level":"info","kind":"a","file_id":null,"message":"first","detail":null}"#;
        let json2 = r#"{"id":2,"ts":0,"level":"info","kind":"b","file_id":null,"message":"second","detail":null}"#;
        let block = format!("data: {json1}\ndata: {json2}");
        let event = parse_sse_block(&block).expect("should parse");
        assert_eq!(event.kind, "b");
    }

    #[test]
    fn http_stats_count_helper() {
        let mut counts = HashMap::new();
        counts.insert("queued".to_string(), 5_i64);
        counts.insert("done".to_string(), 100_i64);
        let stats = HttpStats {
            counts_per_status: counts,
            queue_depth: 5,
            last_error: None,
            in_flight: 0,
            oldest_pending_age_secs: None,
        };
        assert_eq!(stats.count("queued"), 5);
        assert_eq!(stats.count("done"), 100);
        assert_eq!(stats.count("failed"), 0); // absent key → 0
    }
}
