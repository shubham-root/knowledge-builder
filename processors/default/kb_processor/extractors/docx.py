"""
DOCX extractor (stub).

Uses ``docling`` to extract text from Word documents.

TODO (T36):
  - Use ``docling.DocumentConverter`` to convert the DOCX file.
  - Export the converted document to Markdown preserving heading hierarchy.
  - Save embedded images to ``work_dir`` and populate ``ExtractionResult.images``.
  - Populate ``metadata`` with core properties (paragraph count, title).
"""

from __future__ import annotations

import logging
from pathlib import Path

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)

_HANDLED_EXTENSIONS = frozenset({".docx", ".doc"})


class DocxExtractor(BaseExtractor):
    """Extract text from a ``.docx`` file using ``docling``."""

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for ``.docx`` / ``.doc`` files."""
        return path.suffix.lower() in _HANDLED_EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Extract content from the DOCX file at *input_path*.

        .. note::
            This is a **stub**.  Replace with a real ``docling`` implementation in T36.

        Raises
        ------
        ExtractionError
            If the file cannot be opened (e.g. not a valid DOCX archive).
        """
        logger.info("DocxExtractor.extract called for %s (stub)", input_path)

        # TODO (T36): Implement real extraction using docling.
        #
        #   from docling.document_converter import DocumentConverter
        #   converter = DocumentConverter()
        #   result = converter.convert(str(input_path))
        #   doc = result.document
        #   content = doc.export_to_markdown()
        #   images = []
        #   for idx, picture in enumerate(doc.pictures):
        #       img_path = work_dir / f"doc_img_{idx + 1}.png"
        #       picture.get_image(doc).save(img_path)
        #       images.append(img_path)
        #   return ExtractionResult(
        #       content=content,
        #       images=images,
        #       metadata={"paragraph_count": len(list(doc.texts))},
        #   )

        raise ExtractionError(
            f"DocxExtractor is a stub — real implementation pending (T36). "
            f"File: {input_path}",
            retryable=False,
        )
