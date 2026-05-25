//! Public error type and `Result` alias for [`crate::Extractor`].

use std::path::PathBuf;

/// Convenience alias.
pub type Result<T> = std::result::Result<T, ExtractionError>;

/// Error returned by the extractor.
///
/// Each variant carries a `retryable` flag implicitly via its name and is
/// exposed via [`ExtractionError::is_retryable`].  The daemon's worker
/// pool reads that to decide whether to backoff-and-retry or move the
/// job straight to the failed bucket.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    /// The transmutation library itself failed to initialise.  In
    /// practice this never happens for the default config.
    #[error("could not initialise the extractor: {0}")]
    Init(String),

    /// The source file does not exist, is not a regular file, or the
    /// current user lacks read permission.
    ///
    /// **Retryable**: false — needs operator action.
    #[error("source unreadable {path}: {detail}")]
    SourceUnreadable {
        /// Path the caller asked us to read.
        path: PathBuf,
        /// OS-level error detail.
        detail: String,
    },

    /// The per-job working directory could not be created or written to.
    ///
    /// **Retryable**: true — typically transient I/O.
    #[error("work_dir unusable {path}: {detail}")]
    WorkDirUnusable {
        /// Path we tried to create/write under.
        path: PathBuf,
        /// OS-level error detail.
        detail: String,
    },

    /// The user requested `mode = ffi` but the kb binary was not built
    /// with the `docling-ffi` cargo feature.
    ///
    /// **Retryable**: false — needs a rebuild.
    #[error(
        "extraction mode 'ffi' is not available in this build of kb. \
         Rebuild with `cargo build --release --features docling-ffi` \
         or change the rule to `mode = \"precision\"` in config.toml."
    )]
    FfiNotCompiled,

    /// transmutation reported success but produced empty markdown.  We
    /// treat this as a transient failure so the daemon retries — usually
    /// caused by a bad model load or a transient parser bug.
    ///
    /// **Retryable**: true.
    #[error("transmutation produced empty markdown for {path} ({format})")]
    EmptyOutput {
        /// Path we tried to extract from.
        path: PathBuf,
        /// Detected format tag.
        format: String,
    },

    /// transmutation refused the document due to a permanent property
    /// (encrypted, corrupted, unsupported format, …).
    ///
    /// **Retryable**: false.
    #[error("permanent extraction failure for {path}: {detail}")]
    Permanent {
        /// Path we tried to extract from.
        path: PathBuf,
        /// Underlying error chain rendered.
        detail: String,
    },

    /// transmutation hit a transient failure (I/O, OOM, parser race).
    ///
    /// **Retryable**: true.
    #[error("transient extraction failure for {path}: {detail}")]
    Transient {
        /// Path we tried to extract from.
        path: PathBuf,
        /// Underlying error chain rendered.
        detail: String,
    },
}

impl ExtractionError {
    /// `true` when the daemon should re-queue the job after backoff.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::WorkDirUnusable { .. }
            | Self::EmptyOutput     { .. }
            | Self::Transient       { .. }
        )
    }

    /// Stable string tag for logging / metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Init(_)              => "init",
            Self::SourceUnreadable {..}=> "source_unreadable",
            Self::WorkDirUnusable  {..}=> "work_dir_unusable",
            Self::FfiNotCompiled       => "ffi_not_compiled",
            Self::EmptyOutput      {..}=> "empty_output",
            Self::Permanent        {..}=> "permanent",
            Self::Transient        {..}=> "transient",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_buckets_match_kind() {
        let cases: &[(ExtractionError, bool, &str)] = &[
            (ExtractionError::Init("x".into()),                            false, "init"),
            (ExtractionError::SourceUnreadable { path: "/a".into(), detail: "x".into() }, false, "source_unreadable"),
            (ExtractionError::WorkDirUnusable  { path: "/a".into(), detail: "x".into() }, true,  "work_dir_unusable"),
            (ExtractionError::FfiNotCompiled,                              false, "ffi_not_compiled"),
            (ExtractionError::EmptyOutput      { path: "/a".into(), format: "pdf".into() }, true,  "empty_output"),
            (ExtractionError::Permanent        { path: "/a".into(), detail: "x".into() }, false, "permanent"),
            (ExtractionError::Transient        { path: "/a".into(), detail: "x".into() }, true,  "transient"),
        ];
        for (err, retryable, kind) in cases {
            assert_eq!(err.is_retryable(), *retryable, "{} should be retryable={}", kind, retryable);
            assert_eq!(err.kind(), *kind);
        }
    }
}
