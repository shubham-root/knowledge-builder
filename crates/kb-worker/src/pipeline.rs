//! In-process pipeline replacing the legacy Python `kb_processor`.
//!
//! Where the old daemon spawned a Python subprocess that did
//! `extract → integrate → return JSON on stdout`, this module runs
//! the same flow as a single async function call inside `kb-worker`:
//!
//! 1. **Extract** — call [`kb_extractor::Extractor::extract_with_mode`]
//!    using the precision tier resolved from the operator's
//!    `[extraction]` config.
//! 2. **Stage**   — write `<work_dir>/extracted.md` with provenance
//!    front-matter so the agent's `cat` tool can read it.
//! 3. **Integrate** — call [`kb_agent::run_agent`] which spawns
//!    `pi --mode rpc`, streams events, snapshots the vault, runs the
//!    rogue-write audit, and returns the parsed plan.
//! 4. **Sweep**   — apply [`kb_agent::link_sweeper::sweep_files`] to
//!    every plan-derived path PLUS every newly-created vault file
//!    (the union catches Obsidian auto-disambiguation drift).
//! 5. **Result**  — collect outputs, attach metadata, return the
//!    same [`ProcessResult`] shape `kb-worker::pool::process_job` has
//!    always consumed so the caller is unchanged.
//!
//! There is no subprocess and no IPC.  The kb daemon binary contains
//! all of this code; on import the only out-of-process binary
//! involvement is `pi` itself (a Node CLI we must keep on PATH).

use std::path::{Path, PathBuf};

use kb_agent::driver::{run_agent, AgentError, AgentInput};
use kb_agent::link_sweeper::{files_touched_by_plan, sweep_files};
use kb_core::config::{ExtractionConfig, ExtractionMode};
use kb_core::types::{ProcessOutput, ProcessResult, ProcessorInput};
use kb_extractor::{Extractor, ExtractorConfig};
use serde_json::{json, Value};
use tracing::{info, warn};

/// Run the full extract → integrate → sweep flow for one job.
///
/// Never panics.  All errors are translated to [`ProcessResult::Error`]
/// with a `retryable` flag matching the underlying error's classification
/// so the worker pool can apply the right policy.
pub async fn run_pipeline(
    input:      &ProcessorInput,
    extraction: &ExtractionConfig,
) -> ProcessResult {
    let started = std::time::Instant::now();
    info!(
        target: "kb_worker::pipeline",
        "pipeline start — job_id={} attempt={} path={}",
        input.job_id, input.attempt, input.input_path.display(),
    );

    if let Err(e) = std::fs::create_dir_all(&input.work_dir) {
        return ProcessResult::Error {
            error: format!("could not create work_dir {}: {e}", input.work_dir.display()),
            retryable: true,
            metadata: Some(json!({"step": "init"})),
        };
    }

    // ── 1. Extract ───────────────────────────────────────────────────────
    let mode = extraction.mode_for(&input.input_path, &input.sources_dir);
    let extractor = match Extractor::new(ExtractorConfig {
        default_mode: mode,
        ocr_language: "eng".to_string(),
        render_page_images: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            return ProcessResult::Error {
                error: format!("extractor init failed: {e}"),
                retryable: false,
                metadata: Some(json!({"step": "extract"})),
            };
        }
    };

    info!(
        target: "kb_worker::pipeline",
        "extract: mode={:?} file={}", mode, input.input_path.display(),
    );
    let extraction_result = match extractor
        .extract_with_mode(&input.input_path, &input.work_dir, mode)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let retryable = e.is_retryable();
            return ProcessResult::Error {
                error: format!("extraction failed: {e}"),
                retryable,
                metadata: Some(json!({
                    "step":           "extract",
                    "extractor_kind": e.kind(),
                    "mode":           format!("{mode:?}"),
                })),
            };
        }
    };
    info!(
        target: "kb_worker::pipeline",
        "extracted {} chars, {} image(s) from {}",
        extraction_result.markdown.len(),
        extraction_result.images.len(),
        input.input_path.display(),
    );

    // ── 2. Stage extracted.md ────────────────────────────────────────────
    let extracted_md = input.work_dir.join("extracted.md");
    let body = build_extracted_md_body(&extraction_result, input);
    if let Err(e) = std::fs::write(&extracted_md, &body) {
        return ProcessResult::Error {
            error: format!("stage failed: cannot write extracted.md: {e}"),
            retryable: true,
            metadata: Some(json!({"step": "stage"})),
        };
    }
    info!(
        target: "kb_worker::pipeline",
        "staged extracted.md ({} bytes) at {}",
        body.len(), extracted_md.display(),
    );

    // ── 3. Integrate ─────────────────────────────────────────────────────
    let agent_mode = std::env::var("KB_AGENT_MODE")
        .unwrap_or_else(|_| "apply".into())
        .trim()
        .to_ascii_lowercase();
    let agent_mode = if matches!(agent_mode.as_str(), "shadow" | "apply") {
        agent_mode
    } else {
        warn!(
            target: "kb_worker::pipeline",
            "KB_AGENT_MODE={agent_mode:?} is invalid; defaulting to 'apply'",
        );
        "apply".into()
    };
    let model = std::env::var("KB_LLM_MODEL")
        .unwrap_or_else(|_| "openrouter/moonshotai/kimi-k2.5".into());
    let source_basename = input.input_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("(unknown)")
        .to_string();

    let agent_input = AgentInput {
        extracted_path:  extracted_md.clone(),
        work_dir:        input.work_dir.clone(),
        vault_root:      input.vault_root.clone(),
        sources_dir:     input.sources_dir.clone(),
        agent_root:      input.agent_root.clone(),
        source_basename,
        model,
        job_id:          input.job_id,
        mode:            agent_mode.clone(),
    };

    let agent_result = match run_agent(agent_input).await {
        Ok(r) => r,
        Err(e) => {
            let retryable = e.is_retryable();
            // Map the well-known permanent errors onto ergonomic
            // metadata fields the daemon log + `kb show <id>` consume.
            let mut meta = serde_json::Map::new();
            meta.insert("step".into(),       Value::String("integrate".into()));
            meta.insert("agent_mode".into(), Value::String(agent_mode.clone()));
            meta.insert("agent_error".into(), Value::String(format!("{e}")));
            return ProcessResult::Error {
                error: match &e {
                    AgentError::MissingApiKey =>
                        format!("Agent missing credentials: {e}"),
                    AgentError::PiNotFound { .. } =>
                        format!("pi binary not found: {e}"),
                    AgentError::Budget { .. } =>
                        format!("Agent budget exhausted before producing a plan: {e}"),
                    _ =>
                        format!("Agent error: {e}"),
                },
                retryable,
                metadata: Some(Value::Object(meta)),
            };
        }
    };

    info!(
        target: "kb_worker::pipeline",
        "agent done — turns={} elapsed={:.1}s {} aborted={}",
        agent_result.turns,
        agent_result.elapsed.as_secs_f64(),
        agent_result.plan.summary(),
        agent_result.aborted,
    );

    // ── 3.5 Empty plan guard ─────────────────────────────────────────────
    if agent_result.plan.entries.is_empty() && !agent_result.aborted {
        let final_text = agent_result.final_assistant_text.trim();
        let final_excerpt: String = if final_text.is_empty() {
            "(no final assistant message)".into()
        } else {
            let mut s: String = final_text.chars().take(400).collect();
            if final_text.chars().count() > 400 {
                s.push('\u{2026}');
            }
            s
        };
        let stderr_excerpt = if agent_result.pi_stderr.is_empty() {
            String::new()
        } else {
            let mut s: String = agent_result.pi_stderr.chars().take(400).collect();
            if agent_result.pi_stderr.chars().count() > 400 {
                s.push('\u{2026}');
            }
            format!(" pi stderr: {s}")
        };
        warn!(
            target: "kb_worker::pipeline",
            "Agent produced an empty plan after {} turn(s); marking as \
             retryable failure so the work_dir is preserved.",
            agent_result.turns,
        );
        let mut meta = serde_json::Map::new();
        meta.insert("step".into(),               Value::String("integrate".into()));
        meta.insert("reason".into(),             Value::String("empty_plan".into()));
        meta.insert("agent_mode".into(),         Value::String(agent_mode.clone()));
        meta.insert("agent_turns".into(),        Value::Number(agent_result.turns.into()));
        meta.insert("agent_elapsed_secs".into(), json!(agent_result.elapsed.as_secs_f64()));
        meta.insert("agent_log".into(),          Value::String(agent_result.agent_log.display().to_string()));
        meta.insert("plan_file".into(),          Value::String(agent_result.plan_file.display().to_string()));
        meta.insert("extracted_md".into(),       Value::String(extracted_md.display().to_string()));
        if let Some(p) = agent_result.metadata.get("provider") {
            meta.insert("agent_provider".into(), p.clone());
        }
        if let Some(m) = agent_result.metadata.get("model") {
            meta.insert("agent_model".into(),    m.clone());
        }
        meta.insert("pi_stderr".into(),
            Value::String(agent_result.pi_stderr.chars().take(4000).collect()));
        meta.insert("retain_work_dir".into(),    Value::Bool(true));
        return ProcessResult::Error {
            error: format!(
                "Agent ran for {} turn(s) in {agent_mode} mode but proposed \
                 no mutations.  Final message: {final_excerpt}{stderr_excerpt}",
                agent_result.turns,
            ),
            retryable: true,
            metadata: Some(Value::Object(meta)),
        };
    }

    // ── 4. Link sweep (apply mode only) ──────────────────────────────────
    let mut sweep_meta = serde_json::Map::new();
    if agent_mode == "apply" && !agent_result.plan.entries.is_empty() {
        let mut touched = files_touched_by_plan(&agent_result.plan.entries, &input.vault_root);
        touched.extend(agent_result.created_during_run.iter().cloned());

        if !touched.is_empty() {
            let stats = sweep_files(
                touched.into_iter(),
                &input.vault_root,
                &input.sources_dir,
                &input.agent_root,
            );
            sweep_meta = stats.as_metadata();
            if stats.links_replaced > 0 {
                info!(
                    target: "kb_worker::pipeline",
                    "link_sweep: rewrote {} unresolved link(s) across {} file(s); examples: {:?}",
                    stats.links_replaced, stats.files_modified,
                    &stats.examples[..stats.examples.len().min(5)],
                );
            } else if stats.files_examined == 0 && stats.files_input > 0 {
                warn!(
                    target: "kb_worker::pipeline",
                    "link_sweep: skipped all {} plan path(s) without examining any \
                     (outside_root={}, not_a_file={}, non_markdown={}) — plan/disk drift suspected",
                    stats.files_input,
                    stats.skipped_outside_root,
                    stats.skipped_not_a_file,
                    stats.skipped_non_markdown,
                );
            } else {
                info!(
                    target: "kb_worker::pipeline",
                    "link_sweep: clean ({} file(s) examined, no unresolved wikilinks)",
                    stats.files_examined,
                );
            }
        }
    }

    // ── 5. Build outputs + metadata ──────────────────────────────────────
    let outputs: Vec<ProcessOutput> = build_output_records(&agent_result.plan, &input.vault_root);
    let mut meta = serde_json::Map::new();
    meta.insert("agent_aborted".into(),       Value::Bool(agent_result.aborted));
    meta.insert("agent_elapsed_secs".into(),  json!(agent_result.elapsed.as_secs_f64()));
    meta.insert("agent_log".into(),           Value::String(agent_result.agent_log.display().to_string()));
    meta.insert("agent_mode".into(),          Value::String(agent_mode.clone()));
    if let Some(p) = agent_result.metadata.get("provider") {
        meta.insert("agent_provider".into(),  p.clone());
    }
    if let Some(m) = agent_result.metadata.get("model") {
        meta.insert("agent_model".into(),     m.clone());
    }
    meta.insert("agent_turns".into(),         Value::Number(agent_result.turns.into()));
    meta.insert("agent_rogue_writes".into(),  Value::Number((agent_result.rogue_writes.len() as u64).into()));
    meta.insert("rogue_writes".into(),
        Value::Array(agent_result.rogue_writes.iter()
            .map(|p| Value::String(p.display().to_string()))
            .collect()));
    meta.insert("rogue_writes_count".into(),  Value::Number((agent_result.rogue_writes.len() as u64).into()));
    meta.insert("plan_entry_count".into(),    Value::Number((agent_result.plan.entries.len() as u64).into()));
    meta.insert("plan_file".into(),           Value::String(agent_result.plan_file.display().to_string()));
    meta.insert("plan_summary".into(),        Value::String(agent_result.plan.summary()));
    meta.insert("extracted_md".into(),        Value::String(extracted_md.display().to_string()));
    meta.insert("extracted_chars".into(),     Value::Number((extraction_result.markdown.len() as u64).into()));
    meta.insert("extraction_mode".into(),     Value::String(format!("{:?}", extraction_result.metadata.mode).to_lowercase()));
    meta.insert("extraction_format".into(),   Value::String(extraction_result.metadata.format.clone()));
    meta.insert("page_count".into(),          Value::Number((extraction_result.metadata.page_count as u64).into()));
    meta.insert("source".into(),              Value::String(input.input_path.display().to_string()));
    meta.insert(
        "title".into(),
        Value::String(extraction_result.metadata.title.clone().unwrap_or_else(|| {
            input.input_path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
        })),
    );
    meta.insert("agent_final_text".into(),    Value::String(agent_result.final_assistant_text.clone()));
    meta.insert("pipeline_elapsed_secs".into(), json!(started.elapsed().as_secs_f64()));
    for (k, v) in sweep_meta {
        meta.insert(k, v);
    }

    info!(
        target: "kb_worker::pipeline",
        "pipeline complete — job_id={} mode={agent_mode} plan_entries={} outputs={}",
        input.job_id, agent_result.plan.entries.len(), outputs.len(),
    );

    ProcessResult::Ok {
        outputs,
        metadata: Some(Value::Object(meta)),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_extracted_md_body(
    extraction: &kb_extractor::Extraction,
    input:      &ProcessorInput,
) -> String {
    let mut s = String::with_capacity(extraction.markdown.len() + 512);
    s.push_str("---\n");
    let basename = input.input_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    s.push_str(&format!("source_basename: {:?}\n", basename));
    s.push_str(&format!("file_type: {}\n",
        input.input_path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_uppercase())
            .unwrap_or_else(|| "UNKNOWN".into()),
    ));
    s.push_str(&format!("extraction_mode: {:?}\n", extraction.metadata.mode));
    s.push_str(&format!("job_id: {}\n", input.job_id));
    if extraction.metadata.page_count > 0 {
        s.push_str(&format!("page_count: {}\n", extraction.metadata.page_count));
    }
    if !extraction.images.is_empty() {
        s.push_str(&format!("figure_count: {}\n", extraction.images.len()));
    }
    s.push_str("---\n\n");
    s.push_str(&extraction.markdown);
    if !extraction.images.is_empty() {
        s.push_str("\n\n## Extracted figures (work_dir copies)\n\n");
        for img in &extraction.images {
            s.push_str(&format!("- `{}`\n", img.display()));
        }
    }
    s
}

/// Convert plan entries into the [`ProcessOutput`] records the worker
/// pool's validator expects.  Only `applied=true` write commands with
/// a `path=` argument are included.
fn build_output_records(plan: &kb_agent::plan::Plan, vault_root: &Path) -> Vec<ProcessOutput> {
    let mut out = Vec::new();
    for entry in &plan.entries {
        if !entry.applied || !entry.is_write() {
            continue;
        }
        let raw = match entry.path_arg() {
            Some(s) => s,
            None    => continue,
        };
        let p = Path::new(raw);
        let abs: PathBuf = if p.is_absolute() {
            p.to_path_buf()
        } else {
            vault_root.join(p)
        };
        let bytes = std::fs::metadata(&abs).map(|m| m.len() as i64).unwrap_or(0);
        out.push(ProcessOutput {
            path:  abs,
            kind:  match entry.cmd.as_str() {
                "create"  => "markdown".into(),
                "append"  | "prepend" => "markdown".into(),
                _         => "markdown".into(),
            },
            bytes,
        });
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kb_agent::plan::{Plan, PlanEntry};
    use std::path::PathBuf;

    fn entry(cmd: &str, args: &[&str], applied: bool) -> PlanEntry {
        PlanEntry {
            ts: 0, mode: "apply".into(), cmd: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            applied, exit_code: if applied { Some(0) } else { None },
        }
    }

    #[test]
    fn build_output_records_collects_create_paths_and_skips_property_set() {
        let plan = Plan {
            path: PathBuf::new(),
            entries: vec![
                entry("create",       &["path=KnowledgeBase/Foo.md", "content=x"], true),
                entry("property:set", &["path=KnowledgeBase/Foo.md", "year=2024"], true),
                entry("create",       &["path=KnowledgeBase/Bar.md", "content=y"], false),
            ],
        };
        let out = build_output_records(&plan, &PathBuf::from("/v"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, PathBuf::from("/v/KnowledgeBase/Foo.md"));
        assert_eq!(out[0].kind, "markdown");
    }
}
