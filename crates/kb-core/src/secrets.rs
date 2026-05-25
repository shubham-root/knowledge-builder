//! Processor secrets loader.
//!
//! Reads `~/.config/knowledge-builder/secrets.env` and parses it into a
//! `BTreeMap<String, String>` that the daemon forwards into every processor
//! subprocess (via `Command::envs`).  This gives a single, version-control-
//! safe home for credentials regardless of whether the daemon is launched
//! by `launchd` or run directly via `kb daemon --foreground`.
//!
//! ## File format
//!
//! ```dotenv
//! # Comments and blank lines are ignored.
//! KB_LLM_MODEL=openrouter/anthropic/claude-3.5-haiku
//! OPENROUTER_API_KEY=sk-or-v1-...
//! # Optional: KB_LLM_MAX_CONTENT_CHARS=80000
//! ```
//!
//! Values may optionally be wrapped in matching single or double quotes; the
//! quotes are stripped.  `export ` prefixes are tolerated for compatibility
//! with shell-style `.env` files.
//!
//! ## Permissions
//!
//! Because the file holds credentials it should be `chmod 600` (owner-only
//! read+write).  [`load_secrets`] emits a warning event when the mode permits
//! group/world access; it does **not** refuse to load — that decision belongs
//! to the operator.
//!
//! ## Missing file
//!
//! Returns `Ok(BTreeMap::new())` (silent success) if the file does not exist.
//! This mirrors the behaviour of `Config::load` / `figment`'s `Toml::file` and
//! lets the daemon start with default behaviour for users who haven't
//! configured a processor that needs secrets yet.

use std::{
    collections::BTreeMap,
    fs,
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SecretsError {
    /// Failed to read the secrets file.
    #[error("failed to read secrets file '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A line could not be parsed as `KEY=VALUE`.
    #[error("secrets file '{path}' line {line}: {detail}")]
    Parse {
        path:   PathBuf,
        line:   usize,
        detail: String,
    },
}

// ── Loaded data + status ──────────────────────────────────────────────────────

/// Result of loading the secrets file, including diagnostic info for
/// `kb doctor`.
#[derive(Debug, Clone, Default)]
pub struct SecretsLoad {
    /// Resolved path that was attempted.
    pub path: PathBuf,
    /// `true` if the file existed and was read successfully.
    pub loaded: bool,
    /// `true` if the file was found but its mode permits group/world access.
    pub insecure_perms: bool,
    /// File mode bits (Unix only).  `None` on non-Unix platforms or when stat
    /// failed.
    pub mode: Option<u32>,
    /// Parsed key/value pairs.  Empty if the file was missing.
    pub entries: BTreeMap<String, String>,
}

impl SecretsLoad {
    /// Names of the keys that were loaded.  Used by `kb doctor` to show what
    /// is in effect *without* printing the values.
    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }
}

// ── Path resolution ───────────────────────────────────────────────────────────

/// Resolved path to the secrets file: `~/.config/knowledge-builder/secrets.env`.
///
/// Same convention as [`config_file_path`](crate::config::config_file_path) —
/// XDG-style on every platform, deliberately *not* `dirs::config_dir()` (which
/// returns `~/Library/Application Support` on macOS).
pub fn secrets_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("knowledge-builder")
        .join("secrets.env")
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// Load the secrets file from its standard XDG path.
///
/// Returns an empty `SecretsLoad` (with `loaded = false`) when the file does
/// not exist.  Returns `Err` only when the file *does* exist but cannot be
/// read or parsed.
pub fn load_secrets() -> Result<SecretsLoad, SecretsError> {
    load_secrets_from(&secrets_file_path())
}

/// Load secrets from an explicit path.  Exposed for tests.
pub fn load_secrets_from(path: &Path) -> Result<SecretsLoad, SecretsError> {
    let mut out = SecretsLoad {
        path: path.to_path_buf(),
        ..Default::default()
    };

    // Stat first; non-existence is silent success.
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(SecretsError::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }
    };

    // Permission check (Unix only).  We treat anything looser than 0600 as
    // insecure; that catches 0640, 0644, 0664, 0666, 0777, etc.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        out.mode = Some(mode);
        // Bits 077 cover all group + world permissions.
        if mode & 0o077 != 0 {
            out.insecure_perms = true;
        }
    }

    // Read + parse.
    let content = fs::read_to_string(path).map_err(|e| SecretsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    out.entries = parse_dotenv(&content, path)?;
    out.loaded = true;
    Ok(out)
}

// ── Parser ────────────────────────────────────────────────────────────────────

fn parse_dotenv(
    content: &str,
    path: &Path,
) -> Result<BTreeMap<String, String>, SecretsError> {
    let mut out = BTreeMap::new();

    for (idx, raw) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();

        // Skip blank lines and comments.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Tolerate a leading `export ` for shell-compat.
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();

        // Split on first `=`.
        let eq = line.find('=').ok_or_else(|| SecretsError::Parse {
            path:   path.to_path_buf(),
            line:   line_no,
            detail: format!("expected KEY=VALUE, got: {raw:?}"),
        })?;

        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();

        if key.is_empty() {
            return Err(SecretsError::Parse {
                path:   path.to_path_buf(),
                line:   line_no,
                detail: "empty key before `=`".to_string(),
            });
        }

        // Validate key shape: ASCII letters, digits, underscore.  This is
        // what most env-var consumers (POSIX, Python `os.environ`, etc.)
        // tolerate without escaping.
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(SecretsError::Parse {
                path:   path.to_path_buf(),
                line:   line_no,
                detail: format!("invalid characters in key: {key:?}"),
            });
        }

        // Strip matching surrounding quotes from the value.
        let val = strip_matching_quotes(val);

        out.insert(key.to_string(), val.to_string());
    }

    Ok(out)
}

fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last  = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(tmp: &TempDir, name: &str, content: &str, mode: u32) -> PathBuf {
        let p = tmp.path().join(name);
        fs::write(&p, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&p).unwrap().permissions();
            perms.set_mode(mode);
            fs::set_permissions(&p, perms).unwrap();
        }
        let _ = mode;
        p
    }

    #[test]
    fn missing_file_is_silent_success() {
        let tmp = TempDir::new().unwrap();
        let p   = tmp.path().join("does-not-exist.env");
        let got = load_secrets_from(&p).unwrap();
        assert!(!got.loaded);
        assert!(got.entries.is_empty());
        assert!(!got.insecure_perms);
    }

    #[test]
    fn parses_simple_key_value() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            &tmp,
            "s.env",
            "KB_LLM_MODEL=openrouter/anthropic/claude-3.5-haiku\nOPENROUTER_API_KEY=sk-or-abc\n",
            0o600,
        );
        let got = load_secrets_from(&p).unwrap();
        assert!(got.loaded);
        assert_eq!(
            got.entries.get("KB_LLM_MODEL").map(String::as_str),
            Some("openrouter/anthropic/claude-3.5-haiku"),
        );
        assert_eq!(
            got.entries.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-or-abc"),
        );
        assert!(!got.insecure_perms);
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            &tmp,
            "s.env",
            "# top comment\n\nKEY1=v1\n   \n  # indented comment\nKEY2=v2\n",
            0o600,
        );
        let got = load_secrets_from(&p).unwrap();
        assert_eq!(got.entries.len(), 2);
    }

    #[test]
    fn strips_matching_quotes_only() {
        let tmp = TempDir::new().unwrap();
        let p = write(
            &tmp,
            "s.env",
            "A=\"double\"\nB='single'\nC=\"mismatched'\nD=plain\n",
            0o600,
        );
        let got = load_secrets_from(&p).unwrap();
        assert_eq!(got.entries["A"], "double");
        assert_eq!(got.entries["B"], "single");
        assert_eq!(got.entries["C"], "\"mismatched'"); // no strip
        assert_eq!(got.entries["D"], "plain");
    }

    #[test]
    fn tolerates_export_prefix() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "s.env", "export FOO=bar\n", 0o600);
        let got = load_secrets_from(&p).unwrap();
        assert_eq!(got.entries["FOO"], "bar");
    }

    #[test]
    fn rejects_missing_equals() {
        let tmp = TempDir::new().unwrap();
        let p   = write(&tmp, "s.env", "BADLINE\n", 0o600);
        let err = load_secrets_from(&p).unwrap_err();
        assert!(matches!(err, SecretsError::Parse { line: 1, .. }));
    }

    #[test]
    fn rejects_invalid_key_chars() {
        let tmp = TempDir::new().unwrap();
        let p   = write(&tmp, "s.env", "BAD-KEY=v\n", 0o600);
        let err = load_secrets_from(&p).unwrap_err();
        assert!(matches!(err, SecretsError::Parse { line: 1, .. }));
    }

    #[cfg(unix)]
    #[test]
    fn flags_world_readable_perms() {
        let tmp = TempDir::new().unwrap();
        let p   = write(&tmp, "s.env", "K=v\n", 0o644);
        let got = load_secrets_from(&p).unwrap();
        assert!(got.insecure_perms, "0644 should trip insecure_perms");
        assert_eq!(got.mode, Some(0o644));
    }
}
