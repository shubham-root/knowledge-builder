"""
PDF extractor (stub).

Uses ``docling`` to extract text and embedded images from PDF files.
docling handles both native-text PDFs and scanned pages via its built-in OCR.

TODO (T35):
  - Use ``docling.DocumentConverter`` to convert the PDF.
  - Export the resulting document to Markdown (``doc.export_to_markdown()``).
  - Save embedded figures to ``work_dir`` and populate ``ExtractionResult.images``.
  - Populate ``metadata`` with page count, title, and author.
"""

from __future__ import annotations

import logging
from pathlib import Path

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)

_HANDLED_EXTENSIONS = frozenset({".pdf"})


class PdfExtractor(BaseExtractor):
    """Extract text and figures from a PDF file using ``docling``."""

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for ``.pdf`` files."""
        return path.suffix.lower() in _HANDLED_EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Extract content from *input_path*.

        .. note::
            This is a **stub**.  Replace with a real ``docling`` implementation in T35.

        Raises
        ------
        ExtractionError
            If the file cannot be opened (e.g. encrypted, corrupt).
        """
        logger.info("PdfExtractor.extract called for %s (stub)", input_path)

        # TODO (T35): Implement real extraction using docling.
        #
        #   from docling.document_converter import DocumentConverter
        #   converter = DocumentConverter()
        #   result = converter.convert(str(input_path))
        #   doc = result.document
        #   content = doc.export_to_markdown()
        #   images = []
        #   for idx, picture in enumerate(doc.pictures):
        #       img_path = work_dir / f"page_img_{idx + 1}.png"
        #       picture.get_image(doc).save(img_path)
        #       images.append(img_path)
        #   return ExtractionResult(
        #       content=content,
        #       images=images,
        #       metadata={"page_count": len(doc.pages)},
        #   )

        raise ExtractionError(
            f"PdfExtractor is a stub — real implementation pending (T35). "
            f"File: {input_path}",
            retryable=False,
        )
