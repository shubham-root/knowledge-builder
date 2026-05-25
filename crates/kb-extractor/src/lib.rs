//! Document → Markdown extraction for Knowledge Builder.
//!
//! This crate is the Rust replacement for the Python `kb_processor.extractors`
//! package.  It wraps the [`transmutation`] crate (a pure-Rust document
//! conversion engine) and presents an API tailored to the kb pipeline:
//!
//! * One entry point — [`Extractor::extract`] — that takes a source file
//!   path and a per-job working directory and returns an [`Extraction`].
//! * Three precision tiers — [`ExtractionMode::Fast`],
//!   [`ExtractionMode::Precision`], and (opt-in via the `docling-ffi` cargo
//!   feature) [`ExtractionMode::Ffi`].  The mode is resolved per-file by the
//!   caller from the operator's `[extraction]` config.
//! * Errors classified as retryable (transient I/O, transient
//!   transmutation crashes) vs. permanent (encrypted PDF, corrupted
//!   document) so the daemon's worker pool can apply the right retry
//!   policy.
//!
//! No Python interpreter is involved.  The crate compiles into the `kb`
//! binary with no external runtime dependencies beyond:
//!
//! * `pdftoppm` (poppler) — only when the daemon decides to render
//!   per-page images.  Not used for default text-only extraction.
//! * `tesseract` — only for image OCR (JPG/PNG/TIFF/BMP/GIF/WEBP inputs).
//!
//! # Quick example
//!
//! ```no_run
//! use kb_extractor::{Extractor, ExtractorConfig, ExtractionMode};
//! use std::path::Path;
//!
//! # async fn run() -> anyhow::Result<()> {
//! let extractor = Extractor::new(ExtractorConfig::default())?;
//! let result = extractor
//!     .extract(Path::new("paper.pdf"), Path::new("/tmp/job-42"))
//!     .await?;
//! println!("{}", result.markdown);
//! # Ok(())
//! # }
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod errors;
mod modes;

pub use errors::{ExtractionError, Result};
pub use kb_core::config::ExtractionMode;
pub use modes::ExtractionModeResolver;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

// ── Public types ─────────────────────────────────────────────────────────────

/// Per-extractor configuration handed in by the daemon at startup.
///
/// Default = fast mode, no per-page image rendering.  The caller layers
/// per-file mode on top of this via [`Extractor::extract_with_mode`].
#[derive(Debug, Clone)]
pub struct ExtractorConfig {
    /// Mode to use when the caller does not request a per-file override.
    /// Mirrors [`kb_core::config::ExtractionConfig::default_mode`].
    pub default_mode: ExtractionMode,

    /// OCR language tag(s) for image inputs, in tesseract format
    /// (e.g. `"eng"`, `"eng+por"`).  Defaults to `"eng"`.
    pub ocr_language: String,

    /// When `true`, also emit one PNG per page next to the markdown so
    /// the agent can include them in notes.  Off by default — costs
    /// ~5 s/15 pages on a 2024 MBP and the agent rarely needs them.
    pub render_page_images: bool,
}

impl Default for ExtractorConfig {
    fn default() -> Self {
        Self {
            default_mode:        ExtractionMode::default(),
            ocr_language:        "eng".to_string(),
            render_page_images:  false,
        }
    }
}

/// One successful extraction.
///
/// All paths in [`Self::images`] are absolute and live under the caller's
/// `work_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extraction {
    /// The full markdown content of the document, page-concatenated.
    pub markdown: String,

    /// Image assets emitted to `work_dir` (currently: per-page renders
    /// when [`ExtractorConfig::render_page_images`] is on; empty
    /// otherwise).  Sorted by page number.
    pub images: Vec<PathBuf>,

    /// Structured metadata mirrored from [`transmutation::DocumentMetadata`]
    /// plus our own fields.
    pub metadata: ExtractionMetadata,
}

/// Structured metadata that downstream stages (agent prompt, audit log,
/// `kb show`) consume.  Optional fields are `None` when transmutation
/// didn't surface them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionMetadata {
    /// Source file (absolute, canonicalized).
    pub source: PathBuf,

    /// Detected input format as a stable lower-case tag (`"pdf"`, `"docx"`,
    /// `"xlsx"`, `"pptx"`, `"png"`, `"jpg"`, …).
    pub format: String,

    /// Number of pages (PDF) or slides (PPTX) or sheets (XLSX), best
    /// effort.  `0` for single-asset inputs (txt, md, single image).
    pub page_count: usize,

    /// Document title, when surfaced by transmutation's metadata
    /// extractor.
    pub title: Option<String>,

    /// First-author or sole author, when surfaced.
    pub author: Option<String>,

    /// Mode actually used (after applying any opt-in feature gates).
    pub mode: ExtractionMode,

    /// Wall-clock duration of the conversion.
    pub elapsed: Duration,

    /// Output size in bytes (length of the encoded markdown).
    pub markdown_bytes: u64,
}

// ── Public driver ────────────────────────────────────────────────────────────

/// Stateless extractor handle.  Cheap to construct.
#[derive(Debug)]
pub struct Extractor {
    config: ExtractorConfig,
    inner:  transmutation::Converter,
}

impl Extractor {
    /// Build a new extractor.  Returns an error only if transmutation's own
    /// initialisation fails (which in practice never happens for the
    /// default config).
    pub fn new(config: ExtractorConfig) -> Result<Self> {
        let inner = transmutation::Converter::new()
            .map_err(|e| ExtractionError::Init(e.to_string()))?;
        Ok(Self { config, inner })
    }

    /// Read-only access to the in-effect config, for diagnostics.
    pub fn config(&self) -> &ExtractorConfig { &self.config }

    /// Extract using the configured default mode.
    pub async fn extract(
        &self,
        input:    &Path,
        work_dir: &Path,
    ) -> Result<Extraction> {
        self.extract_with_mode(input, work_dir, self.config.default_mode).await
    }

    /// Extract with a per-call mode override.
    ///
    /// `mode = Ffi` returns [`ExtractionError::FfiNotCompiled`] when the
    /// binary was not built with `--features docling-ffi`.  In that
    /// situation the caller should either (a) fall back to `Precision`
    /// or (b) abort the job with the friendly error already encoded.
    pub async fn extract_with_mode(
        &self,
        input:    &Path,
        work_dir: &Path,
        mode:     ExtractionMode,
    ) -> Result<Extraction> {
        // ── 1. Validate the input path ───────────────────────────────────
        // Canonicalize so the Leptonica OCR path-resolution quirk on
        // macOS (where /tmp/foo can fail to open if /tmp is a symlink to
        // /private/tmp) is sidestepped.  Also asserts existence.
        let input_canon = input.canonicalize().map_err(|e| {
            ExtractionError::SourceUnreadable {
                path:   input.to_path_buf(),
                detail: e.to_string(),
            }
        })?;

        if !input_canon.is_file() {
            return Err(ExtractionError::SourceUnreadable {
                path:   input_canon,
                detail: "not a regular file".into(),
            });
        }

        std::fs::create_dir_all(work_dir).map_err(|e| {
            ExtractionError::WorkDirUnusable {
                path:   work_dir.to_path_buf(),
                detail: e.to_string(),
            }
        })?;

        // ── 2. Gate Ffi mode behind the compile-time feature ─────────────
        if mode == ExtractionMode::Ffi && !cfg!(feature = "docling-ffi") {
            return Err(ExtractionError::FfiNotCompiled);
        }

        // ── 3. Build transmutation options ──────────────────────────────
        let mut opts = transmutation::ConversionOptions::default();
        opts.optimize_for_llm = true;
        opts.split_pages      = false;
        opts.ocr_language     = self.config.ocr_language.clone();
        opts.use_precision_mode = matches!(
            mode,
            ExtractionMode::Precision | ExtractionMode::Ffi,
        );
        opts.use_ffi          = mode == ExtractionMode::Ffi;

        let format = input_format_tag(&input_canon);

        info!(
            target: "kb_extractor",
            "extract: file={} format={} mode={:?}",
            input_canon.display(), format, mode,
        );

        // ── 4. Run the conversion ───────────────────────────────────────
        let started = Instant::now();
        let conv_result = self.inner
            .convert(&input_canon)
            .to(transmutation::OutputFormat::Markdown {
                split_pages:      false,
                optimize_for_llm: true,
            })
            .with_options(opts)
            .execute()
            .await
            .map_err(|e| classify_transmutation_error(&input_canon, e))?;

        let elapsed = started.elapsed();

        // ── 5. Concatenate markdown payloads ────────────────────────────
        // Even with split_pages=false, transmutation still returns a Vec
        // (with one element); we concatenate defensively in case a
        // future version emits >1.
        let mut markdown = String::new();
        for (i, out) in conv_result.content.iter().enumerate() {
            if i > 0 && !markdown.ends_with('\n') {
                markdown.push('\n');
            }
            match std::str::from_utf8(&out.data) {
                Ok(s)  => markdown.push_str(s),
                Err(_) => {
                    warn!(
                        target: "kb_extractor",
                        "non-utf8 page payload from transmutation; skipping page index {}",
                        out.page_number,
                    );
                }
            }
        }

        // Defensive guard: if transmutation reports success but content is
        // empty, we treat that as a transient failure so the daemon retries.
        if markdown.trim().is_empty() {
            return Err(ExtractionError::EmptyOutput {
                path:   input_canon,
                format: format.into(),
            });
        }

        // ── 6. Optional per-page image rendering ────────────────────────
        let images = if self.config.render_page_images {
            self.render_pages(&input_canon, work_dir).await.unwrap_or_else(|e| {
                warn!(
                    target: "kb_extractor",
                    "render_page_images requested but failed: {e} — continuing without images",
                );
                Vec::new()
            })
        } else {
            Vec::new()
        };

        let stats = &conv_result.statistics;
        let markdown_bytes = markdown.len() as u64;
        let page_count = conv_result.metadata.page_count.max(conv_result.content.len());
        debug!(
            target: "kb_extractor",
            "extract done: pages={} bytes={} elapsed={:?}",
            page_count, markdown_bytes, elapsed,
        );
        let _ = stats; // statistics consumed by tracing only for now

        Ok(Extraction {
            markdown,
            images,
            metadata: ExtractionMetadata {
                source:         input_canon,
                format:         format.to_string(),
                page_count,
                title:          conv_result.metadata.title.clone(),
                author:         conv_result.metadata.author.clone(),
                mode,
                elapsed,
                markdown_bytes,
            },
        })
    }

    /// Best-effort per-page PNG rendering.  Used only when
    /// [`ExtractorConfig::render_page_images`] is on.
    ///
    /// Rust transmutation v0.3 emits one PNG per *page* (full-page render),
    /// not per *figure*.  Image inputs (JPG/PNG/...) are skipped here —
    /// they're already images, the agent doesn't need a re-render.
    async fn render_pages(
        &self,
        input:    &Path,
        work_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        let format = input_format_tag(input);
        if !matches!(format, "pdf" | "docx" | "pptx") {
            return Ok(Vec::new());
        }

        let mut opts = transmutation::ConversionOptions::default();
        opts.dpi = 150;
        let res = self.inner
            .convert(input)
            .to(transmutation::OutputFormat::Image {
                format:  transmutation::ImageFormat::Png,
                quality: 85,
                dpi:     150,
            })
            .with_options(opts)
            .execute()
            .await
            .map_err(|e| classify_transmutation_error(input, e))?;

        // Persist each page's bytes to work_dir/page_NNNN.png
        let mut out: Vec<PathBuf> = Vec::with_capacity(res.content.len());
        for output in &res.content {
            let n   = output.page_number;
            let dst = work_dir.join(format!("page_{n:04}.png"));
            std::fs::write(&dst, &output.data).map_err(|e| {
                ExtractionError::WorkDirUnusable {
                    path:   dst.clone(),
                    detail: e.to_string(),
                }
            })?;
            out.push(dst);
        }
        out.sort();
        Ok(out)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Map a path's extension to a stable lowercase tag.  Returns
/// `"unknown"` when the extension is missing or unrecognised.  This is
/// used purely for logging and the [`ExtractionMetadata::format`] field
/// — actual conversion routing happens inside transmutation by magic
/// bytes.
fn input_format_tag(path: &Path) -> &'static str {
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None    => return "unknown",
    };
    match ext.as_str() {
        "pdf"                  => "pdf",
        "docx"                 => "docx",
        "xlsx"                 => "xlsx",
        "pptx"                 => "pptx",
        "ppt"                  => "ppt",
        "png"                  => "png",
        "jpg" | "jpeg"         => "jpg",
        "gif"                  => "gif",
        "webp"                 => "webp",
        "bmp"                  => "bmp",
        "tiff" | "tif"         => "tiff",
        "txt"                  => "txt",
        "md"                   => "md",
        "html" | "htm"         => "html",
        "csv"                  => "csv",
        _                      => "unknown",
    }
}

/// Map a [`transmutation::TransmutationError`] (or its source chain) onto
/// our own error taxonomy with the right `retryable` flag.
fn classify_transmutation_error(
    path: &Path,
    err:  transmutation::TransmutationError,
) -> ExtractionError {
    let msg = err.to_string().to_lowercase();

    // Permanent: file is encrypted or corrupted.
    const PERMANENT: &[&str] = &[
        "encrypt", "password", "protected", "decrypt",
        "corrupt", "malformed", "premature", "truncated",
        "unexpected eof", "no /root", "no startxref", "no xref",
        "invalid pdf",
        // transmutation 0.3.x emits this for unparseable PDFs (bad
        // headers, mangled xref, etc.).  Treating it as permanent
        // matches user expectation: a broken file won't get fixed by
        // retrying.
        "failed to load pdf",
        // umya-spreadsheet (xlsx) on a corrupted workbook.
        "invalid excel", "not a valid zip",
        // docx-rs on bad inputs.
        "invalid docx",
    ];
    for needle in PERMANENT {
        if msg.contains(needle) {
            return ExtractionError::Permanent {
                path:   path.to_path_buf(),
                detail: err.to_string(),
            };
        }
    }

    // Permanent: the format isn't supported (likely a feature flag missing).
    if msg.contains("unsupported file format") || msg.contains("format not supported") {
        return ExtractionError::Permanent {
            path:   path.to_path_buf(),
            detail: format!(
                "{err} — hint: rebuild kb with the relevant transmutation \
                 feature enabled (image-ocr, office, pdf-to-image)",
            ),
        };
    }

    ExtractionError::Transient {
        path:   path.to_path_buf(),
        detail: err.to_string(),
    }
}
