//! `kb-ops` — Observability and HTTP API layer for Knowledge Builder.
//!
//! Provides:
//! - [`api`]    — `axum` router wiring all HTTP endpoints together.
//! - [`sse`]    — Server-Sent Events tail stream (`GET /tail`).

pub mod api;
pub mod sse;
