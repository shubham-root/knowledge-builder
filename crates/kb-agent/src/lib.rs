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
//!
//! Future modules in this crate (Session B continuation):
//!
//! * `driver`         — spawn `pi --mode rpc`, stream JSON-line events,
//!                       write the plan file via the wrapper, drain
//!                       stderr for diagnosability.
//! * `indexer`        — vault snapshot + diff for the rogue-write audit.
//!
//! No Python interpreter is involved; this crate compiles into the `kb`
//! binary directly.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod plan;
pub mod link_sweeper;
