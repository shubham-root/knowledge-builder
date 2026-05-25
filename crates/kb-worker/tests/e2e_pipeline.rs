//! End-to-end pipeline test: enqueue → claim → process → validate → done
use std::path::PathBuf;
use kb_core::{StateStore, EnqueueOutcome, Status};
use kb_core::config::ProcessorConfig;
use kb_worker::process_job;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_full_pipeline_happy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let vault_root = tmp.path().join("vault");
    let sources_dir = vault_root.join("Sources");
    let notes_dir = vault_root.join("Notes");
    let work_dir = tmp.path().join("work");
    
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::create_dir_all(&notes_dir).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    
    let source_file = sources_dir.join("hello.pdf");
    std::fs::write(&source_file, "fake PDF content").unwrap();
    
    let stub_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("processors/stub/run.sh");
    assert!(stub_path.exists(), "Stub processor not found at {:?}", stub_path);
    
    let db_path = tmp.path().join("test.db");
    let backoff = vec![30u64, 300, 1800];
    let state = StateStore::new(&db_path, &backoff).await.unwrap();
    
    let outcome = state.process_stable_file(
        source_file.clone(), 16, 1234567890_000_000_000, 999,
        "sha256:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string(),
    ).await.unwrap();
    assert!(matches!(outcome, EnqueueOutcome::Queued));
    
    let job = state.claim_next().await.unwrap().expect("should have a job");
    assert_eq!(job.status, Status::Processing);
    
    let config = ProcessorConfig {
        command: stub_path.to_string_lossy().to_string(),
        timeout_secs: 30,
        work_dir_root: work_dir.to_string_lossy().to_string(),
    };
    
    let result = process_job(job.clone(), state.clone(), &config, &Default::default(), &vault_root, &sources_dir, std::path::Path::new("/tmp"), CancellationToken::new()).await;
    assert!(result.is_ok(), "process_job failed: {:?}", result.err());
    
    // Verify done
    let row = state.get_file_by_id(job.id).await.unwrap().unwrap();
    assert_eq!(row.status, Status::Done, "Expected Done, got {:?}", row.status);
    
    // Verify outputs
    let outputs = state.get_outputs_for_file(job.id).await.unwrap();
    assert!(!outputs.is_empty(), "Should have outputs");
    assert!(outputs[0].path.exists(), "Output file must exist on disk");
    
    // Verify invariant: output inside vault, not inside sources
    let co = std::fs::canonicalize(&outputs[0].path).unwrap();
    let cv = std::fs::canonicalize(&vault_root).unwrap();
    let cs = std::fs::canonicalize(&sources_dir).unwrap();
    assert!(co.starts_with(&cv), "Output must be inside vault");
    assert!(!co.starts_with(&cs), "Output must NOT be inside sources");
    
    println!("✅ Happy path: queued→processing→done, output validated, invariant holds");
}

#[tokio::test]
async fn test_bad_path_processor_fails_non_retryable() {
    let tmp = tempfile::tempdir().unwrap();
    let vault_root = tmp.path().join("vault");
    let sources_dir = vault_root.join("Sources");
    let work_dir = tmp.path().join("work");
    
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    
    let source_file = sources_dir.join("evil.pdf");
    std::fs::write(&source_file, "content").unwrap();
    
    let stub_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("processors/stub/run_bad_path.sh");
    assert!(stub_path.exists(), "Bad path stub not found at {:?}", stub_path);
    
    let db_path = tmp.path().join("test.db");
    let backoff = vec![30u64, 300, 1800];
    let state = StateStore::new(&db_path, &backoff).await.unwrap();
    
    let outcome = state.process_stable_file(
        source_file.clone(), 7, 1234567890_000_000_000, 888,
        "sha256:deadbeef00000000000000000000000000000000000000000000000000000000".to_string(),
    ).await.unwrap();
    assert!(matches!(outcome, EnqueueOutcome::Queued));
    
    let job = state.claim_next().await.unwrap().expect("should have a job");
    
    let config = ProcessorConfig {
        command: stub_path.to_string_lossy().to_string(),
        timeout_secs: 30,
        work_dir_root: work_dir.to_string_lossy().to_string(),
    };
    
    let result = process_job(job.clone(), state.clone(), &config, &Default::default(), &vault_root, &sources_dir, std::path::Path::new("/tmp"), CancellationToken::new()).await;
    assert!(result.is_ok());
    
    // File should be FAILED (non-retryable) because output was inside sources_dir
    let row = state.get_file_by_id(job.id).await.unwrap().unwrap();
    assert_eq!(row.status, Status::Failed, "Expected Failed due to invariant violation, got {:?}", row.status);
    assert!(row.last_error.is_some());
    println!("✅ Bad path: validator caught invariant violation, marked as terminal failure");
    println!("   Error: {}", row.last_error.unwrap());
}

#[tokio::test]
async fn test_error_processor_retries_with_backoff() {
    let tmp = tempfile::tempdir().unwrap();
    let vault_root = tmp.path().join("vault");
    let sources_dir = vault_root.join("Sources");
    let work_dir = tmp.path().join("work");
    
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    
    let source_file = sources_dir.join("retry.pdf");
    std::fs::write(&source_file, "content").unwrap();
    
    let stub_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("processors/stub/run_error.sh");
    
    let db_path = tmp.path().join("test.db");
    let backoff = vec![30u64, 300, 1800];
    let state = StateStore::new(&db_path, &backoff).await.unwrap();
    
    let outcome = state.process_stable_file(
        source_file.clone(), 7, 1234567890_000_000_000, 777,
        "sha256:cafebabe00000000000000000000000000000000000000000000000000000000".to_string(),
    ).await.unwrap();
    assert!(matches!(outcome, EnqueueOutcome::Queued));
    
    let job = state.claim_next().await.unwrap().expect("should have a job");
    
    let config = ProcessorConfig {
        command: stub_path.to_string_lossy().to_string(),
        timeout_secs: 30,
        work_dir_root: work_dir.to_string_lossy().to_string(),
    };
    
    let result = process_job(job.clone(), state.clone(), &config, &Default::default(), &vault_root, &sources_dir, std::path::Path::new("/tmp"), CancellationToken::new()).await;
    assert!(result.is_ok());
    
    // Retryable error + available backoff slot → re-queued with future next_attempt_at
    let row = state.get_file_by_id(job.id).await.unwrap().unwrap();
    assert_eq!(row.status, Status::Queued, "Expected re-queued, got {:?}", row.status);
    assert!(row.next_attempt_at.is_some(), "Should have backoff next_attempt_at");
    println!("✅ Error processor: retryable error → re-queued with backoff");
    println!("   next_attempt_at: {}", row.next_attempt_at.unwrap());
}

#[tokio::test]
async fn test_dedup_prevents_reprocessing() {
    let tmp = tempfile::tempdir().unwrap();
    let vault_root = tmp.path().join("vault");
    let sources_dir = vault_root.join("Sources");
    
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::create_dir_all(vault_root.join("Notes")).unwrap();
    
    let file_a = sources_dir.join("a.pdf");
    let file_b = sources_dir.join("b.pdf");
    std::fs::write(&file_a, "same content").unwrap();
    std::fs::write(&file_b, "same content").unwrap();
    
    let db_path = tmp.path().join("test.db");
    let backoff = vec![30u64];
    let state = StateStore::new(&db_path, &backoff).await.unwrap();
    let hash = "sha256:same_hash_0000000000000000000000000000000000000000000000000000".to_string();
    
    let stub_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("processors/stub/run.sh");
    let work_dir = tmp.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    
    // Process first file to done
    let outcome1 = state.process_stable_file(file_a.clone(), 12, 100, 1, hash.clone()).await.unwrap();
    assert!(matches!(outcome1, EnqueueOutcome::Queued));
    let job = state.claim_next().await.unwrap().unwrap();
    let config = ProcessorConfig {
        command: stub_path.to_string_lossy().to_string(),
        timeout_secs: 30,
        work_dir_root: work_dir.to_string_lossy().to_string(),
    };
    process_job(job, state.clone(), &config, &Default::default(), &vault_root, &sources_dir, std::path::Path::new("/tmp"), CancellationToken::new()).await.unwrap();
    
    // Second file with same hash → SkippedDuplicate
    let outcome2 = state.process_stable_file(file_b.clone(), 12, 100, 2, hash.clone()).await.unwrap();
    assert!(matches!(outcome2, EnqueueOutcome::SkippedDuplicate),
        "Expected SkippedDuplicate, got {:?}", outcome2);
    
    // Same file again → AlreadyDone
    let outcome3 = state.process_stable_file(file_a.clone(), 12, 100, 1, hash.clone()).await.unwrap();
    assert!(matches!(outcome3, EnqueueOutcome::AlreadyDone),
        "Expected AlreadyDone, got {:?}", outcome3);
    
    println!("✅ Dedup: duplicate hash → SkippedDuplicate; same file+hash → AlreadyDone");
}
