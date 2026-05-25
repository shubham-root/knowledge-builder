"""
PDF extractor using the ``docling`` library.

``docling`` provides unified OCR + layout analysis for PDF files:

* Native-text PDFs:  text extracted structurally (headings, tables, lists).
* Scanned PDFs:      OCR is applied automatically; no extra configuration needed.
* Mixed PDFs:        both paths are used per page as appropriate.

The extractor converts the PDF to Markdown via
``result.document.export_to_markdown()``, saves any embedded figures/images to
the per-job ``work_dir``, and returns a fully-populated
:class:`~kb_processor.extractors.base.ExtractionResult`.

Edge cases handled
------------------
* **Encrypted / password-protected PDFs** — detected by inspecting the
  exception message from the underlying PDF library; raises
  :class:`~kb_processor.extractors.base.ExtractionError` with
  ``retryable=False``.
* **Corrupted PDFs** — detected similarly; raises ``ExtractionError`` with
  ``retryable=False``.
* **Very large PDFs (>100 pages)** — processed in full; a ``WARNING`` log line
  is emitted so operators can track slow jobs.
* **Scanned PDFs** — handled transparently by docling's built-in OCR.
* **Import error** (docling not installed) — raises ``ExtractionError`` with
  ``retryable=False`` and a helpful install hint.
"""

from __future__ import annotations

import io
import logging
from dataclasses import dataclass
import os
from pathlib import Path
from typing import Any

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)

# Emit a WARNING when a PDF exceeds this many pages (still processed in full).
_LARGE_PDF_PAGE_THRESHOLD = 100

# Keywords that indicate an encrypted / password-protected PDF.
_ENCRYPT_KEYWORDS = frozenset(
    {"encrypt", "password", "protected", "decrypt", "permission", "drm"}
)

# Keywords that indicate a corrupted or structurally invalid PDF.
_CORRUPT_KEYWORDS = frozenset(
    {
        "corrupt",
        "invalid",
        "malformed",
        "bad",
        "broken",
        "unexpected eof",
        "premature",
        "truncated",
        "pdf structure",
        "no /root",
        "no startxref",
        "no xref",
    }
)


def _classify_exception(exc: Exception) -> tuple[str, bool]:
    """
    Map a low-level docling / PDF-library exception to a human-readable
    message and a ``retryable`` flag.

    Returns
    -------
    tuple[str, bool]
        ``(message, retryable)``
    """
    msg_lower = str(exc).lower()

    if any(k in msg_lower for k in _ENCRYPT_KEYWORDS):
        return (
            "PDF is encrypted or password-protected. "
            "Please decrypt the file before processing.",
            False,  # Needs user action — not retryable.
        )

    if any(k in msg_lower for k in _CORRUPT_KEYWORDS):
        return (
            f"PDF appears to be corrupted or structurally invalid. Details: {exc}",
            False,  # File must be replaced — not retryable.
        )

    # Generic conversion failure — may be transient (OOM, temp I/O, …).
    return (
        f"docling failed to convert PDF. Details: {exc}",
        True,
    )


# ---------------------------------------------------------------------------
# Batch sizing & per-batch policy
# ---------------------------------------------------------------------------

#: Default number of pages per docling conversion call.  A small batch keeps
#: per-batch latency bounded (and predictable for ``document_timeout``),
#: lets us adapt the OCR / image policy to each batch's content, and produces
#: streaming progress for long books.  Override with ``KB_PDF_BATCH_SIZE``.
_DEFAULT_BATCH_SIZE: int = 5

#: Per-batch hard timeout (seconds), forwarded to docling's
#: ``PdfPipelineOptions.document_timeout``.  Override with
#: ``KB_PDF_BATCH_TIMEOUT_SECS``.
_DEFAULT_BATCH_TIMEOUT_SECS: int = 300

#: Minimum selectable-text characters per sampled page below which we
#: classify the page as "needs OCR".
_MIN_TEXT_CHARS_PER_PAGE: int = 50


def _batch_size() -> int:
    raw = os.environ.get("KB_PDF_BATCH_SIZE", "").strip()
    try:
        n = int(raw) if raw else _DEFAULT_BATCH_SIZE
    except ValueError:
        return _DEFAULT_BATCH_SIZE
    return max(1, n)


def _batch_timeout_secs() -> int:
    raw = os.environ.get("KB_PDF_BATCH_TIMEOUT_SECS", "").strip()
    try:
        n = int(raw) if raw else _DEFAULT_BATCH_TIMEOUT_SECS
    except ValueError:
        return _DEFAULT_BATCH_TIMEOUT_SECS
    return max(30, n)


def _classify_batch(
    pdf_path: Path,
    page_start_1: int,
    page_end_1: int,
) -> "BatchPolicy":
    """Sample the pages in ``[page_start_1, page_end_1]`` (1-indexed inclusive)
    via pypdfium2 and return the best policy for that batch.

    Policies:

    * ``text_native``  — every page has substantial selectable text.  Skip
                        OCR entirely; docling streams the embedded text.
    * ``scanned``      — pages have little or no selectable text.  Run
                        full OCR pipeline.
    * ``mixed``        — some pages have text, others don't.  Run OCR
                        (safer to over-extract than miss content).

    Falls back to ``scanned`` on any error so callers are never wrong-by-
    omission.
    """
    try:
        import pypdfium2 as pdfium  # noqa: PLC0415
    except ImportError:
        logger.debug("pypdfium2 unavailable; defaulting batch to scanned policy")
        return BatchPolicy(do_ocr=True, kind="scanned")

    try:
        pdf = pdfium.PdfDocument(str(pdf_path))
    except Exception as exc:  # noqa: BLE001
        logger.debug("pypdfium2 open failed for %s: %s", pdf_path, exc)
        return BatchPolicy(do_ocr=True, kind="scanned")

    page_count = len(pdf)
    page_end_1 = min(page_end_1, page_count)

    pages_with_text = 0
    pages_sampled   = 0

    for one_indexed in range(page_start_1, page_end_1 + 1):
        try:
            text = pdf[one_indexed - 1].get_textpage().get_text_range() or ""
        except Exception as exc:  # noqa: BLE001
            logger.debug("text sample failed on page %d: %s", one_indexed, exc)
            continue
        pages_sampled += 1
        if len(text.strip()) >= _MIN_TEXT_CHARS_PER_PAGE:
            pages_with_text += 1

    if pages_sampled == 0:
        return BatchPolicy(do_ocr=True, kind="scanned")
    if pages_with_text == pages_sampled:
        return BatchPolicy(do_ocr=False, kind="text_native")
    if pages_with_text == 0:
        return BatchPolicy(do_ocr=True, kind="scanned")
    return BatchPolicy(do_ocr=True, kind="mixed")


@dataclass(frozen=True)
class BatchPolicy:
    """Per-batch extraction parameters."""
    do_ocr: bool
    kind:   str  # "text_native" | "scanned" | "mixed"


def _page_count(pdf_path: Path) -> int:
    """Return total page count via pypdfium2; 0 on any error."""
    try:
        import pypdfium2 as pdfium  # noqa: PLC0415
        return len(pdfium.PdfDocument(str(pdf_path)))
    except Exception as exc:  # noqa: BLE001
        logger.debug("page-count lookup failed for %s: %s", pdf_path, exc)
        return 0


class PdfExtractor(BaseExtractor):
    """
    Extract text, tables, and figures from a PDF file using ``docling``.

    ``docling`` is imported lazily inside :meth:`extract` so that importing
    this module does not fail if ``docling`` is not installed (the pipeline
    reports a clean error at extraction time rather than at import time).
    """

    # ------------------------------------------------------------------ #
    # Routing                                                              #
    # ------------------------------------------------------------------ #

    def can_handle(self, path: Path) -> bool:
        """
        Return ``True`` for files with a ``.pdf`` extension (case-insensitive).

        Parameters
        ----------
        path:
            Candidate file path.

        Returns
        -------
        bool

        Examples
        --------
        >>> from pathlib import Path
        >>> extractor = PdfExtractor()
        >>> extractor.can_handle(Path("report.pdf"))
        True
        >>> extractor.can_handle(Path("THESIS.PDF"))
        True
        >>> extractor.can_handle(Path("data.xlsx"))
        False
        """
        return path.suffix.lower() == ".pdf"

    # ------------------------------------------------------------------ #
    # Extraction                                                           #
    # ------------------------------------------------------------------ #

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """Extract content from the PDF in 5-page batches.

        For each batch we sample the pages' selectable text via pypdfium2,
        pick a per-batch policy (text-native vs OCR), and call
        ``DocumentConverter.convert(..., page_range=(a, b))`` with
        ``document_timeout`` set to :data:`_DEFAULT_BATCH_TIMEOUT_SECS`.
        Per-batch markdown is concatenated; figures from each batch are
        saved to ``work_dir`` with globally-unique names.

        A batch failure (timeout, conversion error) is logged and *skipped*
        with a placeholder section in the output — it does not abort the
        rest of the document, so a 200-page book is not lost to a single
        bad page.

        Tunables (env vars)
        -------------------
        * ``KB_PDF_BATCH_SIZE``           pages per batch (default 5)
        * ``KB_PDF_BATCH_TIMEOUT_SECS``   per-batch timeout  (default 300)

        Returns
        -------
        ExtractionResult
            ``content``  — concatenated batch markdown.
            ``images``   — figure PNGs saved under *work_dir*.
            ``metadata`` — ``page_count``, ``figure_count``, ``title``,
                          plus a ``batches`` list with per-batch policy +
                          timing for observability.

        Raises
        ------
        ExtractionError
            * ``retryable=False`` — docling not installed or PDF unreadable.
            * ``retryable=True``  — *all* batches failed (the whole document
              is unrecoverable, but a single bad batch will not raise).
        """
        # ── 1. Lazy import ──────────────────────────────────────────── #
        try:
            from ._docling_accel import make_accelerated_converter  # noqa: PLC0415
        except ImportError as exc:
            raise ExtractionError(
                "The \'docling\' package is not installed. "
                "Install it with: pip install docling",
                retryable=False,
            ) from exc

        logger.info("Starting batched PDF extraction: %s", input_path)
        work_dir.mkdir(parents=True, exist_ok=True)

        # ── 2. Determine page count up front ─────────────────────────────── #
        total_pages = _page_count(input_path)
        if total_pages == 0:
            raise ExtractionError(
                f"Could not determine page count for {input_path} — "
                "file may be empty or corrupted.",
                retryable=False,
            )

        batch_size       = _batch_size()
        batch_timeout    = _batch_timeout_secs()
        batch_count      = (total_pages + batch_size - 1) // batch_size

        logger.info(
            "PDF has %d page(s); processing in %d batch(es) of %d (timeout=%ds/batch)",
            total_pages,
            batch_count,
            batch_size,
            batch_timeout,
        )

        # Cache one converter per (do_ocr) policy so we don't rebuild
        # docling's heavy ML pipelines for every batch.
        converters: dict[bool, Any] = {}

        def _converter_for(do_ocr: bool) -> Any:
            if do_ocr not in converters:
                logger.info("Building docling converter for do_ocr=%s", do_ocr)
                converters[do_ocr] = make_accelerated_converter(do_ocr=do_ocr)
            return converters[do_ocr]

        # Apply the per-batch ``document_timeout`` by injecting it into the
        # cached PdfPipelineOptions just-in-time.  ``_build_pdf_pipeline_options``
        # caches by ``do_ocr``; we mutate the returned options object's
        # ``document_timeout`` field which docling reads on every convert().
        from ._docling_accel import _build_pdf_pipeline_options  # noqa: PLC0415

        for do_ocr_setting in (True, False):
            try:
                opts = _build_pdf_pipeline_options(do_ocr=do_ocr_setting)
                if hasattr(opts, "document_timeout"):
                    opts.document_timeout = float(batch_timeout)
            except Exception as exc:  # noqa: BLE001
                logger.debug("Could not set document_timeout for do_ocr=%s: %s",
                             do_ocr_setting, exc)

        # ── 3. Iterate batches ────────────────────────────────────── #
        markdown_parts:    list[str]            = []
        image_paths:       list[Path]           = []
        batch_summaries:   list[dict[str, Any]] = []
        successful_batches = 0
        global_figure_idx  = 0
        merged_title:      str | None           = None
        merged_authors:    Any                  = None

        import time as _time  # noqa: PLC0415

        for batch_no in range(1, batch_count + 1):
            page_start = (batch_no - 1) * batch_size + 1
            page_end   = min(batch_no * batch_size, total_pages)

            policy = _classify_batch(input_path, page_start, page_end)
            converter = _converter_for(policy.do_ocr)

            t0 = _time.perf_counter()
            try:
                conv_result = converter.convert(
                    str(input_path),
                    page_range=(page_start, page_end),
                )
                doc = conv_result.document
            except Exception as exc:  # noqa: BLE001
                elapsed = _time.perf_counter() - t0
                logger.warning(
                    "Batch %d/%d (pages %d-%d, %s) failed after %.1fs: %s",
                    batch_no, batch_count, page_start, page_end, policy.kind, elapsed, exc,
                )
                placeholder = (
                    f"\n\n<!-- batch {batch_no}/{batch_count}: pages "
                    f"{page_start}-{page_end} FAILED ({policy.kind}, "
                    f"{type(exc).__name__}: {exc}) -->\n\n"
                )
                markdown_parts.append(placeholder)
                batch_summaries.append({
                    "batch": batch_no,
                    "pages": [page_start, page_end],
                    "policy": policy.kind,
                    "do_ocr": policy.do_ocr,
                    "ok": False,
                    "elapsed_secs": round(elapsed, 2),
                    "error": f"{type(exc).__name__}: {exc}",
                })
                continue

            # Per-batch markdown.
            try:
                batch_md = doc.export_to_markdown()
            except Exception as exc:  # noqa: BLE001
                logger.warning(
                    "Batch %d markdown export failed: %s", batch_no, exc,
                )
                batch_md = ""

            # Per-batch figures — named globally so they don't collide.
            batch_pictures = getattr(doc, "pictures", []) or []
            for picture in batch_pictures:
                global_figure_idx += 1
                figure_name = f"figure_{global_figure_idx:04d}.png"
                img_path    = work_dir / figure_name
                try:
                    pil_img = picture.get_image(doc)
                    if pil_img is not None:
                        pil_img.save(str(img_path), "PNG")
                        image_paths.append(img_path)
                except Exception as exc:  # noqa: BLE001
                    logger.warning(
                        "Could not save figure %d in batch %d: %s",
                        global_figure_idx, batch_no, exc,
                    )

            # Capture title / authors from the first successful batch only.
            if merged_title is None:
                doc_meta = getattr(doc, "metadata", None)
                if doc_meta is not None:
                    t = getattr(doc_meta, "title", None)
                    if t:
                        merged_title = str(t)
                    a = getattr(doc_meta, "authors", None)
                    if a:
                        merged_authors = (
                            [str(x) for x in a]
                            if hasattr(a, "__iter__") and not isinstance(a, str)
                            else str(a)
                        )

            elapsed = _time.perf_counter() - t0
            successful_batches += 1
            markdown_parts.append(batch_md)

            batch_summaries.append({
                "batch": batch_no,
                "pages": [page_start, page_end],
                "policy": policy.kind,
                "do_ocr": policy.do_ocr,
                "ok": True,
                "elapsed_secs": round(elapsed, 2),
                "chars": len(batch_md),
                "figures": len(batch_pictures),
            })

            # Streaming progress — surfaces in `kb tail` via the daemon's
            # capture of processor stdout.
            print(
                f"[kb-processor] PDF batch {batch_no}/{batch_count} "
                f"pages={page_start}-{page_end} policy={policy.kind} "
                f"ok elapsed={elapsed:.1f}s chars={len(batch_md)} "
                f"figures={len(batch_pictures)}",
                flush=True,
            )

        # ── 4. Bail out only if every batch failed ──────────────────────── #
        if successful_batches == 0:
            raise ExtractionError(
                f"All {batch_count} batch(es) failed; PDF could not be "
                f"extracted. See per-batch errors in metadata.batches.  "
                f"[file: {input_path}]",
                retryable=True,
            )

        markdown_text = "".join(markdown_parts)

        # ── 5. Build metadata ────────────────────────────────────────── #
        metadata: dict[str, Any] = {
            "page_count":         total_pages,
            "figure_count":       len(image_paths),
            "source":             str(input_path),
            "batch_size":         batch_size,
            "batch_count":        batch_count,
            "successful_batches": successful_batches,
            "failed_batches":     batch_count - successful_batches,
            "batches":            batch_summaries,
        }
        if merged_title:
            metadata["title"] = merged_title
        else:
            metadata["title"] = input_path.stem
        if merged_authors:
            metadata["authors"] = merged_authors

        if total_pages > _LARGE_PDF_PAGE_THRESHOLD:
            logger.warning(
                "Large PDF processed: %s had %d pages in %d batch(es), "
                "%d successful.",
                input_path, total_pages, batch_count, successful_batches,
            )

        logger.info(
            "PDF extraction complete: %s — %d chars markdown across "
            "%d/%d successful batch(es), %d figure(s)",
            input_path,
            len(markdown_text),
            successful_batches,
            batch_count,
            len(image_paths),
        )

        return ExtractionResult(
            content=markdown_text,
            images=image_paths,
            metadata=metadata,
        )
