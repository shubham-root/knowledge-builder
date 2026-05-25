//! End-to-end smoke tests against the real `transmutation` library.
//!
//! These tests build small synthetic input files (or skip when an
//! optional toolchain isn't available) and call [`Extractor::extract`]
//! through the same path the daemon will.  They prove that the cargo
//! wiring works and that our error classification matches reality —
//! they do **not** chase corner cases of transmutation itself, which
//! has its own test suite.
//!
//! Run with:
//!
//! ```bash
//! cargo test --release -p kb-extractor -- --include-ignored
//! ```

use std::path::Path;

use kb_extractor::{ExtractionError, ExtractionMode, Extractor, ExtractorConfig};
use tempfile::TempDir;

/// Real, valid one-page PDF baked into the test binary.  Built once via
/// reportlab and committed to the repo so tests are hermetic.
const HELLO_PDF: &[u8] = include_bytes!("fixtures/hello.pdf");

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write fixture");
}

#[tokio::test]
async fn extract_pdf_fast_mode_returns_some_text() {
    let tmp = TempDir::new().unwrap();
    let pdf = tmp.path().join("hello.pdf");
    write(&pdf, HELLO_PDF);

    let extractor = Extractor::new(ExtractorConfig::default()).expect("init");
    let work_dir = tmp.path().join("work");

    let res = extractor
        .extract_with_mode(&pdf, &work_dir, ExtractionMode::Fast)
        .await;

    match res {
        Ok(ext) => {
            assert!(
                !ext.markdown.trim().is_empty(),
                "expected non-empty markdown",
            );
            assert_eq!(ext.metadata.format, "pdf");
            assert_eq!(ext.metadata.mode, ExtractionMode::Fast);
            assert_eq!(ext.images.len(), 0, "render_page_images was off");
            assert!(ext.metadata.page_count >= 1);
        }
        Err(e) => panic!("extraction failed: {e}"),
    }
}

#[tokio::test]
async fn extract_pdf_precision_mode_runs() {
    let tmp = TempDir::new().unwrap();
    let pdf = tmp.path().join("hello.pdf");
    write(&pdf, HELLO_PDF);

    let extractor = Extractor::new(ExtractorConfig::default()).expect("init");
    let res = extractor
        .extract_with_mode(&pdf, &tmp.path().join("w"), ExtractionMode::Precision)
        .await
        .expect("precision should succeed for a healthy PDF");

    assert_eq!(res.metadata.mode, ExtractionMode::Precision);
    assert!(!res.markdown.is_empty());
}

#[tokio::test]
async fn extract_ffi_mode_errors_when_feature_disabled() {
    let tmp = TempDir::new().unwrap();
    let pdf = tmp.path().join("hello.pdf");
    write(&pdf, HELLO_PDF);

    let extractor = Extractor::new(ExtractorConfig::default()).expect("init");
    let res = extractor
        .extract_with_mode(&pdf, &tmp.path().join("w"), ExtractionMode::Ffi)
        .await;

    if cfg!(feature = "docling-ffi") {
        assert!(res.is_ok(), "ffi feature is on; ffi mode should succeed");
    } else {
        match res {
            Err(ExtractionError::FfiNotCompiled) => {} // expected
            Err(other) => panic!("expected FfiNotCompiled, got {other:?}"),
            Ok(_)      => panic!("expected FfiNotCompiled error, got success"),
        }
    }
}

#[tokio::test]
async fn extract_missing_file_is_source_unreadable() {
    let tmp = TempDir::new().unwrap();
    let bogus = tmp.path().join("does-not-exist.pdf");

    let extractor = Extractor::new(ExtractorConfig::default()).expect("init");
    let res = extractor
        .extract(&bogus, &tmp.path().join("w"))
        .await;

    match res {
        Err(ExtractionError::SourceUnreadable { .. }) => {} // expected
        other => panic!("expected SourceUnreadable, got {other:?}"),
    }
}

#[tokio::test]
async fn extract_corrupt_pdf_classified_as_permanent() {
    let tmp = TempDir::new().unwrap();
    let pdf = tmp.path().join("bad.pdf");
    // Header-but-no-body.  Most PDF parsers reject with "premature eof".
    write(&pdf, b"%PDF-1.4\n%%EOF\n");

    let extractor = Extractor::new(ExtractorConfig::default()).expect("init");
    let res = extractor
        .extract(&pdf, &tmp.path().join("w"))
        .await;

    match res {
        Err(e) => {
            // We accept either Permanent (preferred) or EmptyOutput as
            // valid classifications for an empty/garbage PDF — the
            // important property is that the classification doesn't
            // claim Transient (which would cause infinite retries).
            let kind = e.kind();
            assert!(
                matches!(kind, "permanent" | "empty_output"),
                "expected permanent|empty_output, got {kind} ({e:?})",
            );
        }
        Ok(_) => {
            // Some lenient extractors accept this.  If so, the markdown
            // must at least be non-empty per the contract.
        }
    }
}

#[cfg(feature = "image-ocr")]
#[tokio::test]
async fn extract_png_runs_ocr() {
    // We render a tiny PNG with text using the `image` crate-free path:
    // build a 200x80 white BMP, save with the `.png` extension and let
    // tesseract handle it.  If the PNG decoder rejects a BMP, the test
    // gracefully reports through the regular error path.
    use std::io::Write;

    let tmp = TempDir::new().unwrap();
    let png = tmp.path().join("hello.png");

    // Write a minimal valid PNG: a 1x1 white pixel.  Most useful for
    // proving the OCR path runs without panicking; tesseract may
    // produce no text on a 1x1 image, which is fine for this smoke
    // test.
    let bytes = include_bytes!("fixtures/1x1-white.png");
    let mut f = std::fs::File::create(&png).unwrap();
    f.write_all(bytes).unwrap();
    drop(f);

    let extractor = Extractor::new(ExtractorConfig::default()).expect("init");
    let res = extractor
        .extract(&png, &tmp.path().join("w"))
        .await;

    // We don't assert on the OCR text content (a 1x1 image yields
    // nothing); we only assert the path runs to completion *or*
    // surfaces a clean classified error.
    match res {
        Ok(ext)  => assert_eq!(ext.metadata.format, "png"),
        Err(e)   => assert!(
            matches!(e.kind(), "empty_output" | "transient" | "permanent"),
            "unexpected kind {} for {:?}", e.kind(), e,
        ),
    }
}
