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
        """
        Extract content from the PDF at *input_path*.

        Steps
        -----
        1. Import ``docling`` (lazy — fails cleanly if not installed).
        2. Run ``DocumentConverter().convert(input_path)`` to obtain a
           ``DoclingDocument``.
        3. Warn if the document has more than
           :data:`_LARGE_PDF_PAGE_THRESHOLD` pages.
        4. Export the document to Markdown via ``export_to_markdown()``.
        5. Iterate ``doc.pictures`` and save each figure to *work_dir*.
        6. Build a ``metadata`` dict (page count, figure count, title, authors).
        7. Return a populated :class:`~kb_processor.extractors.base.ExtractionResult`.

        Parameters
        ----------
        input_path:
            Absolute path to the source ``.pdf`` file.
        work_dir:
            Per-job working directory.  Extracted figure images are written
            here as ``figure_001.png``, ``figure_002.png``, etc.

        Returns
        -------
        ExtractionResult
            ``content`` is the full Markdown text.
            ``images`` is a list of :class:`~pathlib.Path` objects pointing to
            saved figure PNG files inside *work_dir*.
            ``metadata`` contains ``page_count``, ``figure_count``, ``title``,
            and optionally ``authors``.

        Raises
        ------
        ExtractionError
            * ``retryable=False`` — encrypted, password-protected, or
              irreparably corrupted PDF, or ``docling`` not installed.
            * ``retryable=True``  — transient I/O / OOM / unexpected docling
              failure, or Markdown export failure.
        """
        # ── 1. Lazy import ────────────────────────────────────────────── #
        try:
            from docling.document_converter import DocumentConverter  # noqa: PLC0415
        except ImportError as exc:
            raise ExtractionError(
                "The 'docling' package is not installed. "
                "Install it with: pip install docling",
                retryable=False,
            ) from exc

        logger.info("Starting PDF extraction: %s", input_path)

        # ── 2. Convert with docling ───────────────────────────────────── #
        try:
            converter = DocumentConverter()
            result = converter.convert(str(input_path))
        except Exception as exc:  # noqa: BLE001
            human_msg, retryable = _classify_exception(exc)
            raise ExtractionError(
                f"{human_msg}  [file: {input_path}]",
                retryable=retryable,
            ) from exc

        doc = result.document

        # ── 3. Page count & large-PDF warning ────────────────────────── #
        pages = getattr(doc, "pages", None) or {}
        # doc.pages is a dict keyed by integer page number in docling v2.
        page_count: int = len(pages) if isinstance(pages, dict) else int(pages or 0)

        if page_count > _LARGE_PDF_PAGE_THRESHOLD:
            logger.warning(
                "Large PDF detected: %s has %d pages "
                "(threshold=%d). Processing all pages — this may be slow.",
                input_path,
                page_count,
                _LARGE_PDF_PAGE_THRESHOLD,
            )
        else:
            logger.info("PDF has %d page(s): %s", page_count, input_path)

        # ── 4. Export to Markdown ─────────────────────────────────────── #
        try:
            markdown_text: str = doc.export_to_markdown()
        except Exception as exc:  # noqa: BLE001
            raise ExtractionError(
                f"docling failed to export PDF to Markdown. Details: {exc}  "
                f"[file: {input_path}]",
                retryable=True,
            ) from exc

        # ── 5. Extract figures / pictures ─────────────────────────────── #
        work_dir.mkdir(parents=True, exist_ok=True)
        image_paths: list[Path] = []

        pictures = getattr(doc, "pictures", []) or []
        for idx, picture in enumerate(pictures):
            figure_name = f"figure_{idx + 1:03d}.png"
            img_path = work_dir / figure_name
            try:
                pil_img = picture.get_image(doc)
                if pil_img is None:
                    logger.debug(
                        "Picture %d in %s returned no image data — skipping.",
                        idx + 1,
                        input_path,
                    )
                    continue

                # Save PNG to work_dir.
                pil_img.save(str(img_path), "PNG")
                image_paths.append(img_path)
                logger.debug("Saved figure %d → %s", idx + 1, img_path)

            except Exception as exc:  # noqa: BLE001
                logger.warning(
                    "Could not extract figure %d from %s: %s",
                    idx + 1,
                    input_path,
                    exc,
                )

        # ── 6. Build metadata ─────────────────────────────────────────── #
        metadata: dict[str, Any] = {
            "page_count": page_count,
            "figure_count": len(image_paths),
            "source": str(input_path),
        }

        # Pull title / authors from docling's DocumentMetadata (if present).
        try:
            doc_meta = getattr(doc, "metadata", None)
            if doc_meta is not None:
                title = getattr(doc_meta, "title", None)
                if title:
                    metadata["title"] = str(title)

                authors = getattr(doc_meta, "authors", None)
                if authors:
                    # Normalise to a plain list of strings.
                    metadata["authors"] = (
                        [str(a) for a in authors]
                        if hasattr(authors, "__iter__")
                        and not isinstance(authors, str)
                        else str(authors)
                    )
        except Exception as exc:  # noqa: BLE001
            logger.debug("Could not read docling document metadata: %s", exc)

        # Fall back to doc.name or the file stem when no title was found.
        if "title" not in metadata:
            doc_name: str | None = getattr(doc, "name", None)
            metadata["title"] = doc_name if doc_name else input_path.stem

        # ── 7. Return result ──────────────────────────────────────────── #
        logger.info(
            "PDF extraction complete: %s — %d chars markdown, %d figure(s)",
            input_path,
            len(markdown_text),
            len(image_paths),
        )

        return ExtractionResult(
            content=markdown_text,
            images=image_paths,
            metadata=metadata,
        )
