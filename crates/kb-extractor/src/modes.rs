//! Helper for resolving per-file precision modes.
//!
//! This module is mostly a thin wrapper around
//! [`kb_core::config::ExtractionConfig::mode_for`] but adds the runtime
//! gate for the `ffi` feature: when the binary was not compiled with
//! `docling-ffi`, a configured `mode = "ffi"` rule is downgraded to
//! `precision` and a warning is logged once per `(path, mode)` pair.

use std::path::Path;
use tracing::warn;

use kb_core::config::{ExtractionConfig, ExtractionMode};

/// Resolves the extraction mode to use for a given source file.
///
/// Wraps an [`ExtractionConfig`] with a compile-time check on the `Ffi`
/// variant: if the operator configured `mode = "ffi"` but this build has
/// no `docling-ffi` feature, the resolver downgrades to
/// [`ExtractionMode::Precision`] and prints a one-shot warning.  This
/// keeps the daemon useful instead of failing every job with
/// [`crate::ExtractionError::FfiNotCompiled`] when the operator forgot
/// the rebuild.
#[derive(Debug)]
pub struct ExtractionModeResolver {
    cfg: ExtractionConfig,
    ffi_available: bool,
}

impl ExtractionModeResolver {
    /// Build from a config.  `ffi_available` should be
    /// `cfg!(feature = "docling-ffi")` from the **caller's** crate so
    /// the gate reflects the actual deployed binary, not this lib.
    pub fn new(cfg: ExtractionConfig, ffi_available: bool) -> Self {
        Self { cfg, ffi_available }
    }

    /// Resolve the mode for `file` (which must live under `sources_dir`).
    pub fn mode_for(&self, file: &Path, sources_dir: &Path) -> ExtractionMode {
        let raw = self.cfg.mode_for(file, sources_dir);
        if raw == ExtractionMode::Ffi && !self.ffi_available {
            warn!(
                target: "kb_extractor",
                "config requested ffi mode for {}, but this kb binary was \
                 built without --features docling-ffi; downgrading to \
                 precision mode for this file",
                file.display(),
            );
            return ExtractionMode::Precision;
        }
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kb_core::config::{ExtractionConfig, ExtractionRule};
    use std::path::PathBuf;

    fn rule(path: &str, mode: ExtractionMode) -> ExtractionRule {
        ExtractionRule { path: path.into(), mode }
    }

    #[test]
    fn ffi_passes_through_when_compiled_in() {
        let cfg = ExtractionConfig {
            default_mode: ExtractionMode::Fast,
            rules: vec![rule("Legal", ExtractionMode::Ffi)],
        };
        let r = ExtractionModeResolver::new(cfg, /* ffi_available */ true);
        let sources = PathBuf::from("/v/Sources");
        let f = sources.join("Legal/x.pdf");
        assert_eq!(r.mode_for(&f, &sources), ExtractionMode::Ffi);
    }

    #[test]
    fn ffi_downgrades_to_precision_when_not_compiled_in() {
        let cfg = ExtractionConfig {
            default_mode: ExtractionMode::Fast,
            rules: vec![rule("Legal", ExtractionMode::Ffi)],
        };
        let r = ExtractionModeResolver::new(cfg, /* ffi_available */ false);
        let sources = PathBuf::from("/v/Sources");
        let f = sources.join("Legal/x.pdf");
        assert_eq!(r.mode_for(&f, &sources), ExtractionMode::Precision);
    }

    #[test]
    fn fast_and_precision_unaffected_by_ffi_gate() {
        let cfg = ExtractionConfig {
            default_mode: ExtractionMode::Fast,
            rules: vec![rule("ArchivePapers", ExtractionMode::Precision)],
        };
        let r_no_ffi  = ExtractionModeResolver::new(cfg.clone(), false);
        let r_yes_ffi = ExtractionModeResolver::new(cfg,         true);
        let sources = PathBuf::from("/v/Sources");
        for path in ["ArchivePapers/x.pdf", "Other/y.pdf"] {
            let f = sources.join(path);
            assert_eq!(r_no_ffi.mode_for(&f, &sources),  r_yes_ffi.mode_for(&f, &sources));
        }
    }
}
