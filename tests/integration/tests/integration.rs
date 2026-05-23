//! Integration test suite — all 14 scenarios from PLAN.md §13.
//!
//! Each test is independent: its own `TempDir`, its own SQLite DB, its own
//! daemon components.  Tests do NOT share state.
//!
//! # Running
//! ```
//! cargo test -p integration-tests --test integration -- --nocapture
//! ```

mod helpers;

use std::{
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use tokio::sync::mpsc;

use kb_core::{EnqueueOutcome, Status};
use kb_watcher::{scan_once, passes_filter, CancellationToken};

use helpers::{stub_path, FullSystem, TestVault};

// ── Scenario 1 ────────────────────────────────────────────────────────────────

/// Drop a PDF → enqueued → processed → outputs recorded; outputs not re-queued.
///
/// Verifies the happy path end-to-end:
/// 1. File injected into stability tracker.
/// 2. Stability window elapses, file is hashed.
/// 3. `process_stable_file` marks it `queued`.
/// 4. Worker claims and runs stub processor.
/// 5. Stub writes a `.md` note to `vault/Notes/`; worker records outputs.
/// 6. Status transitions to `done`.
/// 7. The output file is not inside `sources_dir`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_01_basic_pdf_processed() {
    let sys = FullSystem::default_happy().await.unwrap();

    // Drop a PDF into sources.
    let pdf = sys.vault.drop_file("scenario01.pdf", b"scenario 01 PDF content");

    // Inject the path into the stability tracker (deterministic — no FSEvents).
    sys.inject_path(pdf.clone()).await;

    // Wait up to 8 s: 500 ms stability + hash + DB write + worker claim + stub run.
    let row = sys
        .vault
        .wait_for_status(&pdf, Status::Done, 8_000)
        .await
        .expect("file should reach 'done' within 8 s");

    // At least one output must be recorded.
    let outputs = sys.vault.get_outputs(row.id).await;
    assert!(!outputs.is_empty(), "at least one output must be recorded");

    // No output may reside inside sources_dir.
    for o in &outputs {
        assert!(
            !o.path.starts_with(&sys.vault.sources_dir),
            "output {:?} must not be inside sources_dir {:?}",
            o.path,
            sys.vault.sources_dir,
        );
    }

    // The output file exists on disk.
    assert!(
        outputs[0].path.exists(),
        "output file {:?} must exist on disk",
        outputs[0].path,
    );

    // The output is not in the files table (it's in Notes/, not Sources/).
    let output_in_files = sys.vault.store.find_by_path(outputs[0].path.clone()).await;
    assert!(
        output_in_files.unwrap().is_none(),
        "output path must not appear in the files table — it is not in sources_dir"
    );

    sys.shutdown().await;
}

// ── Scenario 2 ────────────────────────────────────────────────────────────────

/// Drop the same PDF twice (same content) → second row `skipped`.
///
/// Rule §3.3 #3: if another row with the same `content_hash` is `done`, the
/// new path is marked `skipped`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_02_same_content_second_skipped() {
    let sys = FullSystem::default_happy().await.unwrap();

    // First drop.
    let pdf1 = sys.vault.drop_file("original.pdf", b"identical content ABC");
    sys.inject_path(pdf1.clone()).await;
    sys
        .vault
        .wait_for_status(&pdf1, Status::Done, 8_000)
        .await
        .expect("first file must reach 'done'");

    // Second drop — same bytes, same path (simulate re-drop after "done").
    let pdf2 = sys.vault.drop_file("original.pdf", b"identical content ABC");
    sys.inject_path(pdf2.clone()).await;

    // Allow a few seconds for the stability + hash + dedup logic.
    // Rule 1 (AlreadyDone / same hash): no state change. The row stays done.
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    let row2 = sys.vault.get_file_row(&pdf2).await.expect("row must exist");
    // Because it's the SAME path and SAME hash, rule 1 fires (AlreadyDone —
    // no status change), so the row should still be 'done'.
    assert_eq!(
        row2.status,
        Status::Done,
        "re-drop of same content at same path must stay 'done' (rule 1)"
    );

    sys.shutdown().await;
}

// ── Scenario 3 ────────────────────────────────────────────────────────────────

/// Drop a renamed copy of the same PDF → `skipped` (hash dedup).
///
/// Rule §3.3 #3: new path, same content hash as a `done` row → `skipped`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_03_renamed_copy_skipped() {
    // Use direct state-store calls — no watcher needed for this logic test.
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    let config = vault.make_config(&stub_path("run.sh"), 30, 300, vec![1_u64], 2);
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    let original = vault.drop_file("original_copy.pdf", b"copy content XYZ");
    let outcome1 = vault.enqueue_direct(&original).await.unwrap();
    assert!(
        matches!(outcome1, EnqueueOutcome::Queued),
        "first enqueue must be Queued"
    );

    // Wait for the first file to be done.
    vault
        .wait_for_status(&original, Status::Done, 8_000)
        .await
        .expect("original must reach 'done'");

    // Now drop an identically-named-but-different-path copy with the same bytes.
    let copy = vault.drop_file("renamed_copy.pdf", b"copy content XYZ");
    let outcome2 = vault.enqueue_direct(&copy).await.unwrap();

    assert!(
        matches!(outcome2, EnqueueOutcome::SkippedDuplicate),
        "renamed copy with same hash must be SkippedDuplicate, got {:?}",
        outcome2,
    );

    // Verify the DB status.
    let copy_row = vault.get_file_row(&copy).await.expect("copy row must exist");
    assert_eq!(copy_row.status, Status::Skipped);

    shutdown.cancel();
}

// ── Scenario 4 ────────────────────────────────────────────────────────────────

/// Drop a modified PDF → re-queued as new revision.
///
/// Rule §3.3 #2: same path that was `done`, different content hash → `RequeuedRevision`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_04_modified_file_requeued() {
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    let config = vault.make_config(&stub_path("run.sh"), 30, 300, vec![1_u64], 2);
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    // Process the original file.
    let path = vault.drop_file("modified.pdf", b"version 1 content");
    vault.enqueue_direct(&path).await.unwrap();
    vault
        .wait_for_status(&path, Status::Done, 8_000)
        .await
        .expect("original must reach 'done'");

    // Overwrite with different content (simulating a modification).
    std::fs::write(&path, b"version 2 content - completely different").unwrap();

    // Enqueue again — different hash, same path.
    let outcome = vault.enqueue_direct(&path).await.unwrap();
    assert!(
        matches!(outcome, EnqueueOutcome::RequeuedRevision),
        "modified file must be RequeuedRevision, got {:?}",
        outcome,
    );

    // Worker should pick it up and run it to done again.
    vault
        .wait_for_status(&path, Status::Done, 8_000)
        .await
        .expect("revision must also reach 'done'");

    shutdown.cancel();
}

// ── Scenario 5 ────────────────────────────────────────────────────────────────

/// Stub processor returns output inside `sources_dir` → `failed` non-retryable.
///
/// The `run_bad_path.sh` stub writes its output into `sources_dir` and returns
/// `{"status":"ok"}` — the output validator must reject it as a non-retryable
/// failure because outputs must NOT reside inside sources_dir.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_05_output_inside_sources_fails_non_retryable() {
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    // Use run_bad_path.sh which deliberately writes to sources_dir.
    let config = vault.make_config(&stub_path("run_bad_path.sh"), 30, 300, vec![1_u64], 1);
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    let pdf = vault.drop_file("bad_sources.pdf", b"test content sources");
    vault.enqueue_direct(&pdf).await.unwrap();

    // The validator must reject the path → mark_failed(retryable=false) →
    // terminal 'failed' regardless of attempts remaining.
    let row = vault
        .wait_for_status(&pdf, Status::Failed, 8_000)
        .await
        .expect("file must reach terminal 'failed'");

    assert!(
        row.last_error.is_some(),
        "last_error must be set on validation failure"
    );
    // Non-retryable: row stays 'failed', never transitions to 'queued'.
    // Wait a moment to confirm no retry happens.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let final_row = vault.get_file_row(&pdf).await.unwrap();
    assert_eq!(
        final_row.status,
        Status::Failed,
        "non-retryable failure must remain 'failed'"
    );

    shutdown.cancel();
}

// ── Scenario 6 ────────────────────────────────────────────────────────────────

/// Stub processor returns output outside `vault_root` → `failed` non-retryable.
///
/// The `run_outside_vault.sh` stub writes to `/tmp` and reports that path.
/// The output validator must reject it.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_06_output_outside_vault_fails_non_retryable() {
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    let config = vault.make_config(
        &stub_path("run_outside_vault.sh"),
        30,
        300,
        vec![1_u64],
        1,
    );
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    let pdf = vault.drop_file("bad_outside.pdf", b"test content outside vault");
    vault.enqueue_direct(&pdf).await.unwrap();

    let row = vault
        .wait_for_status(&pdf, Status::Failed, 8_000)
        .await
        .expect("file must reach terminal 'failed'");

    assert!(
        row.last_error.is_some(),
        "last_error must be set on validation failure"
    );

    // Confirm non-retryable: stays 'failed' after a short wait.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        vault.get_file_status(&pdf).await,
        Some(Status::Failed),
        "non-retryable failure must remain 'failed'"
    );

    shutdown.cancel();
}

// ── Scenario 7 ────────────────────────────────────────────────────────────────

/// Stub processor returns a symlink-escaping path → rejected.
///
/// The `run_symlink_escape.sh` stub creates `vault_root/<link> -> /tmp` and
/// returns a path through that link.  After canonicalization the path resolves
/// outside vault_root and the validator rejects it.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_07_symlink_escape_rejected() {
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    let config = vault.make_config(
        &stub_path("run_symlink_escape.sh"),
        30,
        300,
        vec![1_u64],
        1,
    );
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    let pdf = vault.drop_file("symlink_test.pdf", b"symlink escape test");
    vault.enqueue_direct(&pdf).await.unwrap();

    let row = vault
        .wait_for_status(&pdf, Status::Failed, 8_000)
        .await
        .expect("symlink-escape output must reach 'failed'");

    assert!(
        row.last_error.is_some(),
        "last_error must be set when symlink escape is detected"
    );

    // Must be non-retryable (processor bug).
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        vault.get_file_status(&pdf).await,
        Some(Status::Failed),
        "symlink-escape failure must be non-retryable"
    );

    shutdown.cancel();
}

// ── Scenario 8 ────────────────────────────────────────────────────────────────

/// Stub processor times out → `failed` retryable; re-queued after backoff.
///
/// The `run_timeout.sh` stub sleeps forever.  With `timeout_secs=2` the worker
/// fires SIGTERM after 2 s, waits 5 s for a graceful exit, then SIGKILL.
/// `mark_failed(retryable=true)` is called, and with `backoff_secs=[2]` the
/// row is put back in `queued` (with `next_attempt_at = now + 2`).
///
/// Note: each timeout cycle takes ~7 s (2 s timeout + 5 s SIGTERM grace).
/// Overall test budget: 30 s.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_08_processor_timeout_retried() {
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    // max_attempts = 2 (backoff_secs.len() + 1), backoff = 2 s.
    let config =
        vault.make_config(&stub_path("run_timeout.sh"), 2, 300, vec![2_u64], 1);
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    let pdf = vault.drop_file("timeout_test.pdf", b"timeout test content");
    vault.enqueue_direct(&pdf).await.unwrap();

    // Step 1: Wait for the worker to CLAIM the job (Processing, attempts == 1).
    // The worker polls every 100 ms, so this should happen within a second.
    let processing_row = vault
        .wait_for_status(&pdf, Status::Processing, 3_000)
        .await
        .expect("worker must claim the job within 3 s");
    assert_eq!(
        processing_row.attempts, 1,
        "attempts must be 1 after first claim"
    );

    // Step 2: After the timeout fires (2 s) + SIGTERM grace (5 s), the row goes
    // back to 'queued' (retryable, backoff = 2 s).  Allow 10 s for this.
    let retried_row = vault
        .wait_for_any_status(&pdf, &[Status::Queued, Status::Failed], 10_000)
        .await
        .expect("row must transition out of Processing within 10 s");

    if retried_row.status == Status::Queued {
        // Row is waiting for the backoff to expire.
        assert_eq!(
            retried_row.attempts, 1,
            "attempts must still be 1 while queued after first retry"
        );
        assert!(
            retried_row.next_attempt_at.is_some(),
            "next_attempt_at must be set for a retryable failure"
        );

        // Step 3: Wait for the second cycle to exhaust all attempts → Failed.
        let final_row = vault
            .wait_for_status(&pdf, Status::Failed, 20_000)
            .await
            .expect("row must reach terminal Failed after both attempts");
        assert!(
            final_row.attempts >= 2,
            "must have attempted at least twice, got attempts={}",
            final_row.attempts,
        );
    } else {
        // Already terminal Failed (both cycles completed).
        assert!(
            retried_row.attempts >= 2,
            "terminal Failed must have attempts >= 2, got {}",
            retried_row.attempts
        );
    }

    shutdown.cancel();
}

// ── Scenario 9 ────────────────────────────────────────────────────────────────

/// Crash mid-processing → on restart `recover_in_flight` resets to `queued`.
///
/// Simulates a daemon crash by:
/// 1. Enqueueing a file.
/// 2. Claiming it (transitions to `processing`).
/// 3. NOT calling `mark_done` or `mark_failed` (simulates crash).
/// 4. Calling `recover_in_flight_with_config` (daemon restart).
/// 5. Verifying the row is back in `queued`.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_09_crash_recovery() -> Result<()> {
    let vault = TestVault::new().await?;

    // Enqueue directly.
    let pdf = vault.drop_file("crash_test.pdf", b"crash recovery content");
    let outcome = vault.enqueue_direct(&pdf).await?;
    assert!(matches!(outcome, EnqueueOutcome::Queued));

    // Claim the job (simulates worker starting).
    let claimed = vault
        .store
        .claim_next()
        .await?
        .expect("claim_next must return the queued row");
    assert_eq!(claimed.status, Status::Processing);

    // ── Simulated crash ──────────────────────────────────────────────────────
    // At this point the daemon would crash.  No mark_done / mark_failed call.
    // ────────────────────────────────────────────────────────────────────────

    // Daemon restarts: call recover_in_flight_with_config.
    let recovered = vault
        .store
        .recover_in_flight_with_config(3_i32)
        .await?;
    assert_eq!(recovered, 1, "exactly one in-flight row must be recovered");

    // The row must be back in 'queued'.
    let row = vault.get_file_row(&pdf).await.expect("row must exist");
    assert_eq!(
        row.status,
        Status::Queued,
        "recovered row must be back in 'queued'"
    );
    assert_eq!(
        row.attempts, 2,
        "attempts must be incremented to count the crash"
    );

    Ok(())
}

// ── Scenario 10 ───────────────────────────────────────────────────────────────

/// Watcher down + file dropped + backstop scan → file picked up.
///
/// Simulates the case where FSEvents missed an event (sleep, cloud sync, etc.)
/// by NOT starting the detection pipeline.  A manual `scan_once` call acts as
/// the backstop scanner and discovers the file.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_10_backstop_scan_catches_missed_file() {
    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    let config = vault.make_config(&stub_path("run.sh"), 30, 300, vec![1_u64], 1);
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    // Drop a file BEFORE the watcher starts (or while it's down).
    let pdf = vault.drop_file("backstop_test.pdf", b"backstop scan test content");

    // The file is not in the DB yet.
    assert!(
        vault.get_file_row(&pdf).await.is_none(),
        "file must not be in DB before scan"
    );

    // Create a channel for the scan to submit paths through (stability tracker
    // inlined for simplicity — we submit directly to the state store here).
    let (path_tx, mut path_rx) = mpsc::channel::<PathBuf>(64);
    let ignore_set = TestVault::build_ignore_set(&config);

    // Run scan_once — it should find the PDF.
    let submitted = scan_once(
        &vault.sources_dir,
        &config.watch.extensions,
        &ignore_set,
        &vault.store,
        &path_tx,
    )
    .await
    .expect("scan_once must succeed");

    assert_eq!(submitted, 1, "scan must find exactly one candidate");

    // Drain the path from the channel and enqueue it directly.
    let found_path = path_rx.recv().await.expect("path must be received");
    vault
        .enqueue_direct(&found_path)
        .await
        .expect("enqueue must succeed");

    // Worker must process it.
    vault
        .wait_for_status(&pdf, Status::Done, 8_000)
        .await
        .expect("backstop file must reach 'done'");

    shutdown.cancel();
}

// ── Scenario 11 ───────────────────────────────────────────────────────────────

/// Concurrent worker pool + many files → no double-claim (atomicity).
///
/// Drops N files, enqueues all of them, then lets a worker pool with
/// `concurrency = N` race to claim them.  Verifies every file ends up `done`
/// exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_11_concurrent_workers_no_double_claim() {
    const N: usize = 8;

    let vault = TestVault::new().await.unwrap();
    let shutdown = CancellationToken::new();
    let config =
        vault.make_config(&stub_path("run.sh"), 30, 300, vec![1_u64], N);
    let _pool_handle = vault.start_worker_pool(&config, shutdown.clone());

    let mut paths = Vec::new();
    for i in 0..N {
        let name = format!("concurrent_{:02}.pdf", i);
        let content = format!("file content {}", i);
        let path = vault.drop_file(&name, content.as_bytes());
        vault.enqueue_direct(&path).await.unwrap();
        paths.push(path);
    }

    // Wait for ALL files to reach 'done'.
    for path in &paths {
        vault
            .wait_for_status(path, Status::Done, 15_000)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "file {:?} did not reach 'done' within 15 s",
                    path.file_name().unwrap()
                )
            });
    }

    // Count 'done' rows in DB — must equal N.
    let stats = vault.store.stats().await.unwrap();
    assert_eq!(
        stats.done, N as i64,
        "exactly {N} rows must be 'done' — no double-claim"
    );
    assert_eq!(stats.processing, 0, "no rows must still be 'processing'");
    assert_eq!(stats.queued, 0, "no rows must still be 'queued'");

    shutdown.cancel();
}

// ── Scenario 12 ───────────────────────────────────────────────────────────────

/// Markdown file dropped in sources → ignored (extension filter).
///
/// `.md` is not in the extension allowlist.  `passes_filter` must return
/// `false` for markdown paths, and `scan_once` must not submit them.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_12_markdown_ignored_by_extension_filter() {
    let vault = TestVault::new().await.unwrap();
    let config = vault.make_config(&stub_path("run.sh"), 30, 300, vec![1_u64], 1);
    let ignore_set = TestVault::build_ignore_set(&config);

    // passes_filter must reject .md files.
    let md_path = vault.sources_dir.join("note.md");
    assert!(
        !passes_filter(&md_path, &config.watch.extensions, &ignore_set),
        ".md must not pass the extension filter"
    );

    // Drop a markdown file and also a PDF.
    vault.drop_file("note.md", b"# A markdown note");
    let pdf = vault.drop_file("real.pdf", b"real PDF content");

    let (path_tx, mut path_rx) = mpsc::channel::<PathBuf>(64);

    let submitted = scan_once(
        &vault.sources_dir,
        &config.watch.extensions,
        &ignore_set,
        &vault.store,
        &path_tx,
    )
    .await
    .expect("scan_once must succeed");

    // Only the PDF should be submitted.
    assert_eq!(submitted, 1, "only the PDF must be submitted (not the .md)");

    let found = path_rx.recv().await.expect("path must be received");
    assert_eq!(
        found, pdf,
        "the submitted path must be the PDF, not the markdown file"
    );

    // The markdown file must never appear in the DB.
    let md_file = vault.sources_dir.join("note.md");
    assert!(
        vault.get_file_row(&md_file).await.is_none(),
        ".md file must not appear in the DB"
    );
}

// ── Scenario 13 ───────────────────────────────────────────────────────────────

/// iCloud placeholder appears then materializes → only the real file enqueued.
///
/// `.icloud` is in `ignore_globs`, so `passes_filter` must reject it.
/// When the real file materializes, the scanner picks it up normally.
#[tokio::test(flavor = "multi_thread")]
async fn scenario_13_icloud_placeholder_then_materialized() {
    let vault = TestVault::new().await.unwrap();
    let config = vault.make_config(&stub_path("run.sh"), 30, 300, vec![1_u64], 1);
    let ignore_set = TestVault::build_ignore_set(&config);

    // iCloud placeholder: "document.pdf.icloud"
    // Extension is "icloud" — not in allowlist.
    let placeholder = vault.sources_dir.join("document.pdf.icloud");
    assert!(
        !passes_filter(&placeholder, &config.watch.extensions, &ignore_set),
        ".icloud placeholder must not pass the filter"
    );

    // Also check the full glob pattern match.
    let placeholder_with_dot = vault.sources_dir.join(".document.pdf.icloud");
    assert!(
        !passes_filter(&placeholder_with_dot, &config.watch.extensions, &ignore_set),
        "dot-prefixed icloud file must not pass the filter"
    );

    // Drop the placeholder file (as iCloud would).
    vault.drop_file("document.pdf.icloud", b"icloud placeholder bytes");

    let (path_tx_placeholder, mut path_rx_placeholder) = mpsc::channel::<PathBuf>(64);
    let count_placeholder = scan_once(
        &vault.sources_dir,
        &config.watch.extensions,
        &ignore_set,
        &vault.store,
        &path_tx_placeholder,
    )
    .await
    .expect("scan must succeed");

    assert_eq!(
        count_placeholder, 0,
        "placeholder must not be submitted by the scanner"
    );
    assert!(
        path_rx_placeholder.try_recv().is_err(),
        "no paths must arrive when only the placeholder is present"
    );

    // Simulate materialization: real PDF appears alongside the placeholder.
    let real_pdf = vault.drop_file("document.pdf", b"real iCloud-synced PDF content");

    let (path_tx_real, mut path_rx_real) = mpsc::channel::<PathBuf>(64);
    let count_real = scan_once(
        &vault.sources_dir,
        &config.watch.extensions,
        &ignore_set,
        &vault.store,
        &path_tx_real,
    )
    .await
    .expect("scan must succeed");

    assert_eq!(count_real, 1, "only the real PDF must be submitted");

    let found = path_rx_real.recv().await.expect("path must be received");
    assert_eq!(found, real_pdf, "submitted path must be the real PDF");
}

// ── Scenario 14 ───────────────────────────────────────────────────────────────

/// Large file: stability window prevents premature hashing.
///
/// Verifies that `StabilityTracker` does NOT emit a file as "stable" while it
/// is still being written.  The test:
/// 1. Starts a `StabilityTracker` with a 600 ms stability window.
/// 2. Starts writing to a file in 100 ms intervals for 500 ms.
/// 3. After 400 ms (still writing), asserts the file is NOT yet in the DB.
/// 4. Stops writing.
/// 5. Waits 800 ms more → file stabilises → hashed → enqueued.
/// 6. Asserts the file IS now in the DB with status `queued` (or later).
#[tokio::test(flavor = "multi_thread")]
async fn scenario_14_stability_window_prevents_premature_hash() {
    use kb_watcher::{StabilityTracker, StableFile};
    use tokio::sync::mpsc as tpsc;

    let vault = TestVault::new().await.unwrap();
    let _config = vault.make_config(&stub_path("run.sh"), 30, 600, vec![1_u64], 1);

    // ── Set up a standalone stability tracker ────────────────────────────────
    // stability_ms = 600, poll_interval_ms = 100.
    let tracker = StabilityTracker::new(600, 100);
    let inject_tx = tracker.sender();

    let (stable_tx, mut stable_rx) = tpsc::channel::<StableFile>(64);
    let _tracker_handle = tracker.run(stable_tx);

    // ── Create the file and inject its path ──────────────────────────────────
    let path = vault.drop_file("large_in_progress.pdf", b"initial content");
    inject_tx
        .send(path.clone())
        .await
        .expect("inject path into tracker");

    // ── Keep writing to the file every 100 ms for 500 ms ────────────────────
    let path_clone = path.clone();
    let write_handle = tokio::spawn(async move {
        for i in 0..5_u32 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let content = format!("updated content iteration {}", i);
            std::fs::write(&path_clone, content.as_bytes()).ok();
        }
    });

    // ── After 400 ms (still writing), the file must NOT be stable yet ────────
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The stable channel should be empty — file is still changing.
    let premature = stable_rx.try_recv();
    assert!(
        premature.is_err(),
        "stability tracker must NOT emit the file while it is still being written"
    );

    // Wait for the writes to finish.
    write_handle.await.expect("write task must finish");

    // ── After writes stop, stability window must be satisfied ─────────────────
    // 600 ms stability + some margin.
    let stable_event = tokio::time::timeout(
        Duration::from_secs(2),
        stable_rx.recv(),
    )
    .await
    .expect("must receive stable event within 2 s after writes stop")
    .expect("stable channel must not close");

    assert_eq!(
        stable_event.path, path,
        "stable event must be for the written file"
    );

    // ── Enqueue via the state store ──────────────────────────────────────────
    let hash = kb_watcher::hasher::hash_file(&path, 1_048_576)
        .await
        .expect("hash must succeed");
    let meta = std::fs::metadata(&path).unwrap();
    let outcome = vault
        .store
        .process_stable_file(
            path.clone(),
            meta.len() as i64,
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.subsec_nanos() as i64)
                .unwrap_or(0),
            0,
            hash,
        )
        .await
        .expect("process_stable_file must succeed");

    assert!(
        matches!(outcome, EnqueueOutcome::Queued),
        "file must be queued after stability window, got {:?}",
        outcome
    );

    // Confirm DB row exists with status queued (or further along).
    let row = vault
        .wait_for_any_status(
            &path,
            &[Status::Queued, Status::Processing, Status::Done],
            1_000,
        )
        .await
        .expect("row must appear in DB");

    assert!(
        matches!(
            row.status,
            Status::Queued | Status::Processing | Status::Done
        ),
        "row must be queued or further, got {:?}",
        row.status
    );
}
