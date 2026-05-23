"""
PPTX extractor — uses ``docling`` to convert PowerPoint presentations to Markdown.

docling's ``DocumentConverter`` natively handles ``.pptx`` (and ``.ppt``) files.
Each slide's content is converted to a structured ``DoclingDocument``.  Calling
``doc.export_to_markdown()`` produces Markdown where slide titles appear as
top-level headings (``# Title``) and slide body content follows immediately.
Embedded images are extracted per-slide when the docling document model exposes
them via ``doc.pictures``.

Structured output contract
--------------------------
The :class:`ExtractionResult` returned by :meth:`PptxExtractor.extract` uses:

* ``text`` — full Markdown representation of the presentation.
* ``images`` — list of ``(filename, bytes)`` pairs for embedded images.  Each
  image is also persisted to ``work_dir`` as ``slide_img_NNN.png``.
* ``metadata`` — dict with keys:

  .. code-block:: python

      {
          "slide_count":  int,          # total slide count (from doc.pages)
          "image_count":  int,          # number of extracted images
          "slides": [                   # best-effort per-slide breakdown
              {
                  "number":  int,       # 1-based slide number
                  "title":   str,       # slide title or "" if not detected
                  "content": str,       # Markdown body for this slide
                  "images":  list[str], # filenames of images on this slide
              },
              ...
          ],
      }

.. note::
   Per-slide decomposition relies on parsing the exported Markdown for heading
   boundaries.  If docling's Markdown output changes its heading format the
   fallback is a single entry covering the whole presentation.
"""

from __future__ import annotations

import logging
import re
from io import BytesIO
from pathlib import Path
from typing import Any

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Internal helpers (shared with docx.py and xlsx.py but duplicated here to
# keep each extractor module self-contained and independently importable)
# ---------------------------------------------------------------------------

def _import_docling() -> Any:
    """Return ``DocumentConverter`` or raise a clear :class:`ExtractionError`."""
    try:
        from docling.document_converter import DocumentConverter  # type: ignore[import]
        return DocumentConverter
    except ImportError as exc:
        raise ExtractionError(
            "docling is not installed — run: pip install 'docling>=2.0.0'",
            retryable=False,
        ) from exc


def _check_conversion_status(result: Any, path: Path) -> None:
    """Raise :class:`ExtractionError` if docling reports a hard failure."""
    try:
        status = result.status
        status_str = str(status).lower()
        if "fail" in status_str:
            raise ExtractionError(
                f"docling conversion failed for '{path}': status={status}",
                retryable=False,
            )
    except AttributeError:
        pass  # Older docling — no .status; proceed optimistically.


def _is_transient_error(message: str) -> bool:
    """Heuristic: permanent format errors vs. transient I/O errors."""
    msg_lower = message.lower()
    permanent_hints = ("password", "encrypt", "protected", "corrupt",
                       "not a valid", "invalid", "unsupported format")
    return not any(hint in msg_lower for hint in permanent_hints)


def _extract_images(doc: Any, prefix: str, work_dir: Path) -> list[tuple[str, bytes]]:
    """
    Extract embedded images from a docling ``DoclingDocument``.

    Saves each image as ``<prefix>_NNN.png`` inside *work_dir* and returns
    ``(filename, bytes)`` pairs.  Per-image failures are logged at ``WARNING``
    and skipped so a single corrupt image cannot abort extraction.
    """
    images: list[tuple[str, bytes]] = []
    try:
        pictures = list(doc.pictures)
    except Exception as exc:  # noqa: BLE001
        logger.warning("Could not iterate document pictures: %s", exc)
        return images

    work_dir.mkdir(parents=True, exist_ok=True)

    for idx, picture in enumerate(pictures):
        filename = f"{prefix}_{idx + 1:03d}.png"
        try:
            pil_img = picture.get_image(doc)
            if pil_img is None:
                logger.debug("Picture %d returned None image; skipping.", idx)
                continue

            buf = BytesIO()
            pil_img.save(buf, format="PNG")
            img_bytes = buf.getvalue()

            (work_dir / filename).write_bytes(img_bytes)
            images.append((filename, img_bytes))
            logger.debug("Extracted image '%s' (%d bytes).", filename, len(img_bytes))
        except Exception as exc:  # noqa: BLE001
            logger.warning("Could not extract image %d from document: %s", idx, exc)

    return images


# ---------------------------------------------------------------------------
# Slide decomposition from Markdown
# ---------------------------------------------------------------------------

def _slide_count_from_doc(doc: Any) -> int:
    """Return the slide count from ``doc.pages`` (best-effort; 0 on failure)."""
    try:
        return len(doc.pages)
    except Exception:  # noqa: BLE001
        return 0


def _parse_slides_from_markdown(markdown: str, total_images: int) -> list[dict[str, Any]]:
    """
    Split a presentation's Markdown export into per-slide dictionaries.

    docling renders slide titles as ``# Title`` headings and separates slide
    body with blank lines.  This function splits on top-level headings.

    Each entry:
    .. code-block:: python

        {"number": int, "title": str, "content": str, "images": list[str]}

    *images* is always an empty list here; callers that track per-slide images
    should populate it separately (full-doc image attribution to a specific slide
    is not possible without docling's internal page-reference data).

    If no top-level headings are found, returns a single entry with the full text.
    """
    # Split on lines that start with a single '#' (top-level heading).
    # Use a look-ahead so the heading line itself is kept at the start of each chunk.
    parts = re.split(r"(?m)^(?=#[^#])", markdown)

    slides: list[dict[str, Any]] = []
    slide_num = 0

    for part in parts:
        part = part.strip()
        if not part:
            continue

        if part.startswith("# "):
            slide_num += 1
            lines = part.split("\n", 1)
            title = lines[0].lstrip("# ").strip()
            body = lines[1].strip() if len(lines) > 1 else ""
            slides.append({
                "number": slide_num,
                "title": title,
                "content": body,
                "images": [],
            })
        else:
            # Content before the first heading (e.g. presentation title block).
            slide_num += 1
            slides.append({
                "number": slide_num,
                "title": "",
                "content": part,
                "images": [],
            })

    # Fallback: no structure found — single entry for the whole presentation.
    if not slides and markdown.strip():
        slides.append({
            "number": 1,
            "title": "",
            "content": markdown.strip(),
            "images": [],
        })

    return slides


# ---------------------------------------------------------------------------
# Public extractor class
# ---------------------------------------------------------------------------

class PptxExtractor(BaseExtractor):
    """
    Extract text and embedded images from a ``.pptx`` (or ``.ppt``) file
    using ``docling``.

    docling converts the entire presentation to a ``DoclingDocument`` where
    each slide maps to a top-level Markdown heading followed by its body
    content.  Embedded images are extracted and saved to :attr:`work_dir`.

    Parameters
    ----------
    path:
        Absolute path to the ``.pptx`` source file.
    work_dir:
        Per-job working directory.  Extracted images are written here as
        ``slide_img_NNN.png`` files.
    """

    EXTENSIONS: frozenset[str] = frozenset({".pptx", ".ppt"})

    @classmethod
    def can_handle(cls, path: Path) -> bool:
        """Return ``True`` for ``.pptx`` / ``.ppt`` files."""
        return path.suffix.lower() in cls.EXTENSIONS

    def extract(self) -> ExtractionResult:
        """
        Convert the PPTX file at :attr:`path` to Markdown.

        All embedded images are extracted, saved to :attr:`work_dir`, and
        included in the returned :class:`ExtractionResult`.

        Returns
        -------
        ExtractionResult
            ``text`` is the full Markdown;
            ``images`` contains ``(filename, bytes)`` pairs;
            ``metadata`` has ``slide_count``, ``image_count``, and a
            ``slides`` list with per-slide breakdown.

        Raises
        ------
        ExtractionError
            * ``retryable=False`` — corrupt/encrypted file, unsupported
              variant, or docling not installed.
            * ``retryable=True``  — transient I/O error during conversion.
        """
        DocumentConverter = _import_docling()  # noqa: N806

        logger.info("PptxExtractor: converting '%s'", self.path)

        # --- Run docling conversion -------------------------------------------
        try:
            converter = DocumentConverter()
            result = converter.convert(str(self.path))
        except ExtractionError:
            raise
        except Exception as exc:  # noqa: BLE001
            error_msg = str(exc)
            raise ExtractionError(
                f"docling failed to convert PPTX '{self.path}': {error_msg}",
                retryable=_is_transient_error(error_msg),
            ) from exc

        _check_conversion_status(result, self.path)

        # --- Export to Markdown -----------------------------------------------
        try:
            doc = result.document
            markdown: str = doc.export_to_markdown()
        except Exception as exc:  # noqa: BLE001
            raise ExtractionError(
                f"Failed to export PPTX '{self.path}' to Markdown: {exc}",
                retryable=False,
            ) from exc

        # --- Extract embedded images ------------------------------------------
        images = _extract_images(doc, prefix="slide_img", work_dir=Path(self.work_dir))

        # --- Build structured slide metadata ----------------------------------
        slide_count = _slide_count_from_doc(doc)
        slides = _parse_slides_from_markdown(markdown, total_images=len(images))

        # Annotate slides with the image filenames extracted (approximate —
        # docling does not expose which image belongs to which slide).
        if images and slides:
            # Distribute images across slides in document order as best-effort.
            images_per_slide = max(1, len(images) // len(slides))
            for i, slide in enumerate(slides):
                start = i * images_per_slide
                end = start + images_per_slide if i < len(slides) - 1 else len(images)
                slide["images"] = [fname for fname, _ in images[start:end]]

        # Use docling's page count when it differs from parsed heading count
        # (the Markdown parser may under- or over-count).
        if slide_count == 0:
            slide_count = len(slides)

        logger.info(
            "PptxExtractor: done — %d chars, %d slide(s), %d image(s)",
            len(markdown),
            slide_count,
            len(images),
        )
        return ExtractionResult(
            text=markdown,
            images=images,
            metadata={
                "slide_count": slide_count,
                "image_count": len(images),
                "slides": slides,
            },
        )
