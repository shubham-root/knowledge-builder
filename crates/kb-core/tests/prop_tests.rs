//! Property-based tests for `kb-core` path invariants and state machine.
//!
//! Uses [`proptest`] to verify correctness over a wide input space.
//!
//! ## Path invariants tested
//! - (a) Any path inside `vault_root` and outside `sources_dir` → `validate_output` succeeds.
//! - (b) Any path outside `vault_root` → `validate_output` returns `OutsideVault`.
//! - (c) Any path inside `sources_dir` → `validate_output` returns `InsideSources`.
//! - (d) `is_inside` is reflexive: `is_inside(p, p) == true` for any path.
//! - (e) `is_inside` is transitive: `a ⊆ b ∧ b ⊆ c → a ⊆ c`.
//! - (f) `safe_canonicalize` is idempotent.
//!
//! ## State machine invariants tested
//! - No file can be in two statuses simultaneously.
//! - Status transitions only follow valid paths.
//! - `claim_next` only returns queued files.
//! - `mark_done` only transitions `processing → done`, leaving all other rows unchanged.
//! - `recover_in_flight` only affects `processing` rows.
//!
//! ## Regression directory
//! Failing seeds are stored in `crates/kb-core/tests/regressions/` (alongside
//! this file) so they are always replayed on subsequent runs.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use kb_core::paths::{is_inside, safe_canonicalize, validate_output, OutputError};
use kb_core::state::StateStore;
use kb_core::types::{EnqueueOutcome, FileRow, Status};
use proptest::prelude::*;
use tempfile::TempDir;

// ═══════════════════════════════════════════════════════════════════════════════
// Proptest configuration
// ═══════════════════════════════════════════════════════════════════════════════

/// Configuration shared by the path-invariant test block (64 cases).
///
/// The `failure_persistence` key directs proptest to store any failing seed
/// in `crates/kb-core/tests/regressions/` so that subsequent runs always
/// replay previously-discovered failures.
macro_rules! path_config {
    () => {
        ProptestConfig {
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::WithSource("regressions"),
            )),
            cases: 64,
            ..ProptestConfig::default()
        }
    };
}

/// Configuration for the (more expensive) state-machine test block (32 cases).
macro_rules! sm_config {
    () => {
        ProptestConfig {
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::WithSource("regressions"),
            )),
            cases: 32,
            ..ProptestConfig::default()
        }
    };
}

// ═══════════════════════════════════════════════════════════════════════════════
// Proptest strategies
// ═══════════════════════════════════════════════════════════════════════════════

/// A single valid path component: starts with a lowercase letter followed by
/// up to 11 alphanumeric / underscore characters.
///
/// This keeps generated paths short, unambiguous, and safe on all platforms.
fn path_component() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,11}".prop_map(|s| s)
}

/// A non-empty list of path components used to build a relative sub-path.
fn rel_parts(max_depth: usize) -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(path_component(), 1..=max_depth)
}

/// A deduplicated list of file-name stems, guaranteed to have ≥ 2 members.
///
/// Used to ensure the state-machine tests have genuinely distinct files so
/// the content-hash dedup logic (§3.3 rule 3) does not collapse them.
fn unique_names(min: usize, max: usize) -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec("[a-z][a-z0-9]{3,10}", min..=max)
        .prop_map(|mut v| {
            v.sort();
            v.dedup();
            v
        })
        .prop_filter("need at least min unique names", move |v| v.len() >= min)
}

/// State-machine operation enum.
#[derive(Debug, Clone)]
enum Op {
    /// Enqueue the file at position `i % n_files` in the pre-built file list.
    Enqueue(usize),
    /// Claim the next queued file.
    Claim,
    /// Mark the `(i % in_flight.len())`-th in-flight file as done.
    Done(usize),
    /// Mark the `(i % in_flight.len())`-th in-flight file as failed (non-retryable).
    Fail(usize),
    /// Recover all `processing` rows back to `queued`.
    Recover,
}

fn op_strat() -> BoxedStrategy<Op> {
    prop_oneof![
        (0usize..16).prop_map(Op::Enqueue),
        Just(Op::Claim),
        (0usize..16).prop_map(Op::Done),
        (0usize..16).prop_map(Op::Fail),
        Just(Op::Recover),
    ]
    .boxed()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Filesystem helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Standard vault layout:
/// ```text
/// <tmp>/           ← vault root  (= tmp.path())
///   sources/       ← sources dir
/// ```
fn setup_vault() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("TempDir::new");
    let vault = tmp.path().to_path_buf();
    let sources = vault.join("sources");
    fs::create_dir_all(&sources).expect("create sources/");
    (tmp, vault, sources)
}

/// Nested vault layout — `tmp.path()` is *outside* the vault:
/// ```text
/// <tmp>/           ← outer directory (outside vault)
///   vault/         ← vault root
///     sources/     ← sources dir
/// ```
fn setup_nested_vault() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("TempDir::new");
    let vault = tmp.path().join("vault");
    let sources = vault.join("sources");
    fs::create_dir_all(&sources).expect("create vault/sources/");
    (tmp, vault, sources)
}

/// Create all parent directories and write a dummy file at `base / parts… / name.txt`.
///
/// Returns the full path to the created file.
fn make_file(base: &Path, parts: &[String], name: &str) -> PathBuf {
    let mut dir = base.to_path_buf();
    for part in parts {
        dir = dir.join(part);
    }
    fs::create_dir_all(&dir).expect("create_dir_all for make_file");
    let file = dir.join(format!("{name}.txt"));
    fs::write(&file, format!("prop-test content: {name}").as_bytes())
        .expect("write dummy file");
    file
}

// ═══════════════════════════════════════════════════════════════════════════════
// Async / StateStore helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Build a single-threaded Tokio runtime suitable for `block_on` calls inside
/// synchronous proptest closures.
///
/// `current_thread` is used so that tokio does not spin up extra threads per
/// proptest iteration.  The dedicated `kb-state-actor` OS thread spawned
/// inside `StateStore::new` is unaffected by the executor choice.
fn make_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Open a fresh `StateStore` backed by `db_path`.
async fn open_store(db_path: &Path) -> StateStore {
    StateStore::new(db_path, &[30u64, 300u64])
        .await
        .expect("StateStore::new")
}

/// Return every file row, regardless of status.
async fn all_files(store: &StateStore) -> Vec<FileRow> {
    store
        .list_files(None, 100_000, 0)
        .await
        .expect("list_files(None)")
}

/// Return all file rows with a specific status.
async fn files_with(store: &StateStore, s: Status) -> Vec<FileRow> {
    store
        .list_files(Some(s), 100_000, 0)
        .await
        .expect("list_files(status)")
}

/// Return the set of IDs that have a given status.
async fn id_set(store: &StateStore, s: Status) -> HashSet<i64> {
    files_with(store, s).await.into_iter().map(|r| r.id).collect()
}

// ═══════════════════════════════════════════════════════════════════════════════
// PATH INVARIANT PROPERTY TESTS
// ═══════════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(path_config!())]

    // ─────────────────────────────────────────────────────────────────────────
    // (a) Any path inside vault_root and outside sources_dir → validate_output
    //     returns Ok.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_valid_output_inside_vault_outside_sources(
        dir_parts in rel_parts(3),
        filename  in "[a-z][a-z0-9_]{1,10}",
    ) {
        let (_g, vault, sources) = setup_vault();
        // "notes/" sits inside vault but is never sources/ — always valid.
        let file = make_file(&vault.join("notes"), &dir_parts, &filename);

        let result = validate_output(&file, &vault, &sources);
        prop_assert!(
            result.is_ok(),
            "expected Ok for valid path; got: {:?}", result
        );
        prop_assert!(
            result.unwrap().is_absolute(),
            "returned canonical path must be absolute"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (b) Any path outside vault_root → validate_output returns OutsideVault.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_outside_vault_returns_outside_vault(
        filename in "[a-z][a-z0-9_]{1,10}",
    ) {
        // tmp.path() is the outer directory; vault lives at tmp/vault/.
        let (tmp, vault, sources) = setup_nested_vault();
        // Write the file directly into the outer dir (outside vault).
        let file = make_file(tmp.path(), &[], &filename);

        let result = validate_output(&file, &vault, &sources);
        prop_assert!(
            matches!(result, Err(OutputError::OutsideVault { .. })),
            "expected OutsideVault; got: {:?}", result
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (c) Any path inside sources_dir → validate_output returns InsideSources.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_inside_sources_returns_inside_sources(
        dir_parts in rel_parts(2),
        filename  in "[a-z][a-z0-9_]{1,10}",
    ) {
        let (_g, vault, sources) = setup_vault();
        // Write the file inside sources/ — forbidden output location.
        let file = make_file(&sources, &dir_parts, &filename);

        let result = validate_output(&file, &vault, &sources);
        prop_assert!(
            matches!(result, Err(OutputError::InsideSources { .. })),
            "expected InsideSources; got: {:?}", result
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (d) is_inside is reflexive: is_inside(p, p) == true for any path.
    //
    // No filesystem access is needed — is_inside operates purely on
    // PathBuf component comparison.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_is_inside_reflexive(parts in rel_parts(5)) {
        let p: PathBuf = parts
            .iter()
            .fold(PathBuf::from("/"), |acc, c| acc.join(c));

        prop_assert!(
            is_inside(&p, &p),
            "is_inside must be reflexive; failed for {:?}", p
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (e) is_inside is transitive:
    //     is_inside(a, b) ∧ is_inside(b, c) → is_inside(a, c).
    //
    // We construct a ⊆ b ⊆ c by concatenating path segments, so the
    // preconditions hold by construction.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_is_inside_transitive(
        c_parts in rel_parts(3),
        b_extra in rel_parts(2),
        a_extra in rel_parts(2),
    ) {
        // Build c ⊃ b ⊃ a by appending segments.
        let c: PathBuf = c_parts.iter().fold(PathBuf::from("/"), |acc, x| acc.join(x));
        let b: PathBuf = b_extra.iter().fold(c.clone(),          |acc, x| acc.join(x));
        let a: PathBuf = a_extra.iter().fold(b.clone(),          |acc, x| acc.join(x));

        // Preconditions guaranteed by construction — document intent.
        prop_assume!(is_inside(&a, &b));
        prop_assume!(is_inside(&b, &c));

        prop_assert!(
            is_inside(&a, &c),
            "transitivity violated: {:?} ⊆ {:?} ⊆ {:?} but is_inside({:?}, {:?}) = false",
            a, b, c, a, c
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (f) safe_canonicalize is idempotent:
    //     canonicalize(canonicalize(p)) == canonicalize(p).
    //
    // A canonical path is already fully resolved, so a second pass must
    // return the same path.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_canonicalize_is_idempotent(
        parts    in rel_parts(3),
        filename in "[a-z][a-z0-9_]{1,10}",
    ) {
        let tmp  = TempDir::new().expect("TempDir::new");
        let file = make_file(tmp.path(), &parts, &filename);

        let first  = safe_canonicalize(&file)
            .expect("first canonicalize must succeed for a freshly-created file");
        let second = safe_canonicalize(&first)
            .expect("second canonicalize must succeed on an already-canonical path");

        prop_assert_eq!(
            &first, &second,
            "canonicalize must be idempotent; first={:?} second={:?}", first, second
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// STATE MACHINE PROPERTY TESTS
// ═══════════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(sm_config!())]

    // ─────────────────────────────────────────────────────────────────────────
    // (a/b) No file in two statuses simultaneously; all transitions follow
    //       valid paths.
    //
    // Strategy: generate a random sequence of state operations (enqueue,
    // claim, done, fail, recover), execute them against a real StateStore,
    // then verify that:
    //   1. Every file path appears in exactly one status bucket.
    //   2. Every file ID appears in exactly one status bucket.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_no_file_in_two_statuses(
        n_files in 2usize..=6,
        ops     in proptest::collection::vec(op_strat(), 4..=16),
    ) {
        let rt        = make_rt();
        let tmp_db    = TempDir::new().expect("TempDir db");
        let tmp_files = TempDir::new().expect("TempDir files");
        let db_path   = tmp_db.path().join("state.db");

        // Pre-create source files on disk.
        let paths: Vec<PathBuf> = (0..n_files)
            .map(|i| {
                let p = tmp_files.path().join(format!("f{i:03}.txt"));
                fs::write(&p, format!("content {i}").as_bytes())
                    .expect("write source file");
                p
            })
            .collect();

        rt.block_on(async {
            let store = open_store(&db_path).await;
            // in_flight tracks IDs that are currently in `processing` status.
            let mut in_flight: Vec<i64> = Vec::new();

            for op in &ops {
                match op {
                    Op::Enqueue(idx) => {
                        let i    = idx % n_files;
                        let hash = format!("sha256:{i:064x}");
                        // Ignore outcome: duplicates are handled by the store.
                        let _ = store
                            .process_stable_file(
                                paths[i].clone(), 100, 0, 0u64, hash,
                            )
                            .await
                            .expect("process_stable_file");
                    }
                    Op::Claim => {
                        if let Some(row) = store
                            .claim_next()
                            .await
                            .expect("claim_next")
                        {
                            in_flight.push(row.id);
                        }
                    }
                    Op::Done(idx) => {
                        if !in_flight.is_empty() {
                            let i  = idx % in_flight.len();
                            let id = in_flight.remove(i);
                            store
                                .mark_done(id, vec![], None)
                                .await
                                .expect("mark_done");
                        }
                    }
                    Op::Fail(idx) => {
                        if !in_flight.is_empty() {
                            let i  = idx % in_flight.len();
                            let id = in_flight.remove(i);
                            store
                                .mark_failed(
                                    id,
                                    "prop-test-fail".to_owned(),
                                    false,
                                )
                                .await
                                .expect("mark_failed");
                        }
                    }
                    Op::Recover => {
                        store
                            .recover_in_flight()
                            .await
                            .expect("recover_in_flight");
                        // Recovered rows return to queued; local tracking reset.
                        in_flight.clear();
                    }
                }
            }

            // ── Invariant 1: every path appears at most once ────────────────
            let all = all_files(&store).await;
            let mut seen_paths: HashSet<PathBuf> = HashSet::new();
            for row in &all {
                assert!(
                    seen_paths.insert(row.path.clone()),
                    "path {:?} appeared twice — unique-path invariant violated",
                    row.path
                );
            }

            // ── Invariant 2: every ID appears in exactly one status bucket ──
            let statuses = [
                Status::Seen,
                Status::Queued,
                Status::Processing,
                Status::Done,
                Status::Failed,
                Status::Skipped,
            ];
            let mut id_to_status: HashMap<i64, Status> = HashMap::new();
            for status in statuses {
                // Clone for the query; keep `status` for the HashMap insert.
                let rows = files_with(&store, status.clone()).await;
                for row in rows {
                    if let Some(prev) = id_to_status.insert(row.id, status.clone()) {
                        panic!(
                            "file {} appeared in both {:?} and {:?} — \
                             single-status invariant violated",
                            row.id, prev, status
                        );
                    }
                }
            }
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (c/d) claim_next only returns queued files.
    //
    // Before each claim we snapshot the queued ID set; the returned row must
    // be a member of that snapshot.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_claim_next_only_returns_queued_files(
        names in unique_names(2, 6),
    ) {
        let rt        = make_rt();
        let tmp_db    = TempDir::new().expect("TempDir db");
        let tmp_files = TempDir::new().expect("TempDir files");
        let db_path   = tmp_db.path().join("state.db");

        rt.block_on(async {
            let store = open_store(&db_path).await;

            // Enqueue each file with a unique hash so rule 3 does not suppress any.
            for (i, name) in names.iter().enumerate() {
                let path = tmp_files.path().join(format!("{name}.txt"));
                fs::write(&path, name.as_bytes()).expect("write");
                let hash = format!("sha256:{i:064x}");
                store
                    .process_stable_file(path, 100, 0, 0u64, hash)
                    .await
                    .expect("process_stable_file");
            }

            // Drain the queue, checking each claim against the pre-claim snapshot.
            loop {
                // Snapshot queued IDs *before* issuing the claim.
                let queued_before: HashSet<i64> = id_set(&store, Status::Queued).await;

                match store.claim_next().await.expect("claim_next") {
                    None => break, // queue exhausted
                    Some(row) => {
                        assert!(
                            queued_before.contains(&row.id),
                            "claim_next returned file {} which was NOT in the \
                             queued snapshot taken immediately before the claim",
                            row.id
                        );
                        // Mark done so the loop terminates.
                        store
                            .mark_done(row.id, vec![], None)
                            .await
                            .expect("mark_done");
                    }
                }
            }
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (e) mark_done only works on processing files.
    //
    // After claiming one file and calling mark_done:
    //   - The claimed file transitions to `done`.
    //   - Every other file's status is unchanged.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_mark_done_only_affects_processing_files(
        n_files in 2usize..=6,
    ) {
        let rt        = make_rt();
        let tmp_db    = TempDir::new().expect("TempDir db");
        let tmp_files = TempDir::new().expect("TempDir files");
        let db_path   = tmp_db.path().join("state.db");

        rt.block_on(async {
            let store = open_store(&db_path).await;

            // Enqueue n_files unique files.
            for i in 0..n_files {
                let path = tmp_files.path().join(format!("md{i}.txt"));
                fs::write(&path, format!("done-test {i}").as_bytes())
                    .expect("write");
                let hash = format!("sha256:{:064x}", (i as u64).wrapping_add(0xdead));
                store
                    .process_stable_file(path, 100, 0, 0u64, hash)
                    .await
                    .expect("process_stable_file");
            }

            // Snapshot state before the claim.
            let queued_before: HashSet<i64> = id_set(&store, Status::Queued).await;

            // Claim exactly one file.
            let claimed = store.claim_next().await.expect("claim_next");
            // If no file was queued (should not happen given enqueue above) skip.
            let claimed_row = match claimed {
                Some(r) => r,
                None    => return,
            };
            let claimed_id = claimed_row.id;

            // The claimed file must be in `processing`.
            let processing_before: HashSet<i64> =
                id_set(&store, Status::Processing).await;
            assert!(
                processing_before.contains(&claimed_id),
                "claimed file {claimed_id} must be in processing before mark_done"
            );

            // mark_done the processing file.
            store
                .mark_done(claimed_id, vec![], None)
                .await
                .expect("mark_done");

            // ── Check: claimed file is now done ───────────────────────────
            let row = store
                .get_file_by_id(claimed_id)
                .await
                .expect("get_file_by_id")
                .expect("row must exist");
            assert_eq!(
                row.status,
                Status::Done,
                "mark_done must transition claimed file to Done"
            );

            // ── Check: claimed file is no longer in processing ────────────
            let processing_after: HashSet<i64> =
                id_set(&store, Status::Processing).await;
            assert!(
                !processing_after.contains(&claimed_id),
                "mark_done must remove file {claimed_id} from processing"
            );

            // ── Check: queued files are unchanged (minus the one claimed) ─
            // queued_before contains all files that were queued before the
            // claim.  After the claim, the claimed ID left queued; after
            // mark_done it is now done — so the remaining queued set should
            // equal queued_before minus claimed_id.
            let expected_queued: HashSet<i64> = queued_before
                .iter()
                .copied()
                .filter(|&id| id != claimed_id)
                .collect();
            let queued_after: HashSet<i64> = id_set(&store, Status::Queued).await;
            assert_eq!(
                queued_after, expected_queued,
                "mark_done must not affect any queued file"
            );
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // (f) recover_in_flight only affects processing files.
    //
    // After enqueuing `n_total` files and claiming `n_to_claim` of them,
    // calling recover_in_flight must:
    //   - Move exactly the `processing` files back to `queued`.
    //   - Leave every file that was already `queued` unchanged.
    //   - Return the count of recovered rows.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_recover_in_flight_only_affects_processing(
        n_total    in 4usize..=8,
        n_to_claim in 1usize..=3,
    ) {
        // Ensure we have more files than we claim, so there are still
        // queued rows to verify unchanged after recovery.
        prop_assume!(n_to_claim < n_total);

        let rt        = make_rt();
        let tmp_db    = TempDir::new().expect("TempDir db");
        let tmp_files = TempDir::new().expect("TempDir files");
        let db_path   = tmp_db.path().join("state.db");

        rt.block_on(async {
            let store = open_store(&db_path).await;

            // Enqueue n_total unique files.
            for i in 0..n_total {
                let path = tmp_files.path().join(format!("rf{i}.txt"));
                fs::write(&path, format!("recover-test {i}").as_bytes())
                    .expect("write");
                let hash = format!("sha256:{:064x}", (i as u64).wrapping_add(0xcafe));
                store
                    .process_stable_file(path, 100, 0, 0u64, hash)
                    .await
                    .expect("process_stable_file");
            }

            // Claim exactly n_to_claim files → they become `processing`.
            let mut processing_ids: HashSet<i64> = HashSet::new();
            for _ in 0..n_to_claim {
                if let Some(row) = store.claim_next().await.expect("claim_next") {
                    processing_ids.insert(row.id);
                }
            }

            // Snapshot queued IDs before recovery.
            let queued_before: HashSet<i64> = id_set(&store, Status::Queued).await;

            // Invoke recovery.
            let recovered = store
                .recover_in_flight()
                .await
                .expect("recover_in_flight");

            // ── Assertion: recovered count matches processing count ────────
            assert_eq!(
                recovered,
                processing_ids.len(),
                "recover_in_flight must return the number of processing rows; \
                 expected {} got {}",
                processing_ids.len(),
                recovered
            );

            // ── Assertion: no processing rows remain ──────────────────────
            let processing_after: Vec<FileRow> =
                files_with(&store, Status::Processing).await;
            assert!(
                processing_after.is_empty(),
                "recover_in_flight must move all processing rows to queued; \
                 {} remain",
                processing_after.len()
            );

            // ── Assertion: recovered rows are now queued ──────────────────
            let queued_after: HashSet<i64> = id_set(&store, Status::Queued).await;
            for &id in &processing_ids {
                assert!(
                    queued_after.contains(&id),
                    "processing file {id} must be queued after recover_in_flight"
                );
            }

            // ── Assertion: originally-queued rows are still queued ────────
            for &id in &queued_before {
                assert!(
                    queued_after.contains(&id),
                    "file {id} was queued before recover_in_flight but is \
                     missing from the queued set afterward"
                );
            }

            // ── Assertion: no row that was NOT processing has changed ──────
            // queued_after must be exactly: queued_before ∪ processing_ids
            let expected_queued: HashSet<i64> = queued_before
                .union(&processing_ids)
                .copied()
                .collect();
            assert_eq!(
                queued_after, expected_queued,
                "recover_in_flight must not affect non-processing rows; \
                 expected queued={:?} got {:?}",
                expected_queued, queued_after
            );
        });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Status transition validity — oracle tracking.
    //
    // Runs a deterministic sequence of claim / done / fail operations driven
    // by proptest-generated indices, maintaining a parallel oracle that tracks
    // the expected status of every file.  After each step the oracle is
    // reconciled against the actual DB state.
    // ─────────────────────────────────────────────────────────────────────────
    #[test]
    fn prop_status_transitions_follow_valid_paths(
        names      in unique_names(2, 5),
        op_indices in proptest::collection::vec(0usize..3, 4..=12),
    ) {
        let rt        = make_rt();
        let tmp_db    = TempDir::new().expect("TempDir db");
        let tmp_files = TempDir::new().expect("TempDir files");
        let db_path   = tmp_db.path().join("state.db");

        rt.block_on(async {
            let store = open_store(&db_path).await;

            // ── Phase 1: enqueue all files, populate oracle ───────────────
            let mut oracle: HashMap<i64, Status> = HashMap::new();
            let mut in_flight: Vec<i64> = Vec::new();

            for (i, name) in names.iter().enumerate() {
                let path = tmp_files.path().join(format!("{name}.txt"));
                fs::write(&path, name.as_bytes()).expect("write");
                let hash = format!("sha256:{i:064x}");

                let outcome = store
                    .process_stable_file(path.clone(), 100, 0, 0u64, hash)
                    .await
                    .expect("process_stable_file");

                if matches!(outcome, EnqueueOutcome::Queued) {
                    if let Some(row) = store
                        .find_by_path(path)
                        .await
                        .expect("find_by_path")
                    {
                        oracle.insert(row.id, Status::Queued);
                    }
                }
            }

            // ── Phase 2: apply op sequence, check oracle each step ────────
            // op_indices cycles through 0=Claim, 1=Done, 2=Fail.
            for &op_idx in &op_indices {
                // Snapshot queued IDs before any claim for assertion (c/d).
                let queued_snapshot: HashSet<i64> =
                    id_set(&store, Status::Queued).await;

                match op_idx {
                    0 /* Claim */ => {
                        if let Some(row) = store
                            .claim_next()
                            .await
                            .expect("claim_next")
                        {
                            // Must have been queued before claim.
                            assert!(
                                queued_snapshot.contains(&row.id),
                                "claim_next returned file {} not in pre-claim \
                                 queued snapshot",
                                row.id
                            );
                            oracle.insert(row.id, Status::Processing);
                            in_flight.push(row.id);
                        }
                    }
                    1 /* Done */ => {
                        if !in_flight.is_empty() {
                            let id = in_flight.remove(0);
                            store
                                .mark_done(id, vec![], None)
                                .await
                                .expect("mark_done");
                            oracle.insert(id, Status::Done);
                        }
                    }
                    2 /* Fail */ => {
                        if !in_flight.is_empty() {
                            let id = in_flight.remove(0);
                            store
                                .mark_failed(id, "transition-test".to_owned(), false)
                                .await
                                .expect("mark_failed");
                            oracle.insert(id, Status::Failed);
                        }
                    }
                    _ => unreachable!(),
                }

                // ── Reconcile oracle with actual DB ───────────────────────
                for (&id, expected) in &oracle {
                    if let Some(row) = store
                        .get_file_by_id(id)
                        .await
                        .expect("get_file_by_id")
                    {
                        assert_eq!(
                            row.status, *expected,
                            "oracle mismatch for file {id}: expected {:?} \
                             but DB has {:?}",
                            expected, row.status
                        );
                    }
                }
            }
        });
    }
}
