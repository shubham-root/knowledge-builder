//! Processor subprocess invocation and JSON result parsing.
//!
//! Spawns the configured processor command, feeds `ProcessorInput` as JSON on
//! stdin, enforces a hard timeout with `SIGTERM` + `SIGKILL` escalation, reads
//! stdout line by line, and parses the **last non-empty line** as a
//! `ProcessResult`.
//!
//! # Contract (§8 Processor Contract)
//!
//! ```text
//! <processor.command> <input_path> <work_dir>
//! stdin  ← JSON ProcessorInput (single line, then EOF)
//! stdout → arbitrary log lines, LAST line is JSON ProcessResult
//! stderr → captured and logged at WARN level
//! exit 0 → ok; non-zero → error
//! ```
//!
//! # Timeout behaviour
//!
//! 1. `SIGTERM` sent to the **process group** (`killpg`).
//! 2. 5-second grace period.
//! 3. `SIGKILL` sent unconditionally.
//! 4. Returns [`ProcessorError::Timeout`].

use std::io;
use std::path::Path;
use std::time::Duration;

use kb_core::types::{ProcessorInput, ProcessResult};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::process::Child;
use tracing::{debug, error, warn};

// ── Public types ──────────────────────────────────────────────────────────────

/// Successful (non-timeout, non-io-error) result from spawning the processor.
///
/// The caller should inspect `result` and `exit_code` to determine whether the
/// processor job succeeded or failed; both a non-zero exit code and a
/// `ProcessResult::Error` variant indicate a job failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorOutput {
    /// All lines written to the processor's stdout (log lines + final JSON).
    pub stdout_lines: Vec<String>,

    /// Everything written to the processor's stderr (for debug/audit).
    pub stderr: String,

    /// The parsed `ProcessResult` extracted from the last non-empty stdout line.
    pub result: ProcessResult,

    /// Raw exit code.  `0` = success per the processor contract.
    pub exit_code: i32,
}

/// Errors that prevent obtaining a meaningful [`ProcessorOutput`].
#[derive(Debug, Error)]
pub enum ProcessorError {
    /// The process did not finish within the allocated time.
    #[error("processor timed out after {elapsed_secs}s")]
    Timeout { elapsed_secs: u64 },

    /// The process could not be spawned at all (executable not found, bad
    /// permissions, etc.).
    #[error("failed to spawn processor command `{command}`: {error}")]
    SpawnFailed {
        command: String,
        #[source]
        error: io::Error,
    },

    /// The process ran and exited, but the last stdout line was absent or could
    /// not be parsed as a `ProcessResult`.
    #[error("processor produced unparseable output: {parse_error}\nstdout:\n{stdout}")]
    InvalidOutput { stdout: String, parse_error: String },

    /// The process exited with a non-zero code and no parseable JSON result was
    /// present on stdout (i.e., the processor crashed rather than reporting an
    /// error via the contract).
    #[error("processor exited with code {code}\nstderr:\n{stderr}")]
    NonZeroExit { code: i32, stderr: String },

    /// Any other I/O error (creating work_dir, reading pipes, etc.).
    #[error("processor I/O error: {0}")]
    IoError(#[from] io::Error),
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Invoke the processor for one job.
///
/// # Arguments
///
/// * `command`       — Processor executable path (may be `"prog arg0 arg1"`
///                     style; tokens are split on ASCII whitespace and the
///                     first becomes the program, the rest become leading args).
/// * `input`         — The structured input payload; serialised to JSON on
///                     the child's stdin.
/// * `work_dir`      — Per-job scratch directory.  Created if absent.  Passed
///                     as the second positional argument to the processor.
/// * `timeout_secs`  — Hard wall-clock timeout.  On expiry: `SIGTERM` → 5 s
///                     grace → `SIGKILL`.
///
/// # Returns
///
/// A [`ProcessorOutput`] on success (even if the processor reported
/// `status: "error"` in its JSON — that is a *job* error, not an invocation
/// error).  A [`ProcessorError`] if the subprocess couldn't be spawned, timed
/// out, or produced no parseable JSON.
pub async fn invoke_processor(
    command: &str,
    input: &ProcessorInput,
    work_dir: &Path,
    timeout_secs: u64,
) -> Result<ProcessorOutput, ProcessorError> {
    // ── (a) Ensure work directory exists ──────────────────────────────────
    tokio::fs::create_dir_all(work_dir).await?;

    // ── (b) Build the command ──────────────────────────────────────────────
    // Support "program [initial_args...]" by splitting on whitespace.
    let mut parts = command.split_ascii_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "processor command is empty"))?;

    let mut cmd = Command::new(program);
    for part in parts {
        cmd.arg(part);
    }
    // Positional args per §8 contract: <input_path> <work_dir>
    cmd.arg(input.input_path.as_os_str());
    cmd.arg(work_dir.as_os_str());

    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(work_dir);

    // Put the child in its own process group so we can kill the whole tree.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    // ── Spawn ─────────────────────────────────────────────────────────────
    let mut child = cmd.spawn().map_err(|e| ProcessorError::SpawnFailed {
        command: command.to_string(),
        error: e,
    })?;

    // Capture the PID *before* any async moves; used for kill-on-timeout.
    let child_pid = child.id().unwrap_or(0) as i32;

    // ── (c) Write JSON ProcessorInput to stdin, then close stdin ──────────
    let stdin_handle = child.stdin.take().expect("stdin should be piped");
    let json_bytes = serde_json::to_vec(input).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("failed to serialise ProcessorInput: {e}"))
    })?;

    // Write in a detached task: the processor may exit (or write a lot to
    // stdout) before consuming all stdin, causing the write to fail with
    // EPIPE — that is expected and harmless.
    tokio::spawn(async move {
        let mut s = stdin_handle;
        let _ = s.write_all(&json_bytes).await;
        // Drop closes stdin → sends EOF to child.
    });

    // Take stdout/stderr handles before moving `child` into the timeout future.
    let stdout_handle = child.stdout.take().expect("stdout should be piped");
    let stderr_handle = child.stderr.take().expect("stderr should be piped");

    // ── (d–f) Read output with timeout ────────────────────────────────────
    let io_result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        read_child_output(stdout_handle, stderr_handle, &mut child),
    )
    .await;

    let (stdout_lines, stderr_str, exit_code) = match io_result {
        Ok(Ok(triple)) => triple,
        Ok(Err(io_err)) => return Err(ProcessorError::IoError(io_err)),

        // ── Timeout path ──────────────────────────────────────────────────
        Err(_elapsed) => {
            error!(
                timeout_secs,
                pid = child_pid,
                "processor timed out — sending SIGTERM to process group"
            );

            #[cfg(unix)]
            kill_process_group(child_pid, libc::SIGTERM);

            // Grace period.
            tokio::time::sleep(Duration::from_secs(5)).await;

            #[cfg(unix)]
            kill_process_group(child_pid, libc::SIGKILL);

            // Reap the zombie / wait to unblock tokio's process watcher.
            let _ = child.wait().await;

            return Err(ProcessorError::Timeout {
                elapsed_secs: timeout_secs,
            });
        }
    };

    // ── (g) Parse last stdout line as ProcessResult ────────────────────────
    parse_output(stdout_lines, stderr_str, exit_code)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Read all stdout lines + all stderr, then wait for the child to exit.
///
/// Returns `(stdout_lines, stderr, exit_code)`.
async fn read_child_output(
    stdout_handle: tokio::process::ChildStdout,
    stderr_handle: tokio::process::ChildStderr,
    child: &mut Child,
) -> io::Result<(Vec<String>, String, i32)> {
    // (e) Read stdout line-by-line (for logging).
    let stdout_future = async {
        let mut reader = BufReader::new(stdout_handle);
        let mut lines: Vec<String> = Vec::new();
        let mut buf = String::new();

        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => break,           // EOF
                Ok(_) => {
                    // Strip trailing CR/LF.
                    let line = buf.trim_end_matches(['\n', '\r']).to_string();
                    if !line.is_empty() {
                        debug!(processor_line = %line, "processor stdout");
                    }
                    lines.push(line);
                }
                Err(e) => {
                    warn!(error = %e, "error reading processor stdout");
                    break;
                }
            }
        }
        lines
    };

    let stderr_future = async {
        let mut reader = BufReader::new(stderr_handle);
        let mut s = String::new();
        let _ = reader.read_to_string(&mut s).await;
        if !s.trim().is_empty() {
            warn!(processor_stderr = %s.trim(), "processor stderr output");
        }
        s
    };

    // Drive stdout and stderr concurrently to avoid pipe-buffer deadlocks.
    let (stdout_lines, stderr_str) = tokio::join!(stdout_future, stderr_future);

    // Pipes are drained; now wait for the process to exit.
    let status = child.wait().await?;
    let exit_code = status.code().unwrap_or(-1);

    Ok((stdout_lines, stderr_str, exit_code))
}

/// Extract the last non-empty line from `stdout_lines`, attempt to parse it
/// as JSON, and assemble a [`ProcessorOutput`] or the appropriate error.
fn parse_output(
    stdout_lines: Vec<String>,
    stderr_str: String,
    exit_code: i32,
) -> Result<ProcessorOutput, ProcessorError> {
    let last_line = stdout_lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(String::as_str)
        .unwrap_or("");

    if last_line.is_empty() {
        // Stdout was completely empty (or only blank lines).
        if exit_code != 0 {
            return Err(ProcessorError::NonZeroExit {
                code: exit_code,
                stderr: stderr_str,
            });
        }
        return Err(ProcessorError::InvalidOutput {
            stdout: stdout_lines.join("\n"),
            parse_error: "stdout was empty — no JSON result line present".to_string(),
        });
    }

    // Try to parse the last line as a ProcessResult.
    match serde_json::from_str::<ProcessResult>(last_line) {
        Ok(result) => {
            // We have a valid JSON result.  Return it even for non-zero exit so
            // the worker layer can extract the `retryable` flag from the Error
            // variant rather than blindly retrying.
            if exit_code != 0 {
                warn!(
                    exit_code,
                    last_line,
                    "processor exited non-zero but provided a parseable result"
                );
            }
            Ok(ProcessorOutput {
                stdout_lines,
                stderr: stderr_str,
                result,
                exit_code,
            })
        }
        Err(parse_err) => {
            // Last line is not valid JSON.
            if exit_code != 0 {
                Err(ProcessorError::NonZeroExit {
                    code: exit_code,
                    stderr: stderr_str,
                })
            } else {
                Err(ProcessorError::InvalidOutput {
                    stdout: stdout_lines.join("\n"),
                    parse_error: format!(
                        "failed to parse last stdout line as ProcessResult: {parse_err}\nline: {last_line}"
                    ),
                })
            }
        }
    }
}

/// Send `signal` to the process group with PGID = `pgid`.
///
/// Logs a warning on failure (e.g., the process already exited).
#[cfg(unix)]
fn kill_process_group(pgid: i32, signal: libc::c_int) {
    if pgid <= 0 {
        warn!(pgid, signal, "kill_process_group: invalid pgid, skipping");
        return;
    }
    // SAFETY: `killpg(2)` is a POSIX syscall with well-defined semantics.
    // ESRCH (no such process group) is harmless and expected if the process
    // already exited during the grace period.
    let ret = unsafe { libc::killpg(pgid, signal) };
    if ret != 0 {
        let err = io::Error::last_os_error();
        // ESRCH just means the process already exited — not an error.
        if err.raw_os_error() != Some(libc::ESRCH) {
            warn!(pgid, signal, error = %err, "killpg failed");
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use kb_core::types::{ProcessOutput, ProcessResult};

    fn make_input() -> ProcessorInput {
        ProcessorInput {
            input_path: PathBuf::from("/vault/sources/test.pdf"),
            content_hash: "sha256:deadbeef".to_string(),
            vault_root: PathBuf::from("/vault"),
            sources_dir: PathBuf::from("/vault/sources"),
            work_dir: PathBuf::from("/tmp/kb-jobs/test"),
            job_id: 1,
            attempt: 1,
        }
    }

    // ── parse_output unit tests ────────────────────────────────────────────

    #[test]
    fn parse_ok_result() {
        let json = r#"{"status":"ok","outputs":[{"path":"/vault/notes/test.md","kind":"markdown","bytes":100}]}"#;
        let lines = vec![
            "INFO extracting pdf".to_string(),
            json.to_string(),
        ];
        let out = parse_output(lines, String::new(), 0).expect("should parse ok result");
        assert_eq!(out.exit_code, 0);
        assert!(out.result.is_ok());
        assert_eq!(out.result.outputs().len(), 1);
    }

    #[test]
    fn parse_error_result() {
        let json = r#"{"status":"error","error":"extraction failed","retryable":true}"#;
        let lines = vec![json.to_string()];
        let out = parse_output(lines, String::new(), 1).expect("should return output even on error");
        assert_eq!(out.exit_code, 1);
        assert!(out.result.is_err());
    }

    #[test]
    fn parse_empty_stdout_nonzero_exit() {
        let err = parse_output(vec![], "process crashed\n".to_string(), 2)
            .expect_err("empty stdout + nonzero should error");
        assert!(matches!(err, ProcessorError::NonZeroExit { code: 2, .. }));
    }

    #[test]
    fn parse_empty_stdout_zero_exit() {
        let err = parse_output(vec![], String::new(), 0)
            .expect_err("empty stdout + zero exit should be invalid output");
        assert!(matches!(err, ProcessorError::InvalidOutput { .. }));
    }

    #[test]
    fn parse_malformed_json_nonzero_exit() {
        let lines = vec!["this is not json".to_string()];
        let err = parse_output(lines, "stderr msg".to_string(), 127)
            .expect_err("bad json + nonzero exit should error");
        assert!(matches!(err, ProcessorError::NonZeroExit { code: 127, .. }));
    }

    #[test]
    fn parse_malformed_json_zero_exit() {
        let lines = vec!["this is not json".to_string()];
        let err = parse_output(lines, String::new(), 0)
            .expect_err("bad json + zero exit should be invalid output");
        assert!(matches!(err, ProcessorError::InvalidOutput { .. }));
    }

    #[test]
    fn parse_last_line_is_used() {
        // First line is valid JSON, last line is log output — should fail.
        let good_json = r#"{"status":"ok","outputs":[]}"#;
        let lines = vec![
            good_json.to_string(),
            "processor done".to_string(),
        ];
        let err = parse_output(lines, String::new(), 0)
            .expect_err("last line is not JSON, should fail");
        assert!(matches!(err, ProcessorError::InvalidOutput { .. }));
    }

    #[test]
    fn parse_skips_trailing_blank_lines() {
        let json = r#"{"status":"ok","outputs":[]}"#;
        let lines = vec![
            json.to_string(),
            "".to_string(),
            "  ".to_string(),
        ];
        let out = parse_output(lines, String::new(), 0).expect("should skip blank trailing lines");
        assert!(out.result.is_ok());
    }

    // ── invoke_processor integration tests (require /bin/sh) ──────────────

    #[tokio::test]
    async fn invoke_processor_happy_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path().join("work");

        // A tiny shell script that prints a log line then the result JSON.
        let script = tmp.path().join("proc.sh");
        tokio::fs::write(
            &script,
            b"#!/bin/sh\necho 'log line'\necho '{\"status\":\"ok\",\"outputs\":[]}'\nexit 0\n",
        )
        .await
        .unwrap();

        // Make executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let input = make_input();
        let result = invoke_processor(
            script.to_str().unwrap(),
            &input,
            &work,
            30,
        )
        .await
        .expect("invoke_processor should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(result.result.is_ok());
        assert!(result.stdout_lines.iter().any(|l| l.contains("log line")));
    }

    #[tokio::test]
    async fn invoke_processor_nonzero_exit_no_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path().join("work");
        let script = tmp.path().join("proc.sh");
        tokio::fs::write(&script, b"#!/bin/sh\necho 'something went wrong' >&2\nexit 1\n")
            .await
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let input = make_input();
        let err = invoke_processor(script.to_str().unwrap(), &input, &work, 30)
            .await
            .expect_err("should fail with NonZeroExit");
        assert!(matches!(err, ProcessorError::NonZeroExit { code: 1, .. }));
    }

    #[tokio::test]
    async fn invoke_processor_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path().join("work");
        let script = tmp.path().join("slow.sh");
        tokio::fs::write(&script, b"#!/bin/sh\nsleep 60\n")
            .await
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let input = make_input();
        let err = invoke_processor(script.to_str().unwrap(), &input, &work, 1)
            .await
            .expect_err("should time out");
        assert!(matches!(err, ProcessorError::Timeout { elapsed_secs: 1 }));
    }

    #[tokio::test]
    async fn invoke_processor_spawn_failed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path().join("work");
        let input = make_input();
        let err =
            invoke_processor("/nonexistent/proc", &input, &work, 30)
                .await
                .expect_err("nonexistent command should fail");
        assert!(matches!(err, ProcessorError::SpawnFailed { .. }));
    }

    #[tokio::test]
    async fn invoke_processor_creates_work_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Nested path that doesn't exist yet.
        let work = tmp.path().join("a").join("b").join("c");
        assert!(!work.exists());

        let script = tmp.path().join("proc.sh");
        tokio::fs::write(
            &script,
            b"#!/bin/sh\necho '{\"status\":\"ok\",\"outputs\":[]}'\n",
        )
        .await
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let input = make_input();
        invoke_processor(script.to_str().unwrap(), &input, &work, 30)
            .await
            .expect("should succeed");
        assert!(work.exists(), "work_dir should have been created");
    }
}
