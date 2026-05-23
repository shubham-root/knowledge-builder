//! Bounded worker pool with atomic job claiming.
//!
//! Uses a `tokio::sync::Semaphore` to cap parallelism at
//! `concurrency` simultaneous subprocesses.  Workers atomically
//! claim `queued` rows from the state store to prevent double-processing.
//!
//! Full implementation: T15.

use std::sync::Arc;
use tokio::sync::Semaphore;

/// A bounded pool of processing workers.
#[allow(dead_code)] // semaphore will be used in T15 implementation
pub struct WorkerPool {
    semaphore:   Arc<Semaphore>,
    concurrency: usize,
}

impl WorkerPool {
    /// Create a new pool capped at `concurrency` simultaneous jobs.
    pub fn new(concurrency: usize) -> Self {
        Self {
            semaphore:   Arc::new(Semaphore::new(concurrency)),
            concurrency,
        }
    }

    /// Returns the configured concurrency limit.
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Start the claim-process loop.
    ///
    /// Continuously acquires a semaphore permit, claims the next `queued`
    /// job from the state store, and spawns a task to process it.
    pub async fn run(self) -> kb_core::Result<()> {
        // TODO (T15): implement claim → spawn loop.
        tracing::debug!(concurrency = self.concurrency, "worker pool stub — not yet implemented");
        Ok(())
    }
}
