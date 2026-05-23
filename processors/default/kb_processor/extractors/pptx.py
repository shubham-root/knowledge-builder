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

* ``content`` — full Markdown representation of the presentation.
* ``images``  — list of ``Path`` objects for PNG images saved to *work_dir*
                (``slide_img_NNN.png``).
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
          ],
      }

.. note::
   Per-slide decomposition relies on parsing the exported Markdown for heading
   boundaries.  If docling's output changes its heading format the fallback is
   a single entry covering the whole presentation.
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
# Module-level helpers
# ---------------------------------------------------------------------------

def _import_docling() -> Any:
    """Return ``DocumentConverter`` class or raise :class:`ExtractionError`."""
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
        status_str = str(result.status).lower()
        if "fail" in status_str:
            raise ExtractionError(
                f"docling conversion failed for '{path}': status={result.status}",
                retryable=False,
            )
    except AttributeError:
        pass


def _is_transient_error(message: str) -> bool:
    """Classify I/O errors as transient vs. format errors as permanent."""
    msg_lower = message.lower()
    permanent_hints = ("password", "encrypt", "protected", "corrupt",
                       "not a valid", "invalid", "unsupported format")
    return not any(hint in msg_lower for hint in permanent_hints)


def _save_images(doc: Any, prefix: str, work_dir: Path) -> list[Path]:
    """
    Extract embedded images from a docling ``DoclingDocument`` and save them
    to *work_dir*.  Returns a list of saved :class:`~pathlib.Path` objects.
    """
    saved: list[Path] = []
    try:
        pictures = list(doc.pictures)
    except Exception as exc:  # noqa: BLE001
        logger.warning("Could not iterate document pictures: %s", exc)
        return saved

    work_dir.mkdir(parents=True, exist_ok=True)
    for idx, picture in enumerate(pictures):
        out_path = work_dir / f"{prefix}_{idx + 1:03d}.png"
        try:
            pil_img = picture.get_image(doc)
            if pil_img is None:
                logger.debug("Picture %d returned None image; skipping.", idx)
                continue
            buf = BytesIO()
            pil_img.save(buf, format="PNG")
            out_path.write_bytes(buf.getvalue())
            saved.append(out_path)
            logger.debug("Saved image '%s'.", out_path.name)
        except Exception as exc:  # noqa: BLE001
            logger.warning("Could not extract image %d: %s", idx, exc)

    return saved


def _slide_count_from_doc(doc: Any) -> int:
    """Return the slide count from ``doc.pages`` (best-effort; 0 on failure)."""
    try:
        return len(doc.pages)
    except Exception:  # noqa: BLE001
        return 0


def _parse_slides_from_markdown(markdown: str) -> list[dict[str, Any]]:
    """
    Split a presentation's Markdown export into per-slide dictionaries.

    docling renders slide titles as ``# Title`` headings.  This function
    splits on top-level headings (single ``#``).

    Each entry: ``{"number": int, "title": str, "content": str, "images": list[str]}``

    If no top-level headings are found, returns a single entry covering the
    whole text.
    """
    # Split keeping the delimiter at the start of each chunk (look-ahead).
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
            # Content before the first heading (e.g. presentation-level title).
            slide_num += 1
            slides.append({
                "number": slide_num,
                "title": "",
                "content": part,
                "images": [],
            })

    # Fallback: no headings found — single entry for the whole presentation.
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
    """

    #: File extensions handled by this extractor.
    EXTENSIONS: frozenset[str] = frozenset({".pptx", ".ppt"})

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for ``.pptx`` / ``.ppt`` files."""
        return path.suffix.lower() in self.EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Convert the PPTX file at *input_path* to Markdown.

        Parameters
        ----------
        input_path:
            Absolute path to the ``.pptx`` source file.
        work_dir:
            Per-job working directory.  Extracted images are saved here as
            ``slide_img_NNN.png`` files.

        Returns
        -------
        ExtractionResult
            ``content`` is the full Markdown; ``images`` is a list of saved
            PNG paths; ``metadata`` has ``slide_count``, ``image_count``, and
            a ``slides`` list with per-slide breakdown.

        Raises
        ------
        ExtractionError
            * ``retryable=False`` — corrupt/encrypted file or docling not installed.
            * ``retryable=True``  — transient I/O error.
        """
        DocumentConverter = _import_docling()  # noqa: N806
        logger.info("PptxExtractor: converting '%s'", input_path)

        try:
            converter = DocumentConverter()
            result = converter.convert(str(input_path))
        except ExtractionError:
            raise
        except Exception as exc:  # noqa: BLE001
            msg = str(exc)
            raise ExtractionError(
                f"docling failed to convert PPTX '{input_path}': {msg}",
                retryable=_is_transient_error(msg),
            ) from exc

        _check_conversion_status(result, input_path)

        try:
            doc = result.document
            markdown: str = doc.export_to_markdown()
        except Exception as exc:  # noqa: BLE001
            raise ExtractionError(
                f"Failed to export PPTX '{input_path}' to Markdown: {exc}",
                retryable=False,
            ) from exc

        # Extract embedded images (save to work_dir)
        images = _save_images(doc, prefix="slide_img", work_dir=work_dir)

        # Build structured slide metadata
        slide_count = _slide_count_from_doc(doc)
        slides = _parse_slides_from_markdown(markdown)

        # Distribute image filenames across slides in document order (best-effort)
        if images and slides:
            n = len(slides)
            per_slide = max(1, len(images) // n)
            for i, slide in enumerate(slides):
                start = i * per_slide
                end = start + per_slide if i < n - 1 else len(images)
                slide["images"] = [p.name for p in images[start:end]]

        if slide_count == 0:
            slide_count = len(slides)

        logger.info(
            "PptxExtractor: done — %d chars, %d slide(s), %d image(s)",
            len(markdown), slide_count, len(images),
        )
        return ExtractionResult(
            content=markdown,
            images=images,
            metadata={
                "slide_count": slide_count,
                "image_count": len(images),
                "slides": slides,
            },
        )
