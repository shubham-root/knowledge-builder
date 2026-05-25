//! Drives one agent job to completion.
//!
//! Spawns `pi --mode rpc` as a subprocess, sends a single user prompt,
//! streams JSON-line events to an audit log, and returns the parsed
//! plan + diagnostics.  This is the Rust replacement for the legacy
//! Python `processors/default/kb_processor/agent/rpc_driver.py`.
//!
//! # Architecture
//!
//! The Knowledge Builder daemon now embeds the agent driver in-process
//! (no Python subprocess in the middle).  When kb-worker decides to
//! integrate a job, it calls [`run_agent`] directly; this function
//! blocks until the agent finishes, the wall-clock budget runs out,
//! or pi crashes.  All I/O is async (tokio), but the public entry
//! point is `async fn` so callers compose it naturally.
//!
//! # Sandboxing layers (preserved from the Python driver)
//!
//! 1. **Restricted PATH.**  pi's `bash` tool sees only a curated
//!    set of binaries staged in a per-job temp dir, plus `node`/`npm`
//!    inherited from the daemon's PATH (since pi itself is a Node
//!    script).  Everything else (`mkdir`, `cp`, `mv`, …) fails with
//!    `command not found`.  See [`prompt::stage_wrapper_dir`] for the
//!    full allowlist.
//!
//! 2. **Wrapper invariants.**  `kb-obsidian` (a separate Rust binary
//!    in this workspace) validates every write command's path against
//!    `KB_VAULT_ROOT` / `KB_SOURCES_DIR` / `KB_AGENT_ROOT`.  See
//!    [`crate::plan`] for the wire format.
//!
//! 3. **Vault diff audit.**  The driver snapshots the vault before
//!    spawning pi and again after pi exits; any non-planned file
//!    change is a "rogue write" surfaced in [`AgentResult::rogue_writes`].

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use crate::indexer::{audit_vault_diff, newly_created, snapshot_vault, VaultSnapshot};
use crate::plan::{read_plan, Plan, PlanParseError};
use crate::prompt::{build_user_prompt, stage_skills_dir, stage_wrapper_dir};

// ── Constants ────────────────────────────────────────────────────────────────

/// How long to wait for the agent to finish after sending the prompt.
/// Wall-clock; LLM call latency dominates.  Override via env.
const DEFAULT_AGENT_TIMEOUT_SECS: u64 = 600;

const STDERR_DRAIN_LIMIT: usize = 64 * 1024;

/// Provider-specific API keys that pi could otherwise pick up via its
/// credential cascade.  We strip them all so a stray `OPENAI_API_KEY`
/// in the daemon's env can't reroute the agent away from
/// `OPENROUTER_API_KEY` (the only credential we explicitly support).
const PROVIDER_KEY_BLOCKLIST: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "COHERE_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
];

// ── Error type ───────────────────────────────────────────────────────────────

/// Errors returned by [`run_agent`].
///
/// Each variant carries a `retryable` flag implicitly via name; the
/// pipeline reads [`AgentError::is_retryable`] when classifying.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// `OPENROUTER_API_KEY` not present in the environment.
    /// Permanent — operator must add the key.
    #[error(
        "OPENROUTER_API_KEY is not set in the daemon's environment.  \
         Add it to ~/.config/knowledge-builder/secrets.env."
    )]
    MissingApiKey,

    /// pi binary not on PATH (and `KB_PI_BIN` unset / unresolvable).
    /// Permanent — operator must install pi-coding-agent.
    #[error(
        "pi binary {bin:?} not found on PATH.  Install pi-coding-agent \
         or set KB_PI_BIN to the absolute path."
    )]
    PiNotFound {
        /// The name we tried to resolve.
        bin: String,
    },

    /// pi could not be exec'd (FileNotFoundError, permission denied, …).
    /// Permanent.
    #[error("could not exec pi at {bin:?}: {source}")]
    PiSpawn {
        /// Path we tried to exec.
        bin: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// pi emitted output that didn't match the documented JSON-RPC schema.
    /// Treated as transient — usually a transient pi bug.
    #[error("pi protocol error: {0}")]
    Protocol(String),

    /// Agent ran over the wall-clock budget without producing a plan.
    /// Transient.
    #[error("agent timed out after {timeout_secs}s without proposing any mutations")]
    Budget {
        /// Configured timeout in seconds.
        timeout_secs: u64,
    },

    /// The plan file was malformed.  Treated as permanent — indicates
    /// a wrapper bug rather than a transient model glitch.
    #[error("plan file corrupt: {0}")]
    PlanCorrupt(#[from] PlanParseError),

    /// Skills directory could not be staged (e.g. `work_dir` not writable).
    /// Transient.
    #[error("skills staging failed: {0}")]
    SkillsStaging(String),

    /// `KB_LLM_MODEL` is set to a value that is not in `provider/id` form.
    /// Permanent.
    #[error("KB_LLM_MODEL has bad shape: {0}")]
    BadModel(String),

    /// I/O error reading or writing files in the work_dir.
    /// Transient.
    #[error("agent I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl AgentError {
    /// `true` when the daemon should re-queue the job after backoff.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Protocol(_)
            | Self::Budget { .. }
            | Self::SkillsStaging(_)
            | Self::Io(_)
        )
    }
}

// ── Inputs / outputs ─────────────────────────────────────────────────────────

/// Everything [`run_agent`] needs to drive one job.
#[derive(Debug, Clone)]
pub struct AgentInput {
    /// Path to the staged `extracted.md` file the agent will `cat`.
    pub extracted_path: PathBuf,
    /// Per-job working directory.  Skills, wrapper bin dir, plan
    /// file, and audit log live under here.
    pub work_dir: PathBuf,
    /// Vault root (canonical absolute).
    pub vault_root: PathBuf,
    /// Sources dir (canonical absolute).  The agent's read-only zone.
    pub sources_dir: PathBuf,
    /// Agent's mutation sandbox (e.g. `vault_root/KnowledgeBase`).
    pub agent_root: PathBuf,
    /// File name of the source document — included in the prompt for
    /// provenance.
    pub source_basename: String,
    /// litellm-style model id, e.g. `openrouter/moonshotai/kimi-k2.5`.
    pub model: String,
    /// SQLite job id — included in the prompt for traceability.
    pub job_id: i64,
    /// `"apply"` (default) or `"shadow"`.
    pub mode: String,
}

/// Per-job structured output of the driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    /// Parsed plan from the wrapper's JSONL audit file.
    pub plan: Plan,
    /// Path to the JSONL plan file (kept for diagnostics).
    pub plan_file: PathBuf,
    /// Path to the JSONL stream of every pi event (audit log).
    pub agent_log: PathBuf,
    /// Concatenated text of the assistant's *last* text block.  Empty
    /// if pi exited before producing one.
    pub final_assistant_text: String,
    /// Wall-clock wall-clock duration of the agent run.
    pub elapsed: Duration,
    /// Number of `turn_end` events observed.
    pub turns: u32,
    /// `true` if we forcibly aborted on timeout.
    pub aborted: bool,
    /// Files modified outside the plan (rogue writes).
    pub rogue_writes: Vec<PathBuf>,
    /// pi's stderr (capped, tail-preserving) — populated even on
    /// success in case it emitted warnings.  Empty in the happy path.
    pub pi_stderr: String,
    /// Files newly-created during the run (computed from snapshot
    /// diff).  The link sweeper unions this with plan-derived paths.
    pub created_during_run: Vec<PathBuf>,
    /// Diagnostic metadata: provider, model id, mode, etc.
    pub metadata: serde_json::Map<String, Value>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn agent_timeout_secs() -> u64 {
    std::env::var("KB_AGENT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_AGENT_TIMEOUT_SECS)
        .max(60)
}

fn pi_bin() -> Result<PathBuf, AgentError> {
    let raw = std::env::var("KB_PI_BIN").unwrap_or_else(|_| "pi".to_string());
    let candidate = PathBuf::from(&raw);
    if candidate.is_absolute() && candidate.is_file() {
        return Ok(candidate);
    }
    // Search PATH.
    if let Some(p) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&p) {
            let c = dir.join(&raw);
            if c.is_file() {
                return Ok(c);
            }
        }
    }
    Err(AgentError::PiNotFound { bin: raw })
}

fn split_provider_model(model: &str) -> Result<(&str, &str), AgentError> {
    let (provider, rest) = model.split_once('/').ok_or_else(|| {
        AgentError::BadModel(format!(
            "KB_LLM_MODEL must be 'provider/id' format; got {model:?}",
        ))
    })?;
    if provider.is_empty() || rest.is_empty() {
        return Err(AgentError::BadModel(format!(
            "KB_LLM_MODEL has empty provider or id: {model:?}",
        )));
    }
    Ok((provider, rest))
}

fn build_subprocess_env(
    extracted_path: &Path,
    plan_file:      &Path,
    mode:           &str,
    wrapper_dir:    &Path,
    vault_root:     &Path,
    sources_dir:    &Path,
    agent_root:     &Path,
) -> Result<Vec<(String, String)>, AgentError> {
    let mut env: std::collections::BTreeMap<String, String> =
        std::env::vars().collect();

    let or_key = env.get("OPENROUTER_API_KEY")
        .map(|s| s.trim())
        .unwrap_or("");
    if or_key.is_empty() {
        return Err(AgentError::MissingApiKey);
    }

    for k in PROVIDER_KEY_BLOCKLIST {
        env.remove(*k);
    }

    env.insert("KB_PLAN_FILE".into(),    plan_file.to_string_lossy().into());
    env.insert("KB_AGENT_MODE".into(),   mode.into());
    env.insert("KB_EXTRACTED".into(),    extracted_path.to_string_lossy().into());
    env.insert("KB_VAULT_ROOT".into(),   vault_root.to_string_lossy().into());
    env.insert("KB_SOURCES_DIR".into(),  sources_dir.to_string_lossy().into());
    env.insert("KB_AGENT_ROOT".into(),   agent_root.to_string_lossy().into());

    // Resolve the real obsidian binary on the operator's PATH — the
    // wrapper would otherwise look for it on its own restricted PATH
    // and fail with a misleading error.
    if !env.get("KB_OBSIDIAN_BIN").map(|s| !s.trim().is_empty()).unwrap_or(false) {
        if let Some(p) = which_on_path("obsidian") {
            env.insert("KB_OBSIDIAN_BIN".into(), p.to_string_lossy().into());
        } else {
            warn!(
                target: "kb_agent::driver",
                "`obsidian` binary not found on the daemon's PATH; the agent \
                 will be unable to read the vault and will likely produce \
                 empty plans.  Enable the Obsidian CLI in the Obsidian app \
                 (Settings → General → Command line interface), \
                 restart the daemon, and try again."
            );
        }
    }

    // Strict PATH — only the wrapper directory.  The wrapper directory
    // contains kb-obsidian, the curated read-only utilities we
    // allowlist (cat/head/tail/sed/grep/etc.), and node/npm/npx
    // (so pi's #!/usr/bin/env node shebang resolves).  We deliberately
    // drop /usr/bin and /bin so bare-name calls to mkdir, cp, mv, rm
    // fail with command-not-found.  See the legacy Python driver's
    // `_AGENT_PATH_BINARIES` for the rationale.
    env.insert("PATH".into(), wrapper_dir.to_string_lossy().into());

    Ok(env.into_iter().collect())
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let c = dir.join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Drive one agent job to completion.
///
/// Blocks (asynchronously) until the agent finishes, the wall-clock
/// budget runs out, or pi crashes.  Never panics.  Always reaps the pi
/// process before returning, even on error.
pub async fn run_agent(inp: AgentInput) -> Result<AgentResult, AgentError> {
    if !matches!(inp.mode.as_str(), "shadow" | "apply") {
        return Err(AgentError::BadModel(format!(
            "AgentInput.mode must be 'shadow' or 'apply'; got {:?}",
            inp.mode,
        )));
    }

    let started = Instant::now();

    // ── Resolve binaries / paths ─────────────────────────────────────────
    let pi_path    = pi_bin()?;
    tokio::fs::create_dir_all(&inp.work_dir).await?;
    let plan_file  = inp.work_dir.join(".kb-plan.jsonl");
    let agent_log  = inp.work_dir.join(".agent-events.jsonl");
    if plan_file.exists() {
        let _ = tokio::fs::remove_file(&plan_file).await;
    }

    // Pre-snapshot for the rogue-write audit.
    let pre_snapshot: VaultSnapshot = tokio::task::block_in_place(|| {
        snapshot_vault(&inp.vault_root, &inp.sources_dir)
    });
    info!(
        target: "kb_agent::driver",
        "vault pre-snapshot: {} file(s) under {}",
        pre_snapshot.len(), inp.vault_root.display(),
    );

    // ── Stage skill + wrapper dirs ──────────────────────────────────────
    let skills_dir  = stage_skills_dir(&inp.work_dir)
        .map_err(|e| AgentError::SkillsStaging(e.to_string()))?;
    let wrapper_dir = stage_wrapper_dir(&inp.work_dir)
        .map_err(|e| AgentError::SkillsStaging(e.to_string()))?;

    // ── Build env + argv ────────────────────────────────────────────────
    let env_vars = build_subprocess_env(
        &inp.extracted_path,
        &plan_file,
        &inp.mode,
        &wrapper_dir,
        &inp.vault_root,
        &inp.sources_dir,
        &inp.agent_root,
    )?;
    let api_key = env_vars.iter()
        .find(|(k, _)| k == "OPENROUTER_API_KEY")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    let (provider, model_id) = split_provider_model(&inp.model)?;
    let timeout_secs = agent_timeout_secs();

    info!(
        target: "kb_agent::driver",
        "spawning pi: provider={} model={} mode={} timeout={}s",
        provider, model_id, inp.mode, timeout_secs,
    );

    // ── Spawn pi ─────────────────────────────────────────────────────────
    let mut cmd = Command::new(&pi_path);
    cmd.arg("--mode").arg("rpc")
        .arg("--no-session")
        .arg("--no-context-files")
        .arg("--no-extensions")
        .arg("--no-prompt-templates")
        .arg("--tools").arg("bash")
        .arg("--skill").arg(&skills_dir)
        .arg("--provider").arg(provider)
        .arg("--model").arg(model_id)
        .arg("--api-key").arg(&api_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }
    // Own process group so we can kill the whole tree on timeout.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            // setsid: become process group leader.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child: Child = cmd.spawn().map_err(|e| AgentError::PiSpawn {
        bin:    pi_path.clone(),
        source: e,
    })?;
    let child_pid = child.id().unwrap_or(0) as i32;

    // Drain pi's stderr on a background task so transient errors are
    // captured instead of vanishing.
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_task = if let Some(stderr) = child.stderr.take() {
        let buf = Arc::clone(&stderr_buf);
        Some(tokio::spawn(async move {
            let mut r = stderr;
            let mut chunk = [0u8; 4096];
            loop {
                match r.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buf = buf.lock().unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                        // Cap memory; keep the tail.
                        if buf.len() > STDERR_DRAIN_LIMIT {
                            let drop_n = buf.len() - STDERR_DRAIN_LIMIT;
                            buf.drain(..drop_n);
                        }
                    }
                }
            }
        }))
    } else {
        None
    };

    // ── Send the prompt ──────────────────────────────────────────────────
    let prompt = build_user_prompt(&inp);
    let prompt_msg = json!({
        "id":      "kb-prompt-1",
        "type":    "prompt",
        "message": prompt,
    });
    if let Some(stdin) = child.stdin.as_mut() {
        let mut line = serde_json::to_vec(&prompt_msg)
            .expect("JSON-serialisable prompt message");
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
    } else {
        return Err(AgentError::Protocol("pi subprocess stdin is closed".into()));
    }

    // ── Stream events ────────────────────────────────────────────────────
    let stdout = child.stdout.take().ok_or_else(|| {
        AgentError::Protocol("pi subprocess has no stdout pipe".into())
    })?;
    let mut reader = BufReader::new(stdout).lines();

    let mut audit = tokio::fs::File::create(&agent_log).await?;
    let mut turns = 0u32;
    let mut aborted = false;
    let mut final_text_segments: Vec<String> = Vec::new();
    let deadline = started + Duration::from_secs(timeout_secs);

    loop {
        // Compute time left.
        let now = Instant::now();
        if now >= deadline {
            aborted = true;
            warn!(
                target: "kb_agent::driver",
                "agent timeout reached after {} turns; aborting", turns,
            );
            // Send abort.
            let _ = send_command_to_child(&mut child, &json!({"type":"abort"})).await;
            break;
        }
        let remaining = deadline - now;

        let next = tokio::time::timeout(remaining, reader.next_line()).await;
        let line_res = match next {
            Ok(r)  => r,
            Err(_) => {
                aborted = true;
                warn!(
                    target: "kb_agent::driver",
                    "agent wall-clock deadline hit; aborting",
                );
                let _ = send_command_to_child(&mut child, &json!({"type":"abort"})).await;
                break;
            }
        };

        let line = match line_res {
            Ok(Some(l)) => l,
            Ok(None)    => break,        // EOF
            Err(e)      => return Err(AgentError::Io(e)),
        };
        if line.trim().is_empty() {
            continue;
        }

        // Audit log everything.
        audit.write_all(line.as_bytes()).await?;
        audit.write_all(b"\n").await?;

        let evt: Value = match serde_json::from_str(&line) {
            Ok(v)  => v,
            Err(e) => {
                return Err(AgentError::Protocol(format!(
                    "pi emitted non-JSON line: {:?} ({e})",
                    &line[..line.len().min(200)],
                )));
            }
        };

        let etype = evt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match etype {
            "turn_end" => {
                turns += 1;
            }
            "message_update" => {
                if let Some(d) = evt.get("assistantMessageEvent") {
                    let dt = d.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if dt == "text_start" {
                        final_text_segments.clear();
                    } else if dt == "text_delta" {
                        if let Some(delta) = d.get("delta").and_then(|v| v.as_str()) {
                            final_text_segments.push(delta.to_string());
                        }
                    }
                }
            }
            "tool_execution_start" => {
                let null_v = serde_json::Value::Null;
                debug!(
                    target: "kb_agent::driver",
                    "agent tool={:?} args={}",
                    evt.get("toolName"),
                    serde_json::to_string(evt.get("args").unwrap_or(&null_v))
                        .unwrap_or_default(),
                );
            }
            "agent_end" => {
                break;
            }
            "response" => {
                if evt.get("success").and_then(|v| v.as_bool()) == Some(false) {
                    return Err(AgentError::Protocol(format!(
                        "pi reported failure on command {:?}: {}",
                        evt.get("command"),
                        evt.get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    )));
                }
            }
            _ => {}
        }
    }
    audit.flush().await?;
    drop(audit);

    // ── Clean shutdown ───────────────────────────────────────────────────
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }
    let exit_status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(s))  => Some(s),
        Ok(Err(_)) => None,
        Err(_) => {
            // Did not exit cleanly — escalate.
            warn!(
                target: "kb_agent::driver",
                "pi did not exit; killing process group",
            );
            kill_process_group(child_pid);
            tokio::time::timeout(Duration::from_secs(5), child.wait()).await.ok().and_then(|r| r.ok())
        }
    };
    let _ = exit_status;
    if let Some(t) = stderr_task {
        let _ = t.await;
    }
    let pi_stderr_bytes = stderr_buf.lock().unwrap().clone();
    let pi_stderr = String::from_utf8_lossy(&pi_stderr_bytes).trim().to_string();
    if !pi_stderr.is_empty() {
        warn!(
            target: "kb_agent::driver",
            "pi wrote {} byte(s) to stderr: {}",
            pi_stderr.len(),
            &pi_stderr.chars().take(500).collect::<String>().replace('\n', " | "),
        );
    }

    // ── Parse the plan ───────────────────────────────────────────────────
    let plan = read_plan(&plan_file).map_err(AgentError::PlanCorrupt)?;

    // ── Post-snapshot + rogue audit ──────────────────────────────────────
    let post_snapshot: VaultSnapshot = tokio::task::block_in_place(|| {
        snapshot_vault(&inp.vault_root, &inp.sources_dir)
    });
    let rogue_writes = audit_vault_diff(&pre_snapshot, &post_snapshot, &plan, &inp.vault_root);
    if !rogue_writes.is_empty() {
        warn!(
            target: "kb_agent::driver",
            "vault audit: agent BYPASSED kb-obsidian and made {} unsanctioned write(s) outside the plan: {:?}",
            rogue_writes.len(),
            rogue_writes.iter().take(10).map(|p| p.display().to_string()).collect::<Vec<_>>(),
        );
    } else {
        info!(target: "kb_agent::driver", "vault audit: clean (no rogue writes)");
    }
    let created_during_run = newly_created(&pre_snapshot, &post_snapshot);

    let elapsed = started.elapsed();
    info!(
        target: "kb_agent::driver",
        "agent done: turns={} elapsed={:.1}s {} aborted={}",
        turns, elapsed.as_secs_f64(), plan.summary(), aborted,
    );

    if aborted && plan.entries.is_empty() {
        return Err(AgentError::Budget { timeout_secs });
    }

    let mut metadata = serde_json::Map::new();
    metadata.insert("provider".into(),     Value::String(provider.into()));
    metadata.insert("model".into(),        Value::String(model_id.into()));
    metadata.insert("mode".into(),         Value::String(inp.mode.clone()));
    metadata.insert("rogue_writes".into(), Value::Number((rogue_writes.len() as u64).into()));

    Ok(AgentResult {
        plan,
        plan_file,
        agent_log,
        final_assistant_text: final_text_segments.join("").trim().to_string(),
        elapsed,
        turns,
        aborted,
        rogue_writes,
        pi_stderr,
        created_during_run,
        metadata,
    })
}

// ── Process-group kill (Unix) ────────────────────────────────────────────────

#[cfg(unix)]
fn kill_process_group(pid: i32) {
    if pid <= 0 { return; }
    unsafe {
        // Negative pid = process group.
        libc::kill(-pid, libc::SIGTERM);
    }
}
#[cfg(not(unix))]
fn kill_process_group(_pid: i32) {}

async fn send_command_to_child(child: &mut Child, cmd: &Value) -> Result<(), std::io::Error> {
    if let Some(stdin) = child.stdin.as_mut() {
        let mut line = serde_json::to_vec(cmd)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
    }
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that mutate process-wide env vars must serialise.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn split_model_basic() {
        assert_eq!(
            split_provider_model("openrouter/moonshotai/kimi-k2.5").unwrap(),
            ("openrouter", "moonshotai/kimi-k2.5"),
        );
        assert_eq!(
            split_provider_model("anthropic/claude-3-5-sonnet-latest").unwrap(),
            ("anthropic", "claude-3-5-sonnet-latest"),
        );
    }

    #[test]
    fn split_model_rejects_no_slash() {
        assert!(matches!(
            split_provider_model("just-a-model"),
            Err(AgentError::BadModel(_)),
        ));
    }

    #[test]
    fn split_model_rejects_empty_parts() {
        assert!(matches!(split_provider_model("/foo"), Err(AgentError::BadModel(_))));
        assert!(matches!(split_provider_model("foo/"), Err(AgentError::BadModel(_))));
    }

    #[test]
    fn timeout_floor_is_60s() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by ENV_LOCK; nothing else reads the var.
        unsafe { std::env::set_var("KB_AGENT_TIMEOUT_SECS", "10"); }
        assert_eq!(agent_timeout_secs(), 60);
        unsafe { std::env::remove_var("KB_AGENT_TIMEOUT_SECS"); }
    }

    #[test]
    fn timeout_default_when_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by ENV_LOCK.
        unsafe { std::env::remove_var("KB_AGENT_TIMEOUT_SECS"); }
        assert_eq!(agent_timeout_secs(), DEFAULT_AGENT_TIMEOUT_SECS);
    }

    #[test]
    fn error_retryability_buckets() {
        assert!(!AgentError::MissingApiKey.is_retryable());
        assert!(!AgentError::PiNotFound { bin: "pi".into() }.is_retryable());
        assert!(!AgentError::BadModel("x".into()).is_retryable());
        assert!(AgentError::Protocol("x".into()).is_retryable());
        assert!(AgentError::Budget { timeout_secs: 60 }.is_retryable());
        assert!(AgentError::SkillsStaging("x".into()).is_retryable());
    }

    #[test]
    fn build_env_strips_other_provider_keys() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::collections::HashSet;
        // SAFETY: tests in this module run single-threaded
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "secret");
            std::env::set_var("OPENAI_API_KEY",     "should-be-removed");
            std::env::set_var("ANTHROPIC_API_KEY",  "should-be-removed");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let env = build_subprocess_env(
            &tmp.path().join("e.md"),
            &tmp.path().join("p.jsonl"),
            "apply",
            &tmp.path().join("wrap"),
            &tmp.path().join("vault"),
            &tmp.path().join("vault/Sources"),
            &tmp.path().join("vault/KB"),
        ).unwrap();

        let map: HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(map.contains("OPENROUTER_API_KEY"));
        assert!(!map.contains("OPENAI_API_KEY"));
        assert!(!map.contains("ANTHROPIC_API_KEY"));
        // Reserved kb env vars are present.
        assert!(map.contains("KB_PLAN_FILE"));
        assert!(map.contains("KB_AGENT_MODE"));
        assert!(map.contains("KB_VAULT_ROOT"));
        assert!(map.contains("KB_AGENT_ROOT"));

        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
    }

    #[test]
    fn build_env_rejects_missing_or_key() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("OPENROUTER_API_KEY"); }
        let tmp = tempfile::TempDir::new().unwrap();
        let res = build_subprocess_env(
            &tmp.path().join("e.md"),
            &tmp.path().join("p.jsonl"),
            "apply",
            &tmp.path().join("wrap"),
            &tmp.path().join("vault"),
            &tmp.path().join("vault/Sources"),
            &tmp.path().join("vault/KB"),
        );
        assert!(matches!(res, Err(AgentError::MissingApiKey)));
    }
}
