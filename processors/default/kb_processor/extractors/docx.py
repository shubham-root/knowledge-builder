"""
DOCX extractor — uses ``docling`` to convert Word documents to Markdown.

docling's ``DocumentConverter`` natively handles ``.docx`` files, preserving
heading hierarchy, bullet lists, numbered lists, tables, and embedded images
with no additional dependencies beyond the single ``docling`` package.

Structured output contract
--------------------------
The :class:`ExtractionResult` returned by :meth:`DocxExtractor.extract` uses:

* ``text`` — full Markdown representation of the document.
* ``images`` — list of ``(filename, bytes)`` pairs for embedded images;
  each image is also saved as a ``.png`` file inside ``work_dir``.
* ``metadata`` — dict with keys:

  .. code-block:: python

      {
          "paragraph_count": int,        # approximate paragraph count
          "image_count":     int,        # number of embedded images extracted
          "title":           str | None, # document title if available
      }
"""

from __future__ import annotations

import logging
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
        pass  # Older docling without .status — proceed optimistically.


def _is_transient_error(message: str) -> bool:
    """Heuristic: classify I/O errors as transient vs. format errors as permanent."""
    msg_lower = message.lower()
    permanent_hints = ("password", "encrypt", "protected", "corrupt",
                       "not a valid", "invalid", "unsupported format")
    return not any(hint in msg_lower for hint in permanent_hints)


def _extract_images(doc: Any, prefix: str, work_dir: Path) -> list[tuple[str, bytes]]:
    """
    Extract embedded images from a docling ``DoclingDocument``.

    Saves each as ``<prefix>_NNN.png`` inside *work_dir* and returns
    ``(filename, bytes)`` pairs.  Per-image failures are logged and skipped.
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
                continue
            buf = BytesIO()
            pil_img.save(buf, format="PNG")
            img_bytes = buf.getvalue()
            (work_dir / filename).write_bytes(img_bytes)
            images.append((filename, img_bytes))
            logger.debug("Extracted image '%s' (%d bytes).", filename, len(img_bytes))
        except Exception as exc:  # noqa: BLE001
            logger.warning("Could not extract image %d: %s", idx, exc)

    return images


# ---------------------------------------------------------------------------
# Public extractor class
# ---------------------------------------------------------------------------

class DocxExtractor(BaseExtractor):
    """
    Extract text and embedded images from a ``.docx`` file using ``docling``.

    Parameters
    ----------
    path:
        Absolute path to the ``.docx`` source file.
    work_dir:
        Per-job working directory.  Extracted images are written here as
        ``doc_img_NNN.png`` files.
    """

    #: File extensions handled by this extractor.
    EXTENSIONS: frozenset[str] = frozenset({".docx"})

    @classmethod
    def can_handle(cls, path: Path) -> bool:
        """Return ``True`` for ``.docx`` files."""
        return path.suffix.lower() in cls.EXTENSIONS

    def extract(self) -> ExtractionResult:
        """
        Convert the DOCX file at :attr:`path` to Markdown.

        Returns
        -------
        ExtractionResult
            ``text`` is the full Markdown; ``images`` contains
            ``(filename, bytes)`` pairs; ``metadata`` has
            ``paragraph_count``, ``image_count``, and optionally ``title``.

        Raises
        ------
        ExtractionError
            * ``retryable=False`` — corrupt/encrypted file or docling not installed.
            * ``retryable=True``  — transient I/O error.
        """
        DocumentConverter = _import_docling()  # noqa: N806
        logger.info("DocxExtractor: converting '%s'", self.path)

        try:
            converter = DocumentConverter()
            result = converter.convert(str(self.path))
        except ExtractionError:
            raise
        except Exception as exc:  # noqa: BLE001
            msg = str(exc)
            raise ExtractionError(
                f"docling failed to convert DOCX '{self.path}': {msg}",
                retryable=_is_transient_error(msg),
            ) from exc

        _check_conversion_status(result, self.path)

        try:
            doc = result.document
            markdown: str = doc.export_to_markdown()
        except Exception as exc:  # noqa: BLE001
            raise ExtractionError(
                f"Failed to export DOCX '{self.path}' to Markdown: {exc}",
                retryable=False,
            ) from exc

        images = _extract_images(doc, prefix="doc_img", work_dir=Path(self.work_dir))

        paragraph_count = 0
        try:
            paragraph_count = sum(1 for _ in doc.texts)
        except Exception:  # noqa: BLE001
            pass

        metadata: dict[str, Any] = {
            "paragraph_count": paragraph_count,
            "image_count": len(images),
        }
        try:
            if doc.name:
                metadata["title"] = str(doc.name)
        except Exception:  # noqa: BLE001
            pass

        logger.info(
            "DocxExtractor: done — %d chars, %d image(s), %d paragraph(s)",
            len(markdown), len(images), paragraph_count,
        )
        return ExtractionResult(text=markdown, images=images, metadata=metadata)
