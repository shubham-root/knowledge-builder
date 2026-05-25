//! Agent integration crate.
//!
//! Houses the post-extraction half of the Knowledge Builder pipeline:
//!
//! * [`plan`]         — JSONL plan reader/writer for the `kb-obsidian`
//!                       wrapper's audit log (kept bit-identical to the
//!                       legacy Python wire format).
//! * [`link_sweeper`] — post-run rewrite of unresolved `[[wikilinks]]`
//!                       inside notes the agent created or modified, so
//!                       the vault never ends up with placeholder stubs.
//! * [`indexer`]      — vault snapshot + diff for the rogue-write audit.
//! * [`prompt`]       — the integration prompt + per-job PATH staging.
//! * [`driver`]       — spawn `pi --mode rpc`, stream JSON-line events,
//!                       drive the agent to completion.
//!
//! No Python interpreter is involved; this crate compiles into the `kb`
//! binary directly.

#![deny(unsafe_code)]
#![warn(missing_docs)]
#![allow(unsafe_code)]

pub mod plan;
pub mod link_sweeper;
pub mod indexer;
pub mod prompt;
pub mod driver;

pub use driver::{run_agent, AgentError, AgentInput, AgentResult};
