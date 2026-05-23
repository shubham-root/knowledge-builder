//! JSON result parser for processor subprocess stdout.
//!
//! The processor contract (§8 of PLAN.md) states that **the last line** of the
//! processor's `stdout` must be a JSON object describing the result.  All
//! preceding lines are treated as freeform log output and are ignored.
//!
//! # Wire format
//!
//! Successful run:
//! ```json
//! {"status": "ok", "outputs": [{"path": "...", "kind": "markdown", "bytes": 1234}], "metadata": {...}}
//! ```
//!
//! Failed run:
//! ```json
//! {"status": "error", "error": "...", "retryable": true, "metadata": {...}}
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use kb_worker::parser::parse_processor_output;
//!
//! let lines: Vec<String> = vec![
//!     "[INFO] extracting text…".into(),
//!     r#"{"status":"ok","outputs":[],"metadata":null}"#.into(),
//! ];
//! let result = parse_processor_output(&lines).expect("should parse");
//! assert!(result.is_ok());
//! ```

use kb_core::ProcessResult;
use thiserror::Error;

// ── ParseError ────────────────────────────────────────────────────────────────

/// Errors that can occur while parsing the processor's `stdout` output.
///
/// Each variant corresponds to a distinct failure mode so callers can decide
/// whether to mark the job retryable, log a structured error, etc.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// The stdout slice was empty, or every line was whitespace-only.
    ///
    /// This typically means the processor exited without producing any output
    /// (crash, OOM, early exit before the JSON was written).
    #[error("processor produced no output (stdout was empty)")]
    EmptyOutput,

    /// The last non-empty line of stdout is not a JSON object.
    ///
    /// This means the processor wrote log lines but never wrote the required
    /// final JSON result — a contract violation.
    #[error("last line of processor output is not a JSON object: {last_line:?}")]
    NoJsonLine {
        /// The last non-empty line that was found (may be a log line, a
        /// partial write, etc.).
        last_line: String,
    },

    /// The last non-empty line looks like JSON (starts with `{`) but either:
    /// - has a syntax error (malformed JSON), or
    /// - parses as a valid JSON object but is missing required fields (e.g.
    ///   `"status": "ok"` without an `"outputs"` array).
    #[error("invalid JSON in processor output — {error}; line was: {line:?}")]
    InvalidJson {
        /// The raw line that failed to parse or deserialize.
        line: String,
        /// The underlying `serde_json` error message.
        error: String,
    },

    /// The JSON object parsed correctly but the `"status"` field contains a
    /// value other than `"ok"` or `"error"`.
    ///
    /// This is a processor contract violation — the daemon cannot determine
    /// whether to record outputs or mark the job as failed.
    #[error("unknown status {status:?} in processor output JSON (expected \"ok\" or \"error\")")]
    UnknownStatus {
        /// The unrecognized status string extracted from the JSON.
        status: String,
    },
}

// ── parse_processor_output ────────────────────────────────────────────────────

/// Parse the processor result from the collected `stdout` lines.
///
/// # Algorithm
///
/// 1. Scan `stdout_lines` from the end; find the **last non-empty line**.
/// 2. If that line starts with `{`, attempt `serde_json` parsing as a
///    [`serde_json::Value`]; on failure → [`ParseError::InvalidJson`].
/// 3. If the line does *not* start with `{`, it is not a JSON object →
///    [`ParseError::NoJsonLine`].
/// 4. Extract the `"status"` string from the parsed value.
///    - Missing `"status"` field → [`ParseError::InvalidJson`].
///    - Unknown status string → [`ParseError::UnknownStatus`].
/// 5. Deserialize the value into [`ProcessResult`] via `serde_json::from_value`.
///    Any field-level error (missing `"outputs"`, wrong type, etc.) →
///    [`ParseError::InvalidJson`].
///
/// # Errors
///
/// Returns a [`ParseError`] describing the first (and only) error encountered.
/// See the enum documentation for details on each variant.
///
/// # Examples
///
/// ```
/// use kb_worker::parser::{parse_processor_output, ParseError};
///
/// // Success case — log lines followed by the result JSON.
/// let lines: Vec<String> = vec![
///     "[INFO] step: extract".into(),
///     "[INFO] step: summarise".into(),
///     r#"{"status":"ok","outputs":[{"path":"/vault/note.md","kind":"markdown","bytes":512}]}"#.into(),
/// ];
/// let result = parse_processor_output(&lines).unwrap();
/// assert!(result.is_ok());
/// assert_eq!(result.outputs().len(), 1);
///
/// // Failure case — empty stdout.
/// let empty: Vec<String> = vec![];
/// assert!(matches!(parse_processor_output(&empty), Err(ParseError::EmptyOutput)));
/// ```
pub fn parse_processor_output(stdout_lines: &[String]) -> Result<ProcessResult, ParseError> {
    // ── Step 1: find the last non-empty line ──────────────────────────────
    let last_line = stdout_lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or(ParseError::EmptyOutput)?;

    let trimmed = last_line.trim();

    // ── Step 2: attempt JSON parse ────────────────────────────────────────
    // Only lines that begin with `{` are treated as JSON object attempts.
    // Any other content (plain text log lines, numbers, arrays …) is a
    // contract violation → NoJsonLine.
    if !trimmed.starts_with('{') {
        return Err(ParseError::NoJsonLine {
            last_line: last_line.clone(),
        });
    }

    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| ParseError::InvalidJson {
            line: last_line.clone(),
            error: e.to_string(),
        })?;

    // ── Step 3: inspect the "status" discriminant ─────────────────────────
    let status = match value.get("status").and_then(|v| v.as_str()) {
        Some(s) => s.to_owned(),
        None => {
            return Err(ParseError::InvalidJson {
                line: last_line.clone(),
                error: "missing required field 'status'".to_string(),
            });
        }
    };

    // ── Step 4: deserialize into ProcessResult ────────────────────────────
    match status.as_str() {
        "ok" | "error" => {
            // serde's internally-tagged `#[serde(tag = "status")]` will
            // handle the variant selection; any missing required field
            // surfaces as a deserialization error.
            serde_json::from_value::<ProcessResult>(value).map_err(|e| {
                ParseError::InvalidJson {
                    line: last_line.clone(),
                    error: e.to_string(),
                }
            })
        }
        other => Err(ParseError::UnknownStatus {
            status: other.to_owned(),
        }),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kb_core::ProcessResult;

    // ── helpers ───────────────────────────────────────────────────────────

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── 1. Valid `ok` result — multiple log lines before JSON ─────────────

    #[test]
    fn valid_ok_result_with_log_preamble() {
        let stdout = lines(&[
            "[INFO] extracting text from PDF",
            "[INFO] running LLM summariser",
            "[DEBUG] tokens_in=1234 tokens_out=567",
            r#"{"status":"ok","outputs":[{"path":"/vault/notes/foo.md","kind":"markdown","bytes":2048},{"path":"/vault/notes/foo-fig1.png","kind":"asset","bytes":98304}],"metadata":{"model":"gpt-4o-mini","tokens_in":1234,"tokens_out":567}}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse ok result");

        assert!(result.is_ok(), "expected Ok variant");
        assert!(!result.is_err());
        assert!(!result.is_retryable());

        let outputs = result.outputs();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].kind, "markdown");
        assert_eq!(outputs[0].bytes, 2048);
        assert_eq!(outputs[1].kind, "asset");
        assert_eq!(outputs[1].bytes, 98304);
    }

    // ── 2. Valid `error` result ────────────────────────────────────────────

    #[test]
    fn valid_error_result_retryable() {
        let stdout = lines(&[
            "[INFO] starting extraction",
            "[ERROR] OCR backend unavailable",
            r#"{"status":"error","error":"OCR backend unavailable: connection refused","retryable":true,"metadata":{"step":"extract"}}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse error result");

        assert!(result.is_err(), "expected Error variant");
        assert!(!result.is_ok());
        assert!(result.is_retryable(), "should be retryable");
        assert_eq!(
            result.error_message(),
            Some("OCR backend unavailable: connection refused")
        );
        assert_eq!(result.outputs().len(), 0, "error result has no outputs");
    }

    #[test]
    fn valid_error_result_non_retryable() {
        let stdout = lines(&[
            r#"{"status":"error","error":"unsupported file format","retryable":false}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse non-retryable error");

        assert!(result.is_err());
        assert!(!result.is_retryable());
        assert_eq!(result.error_message(), Some("unsupported file format"));
    }

    #[test]
    fn valid_error_result_default_retryable_when_field_absent() {
        // When `retryable` is absent, the default is `true` (see ProcessResult).
        let stdout = lines(&[
            r#"{"status":"error","error":"transient network failure"}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse");
        assert!(result.is_err());
        assert!(result.is_retryable(), "missing retryable should default to true");
    }

    // ── 3. Empty stdout → EmptyOutput ─────────────────────────────────────

    #[test]
    fn empty_stdout_slice_returns_empty_output() {
        let stdout: Vec<String> = vec![];
        match parse_processor_output(&stdout) {
            Err(ParseError::EmptyOutput) => {}
            other => panic!("expected EmptyOutput, got {other:?}"),
        }
    }

    #[test]
    fn all_whitespace_lines_returns_empty_output() {
        let stdout = lines(&["", "   ", "\t", "\n", "  \t  "]);
        match parse_processor_output(&stdout) {
            Err(ParseError::EmptyOutput) => {}
            other => panic!("expected EmptyOutput, got {other:?}"),
        }
    }

    // ── 4. Non-JSON last line → NoJsonLine ────────────────────────────────

    #[test]
    fn plain_text_last_line_returns_no_json_line() {
        let stdout = lines(&[
            "[INFO] starting processor",
            "[INFO] extraction complete",
            "done!",
        ]);

        match parse_processor_output(&stdout) {
            Err(ParseError::NoJsonLine { last_line }) => {
                assert_eq!(last_line, "done!");
            }
            other => panic!("expected NoJsonLine, got {other:?}"),
        }
    }

    #[test]
    fn numeric_last_line_returns_no_json_line() {
        let stdout = lines(&["[INFO] processing", "42"]);

        match parse_processor_output(&stdout) {
            Err(ParseError::NoJsonLine { .. }) => {}
            other => panic!("expected NoJsonLine, got {other:?}"),
        }
    }

    #[test]
    fn empty_json_array_last_line_returns_no_json_line() {
        // An array `[]` is valid JSON but not a JSON object — NoJsonLine.
        let stdout = lines(&["[]"]);

        match parse_processor_output(&stdout) {
            Err(ParseError::NoJsonLine { .. }) => {}
            other => panic!("expected NoJsonLine, got {other:?}"),
        }
    }

    // ── 5. Malformed JSON on last line → InvalidJson ──────────────────────

    #[test]
    fn malformed_json_returns_invalid_json() {
        let stdout = lines(&[
            "[INFO] step done",
            r#"{"status":"ok", outputs: [BAD JSON HERE}"#,
        ]);

        match parse_processor_output(&stdout) {
            Err(ParseError::InvalidJson { line, error }) => {
                assert!(line.contains("outputs"), "line should contain the bad content");
                assert!(!error.is_empty(), "error message should be non-empty");
            }
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn truncated_json_returns_invalid_json() {
        // Simulates a processor that was killed mid-write.
        let stdout = lines(&[r#"{"status":"ok","outputs":["#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::InvalidJson { .. }) => {}
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    // ── 6. Valid JSON but unknown status → UnknownStatus ──────────────────

    #[test]
    fn unknown_status_pending_returns_unknown_status() {
        let stdout = lines(&[r#"{"status":"pending","outputs":[]}"#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::UnknownStatus { status }) => {
                assert_eq!(status, "pending");
            }
            other => panic!("expected UnknownStatus, got {other:?}"),
        }
    }

    #[test]
    fn unknown_status_success_returns_unknown_status() {
        // A processor erroneously using "success" instead of "ok".
        let stdout = lines(&[r#"{"status":"success","outputs":[]}"#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::UnknownStatus { status }) => {
                assert_eq!(status, "success");
            }
            other => panic!("expected UnknownStatus, got {other:?}"),
        }
    }

    #[test]
    fn unknown_status_capitalised_returns_unknown_status() {
        // Status matching is case-sensitive per the contract.
        let stdout = lines(&[r#"{"status":"Ok","outputs":[]}"#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::UnknownStatus { status }) => {
                assert_eq!(status, "Ok");
            }
            other => panic!("expected UnknownStatus, got {other:?}"),
        }
    }

    // ── 7. Missing required fields → InvalidJson ──────────────────────────

    #[test]
    fn missing_status_field_returns_invalid_json() {
        let stdout = lines(&[
            r#"{"outputs":[{"path":"/vault/out.md","kind":"markdown","bytes":100}]}"#,
        ]);

        match parse_processor_output(&stdout) {
            Err(ParseError::InvalidJson { error, .. }) => {
                assert!(
                    error.contains("status"),
                    "error should mention missing 'status' field: {error}"
                );
            }
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn ok_status_missing_outputs_field_returns_invalid_json() {
        // `outputs` is required for the `ok` variant.
        let stdout = lines(&[r#"{"status":"ok","metadata":{"model":"gpt-4o"}}"#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::InvalidJson { line, error }) => {
                assert!(line.contains("ok"), "line should be the JSON we provided");
                assert!(!error.is_empty());
            }
            other => panic!("expected InvalidJson (missing outputs), got {other:?}"),
        }
    }

    #[test]
    fn error_status_missing_error_field_returns_invalid_json() {
        // `error` (the message string) is required for the `error` variant.
        let stdout = lines(&[r#"{"status":"error","retryable":true}"#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::InvalidJson { .. }) => {}
            other => panic!("expected InvalidJson (missing error field), got {other:?}"),
        }
    }

    #[test]
    fn outputs_wrong_type_returns_invalid_json() {
        // `outputs` must be an array, not a string.
        let stdout = lines(&[r#"{"status":"ok","outputs":"not-an-array"}"#]);

        match parse_processor_output(&stdout) {
            Err(ParseError::InvalidJson { .. }) => {}
            other => panic!("expected InvalidJson (wrong type), got {other:?}"),
        }
    }

    // ── 8. Multiple JSON objects — only the last one counts ───────────────

    #[test]
    fn multiple_json_lines_only_last_is_parsed() {
        let stdout = lines(&[
            // First JSON line — this is an intermediate progress object that
            // the processor wrote mid-run; should be ignored.
            r#"{"status":"ok","outputs":[]}"#,
            "[INFO] continuing to next step…",
            // Second JSON line — this is the real result.
            r#"{"status":"ok","outputs":[{"path":"/vault/final.md","kind":"markdown","bytes":9999}]}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse last JSON line");
        assert!(result.is_ok());
        let outputs = result.outputs();
        assert_eq!(outputs.len(), 1, "should only see outputs from the last JSON line");
        assert_eq!(outputs[0].bytes, 9999);
    }

    #[test]
    fn last_json_wins_even_if_earlier_was_valid_ok() {
        // If the processor crashes after writing a valid JSON and appends an
        // error line, the error takes precedence because it is last.
        let stdout = lines(&[
            r#"{"status":"ok","outputs":[{"path":"/vault/out.md","kind":"markdown","bytes":512}]}"#,
            r#"{"status":"error","error":"post-processing step failed","retryable":false}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse");
        assert!(result.is_err(), "the last line (error) should win");
        assert!(!result.is_retryable());
    }

    // ── 9. Whitespace trimming ─────────────────────────────────────────────

    #[test]
    fn json_line_with_leading_trailing_whitespace_is_accepted() {
        let stdout = lines(&[
            r#"   {"status":"ok","outputs":[]}   "#,
        ]);

        let result = parse_processor_output(&stdout).expect("should trim and parse");
        assert!(result.is_ok());
        assert_eq!(result.outputs().len(), 0);
    }

    #[test]
    fn trailing_whitespace_lines_after_json_are_skipped() {
        // Blank lines after the JSON should be ignored; the last *non-empty*
        // line is what matters.
        let stdout = lines(&[
            r#"{"status":"ok","outputs":[]}"#,
            "",
            "   ",
        ]);

        let result = parse_processor_output(&stdout).expect("trailing blanks should be skipped");
        assert!(result.is_ok());
    }

    // ── 10. Metadata field is optional ────────────────────────────────────

    #[test]
    fn ok_result_without_metadata_parses_correctly() {
        let stdout = lines(&[
            r#"{"status":"ok","outputs":[{"path":"/vault/out.md","kind":"markdown","bytes":100}]}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("metadata is optional");
        assert!(result.is_ok());
        match result {
            ProcessResult::Ok { metadata, .. } => {
                assert!(metadata.is_none(), "metadata should be None when absent");
            }
            _ => panic!("expected Ok variant"),
        }
    }

    #[test]
    fn error_result_with_rich_metadata_parses_correctly() {
        let stdout = lines(&[
            r#"{"status":"error","error":"API rate limit","retryable":true,"metadata":{"step":"llm","attempt":2,"wait_secs":60}}"#,
        ]);

        let result = parse_processor_output(&stdout).expect("should parse rich metadata");
        assert!(result.is_err());
        assert!(result.is_retryable());
        match result {
            ProcessResult::Error { metadata, .. } => {
                let meta = metadata.expect("metadata should be present");
                assert_eq!(meta["step"], "llm");
                assert_eq!(meta["wait_secs"], 60);
            }
            _ => panic!("expected Error variant"),
        }
    }
}
