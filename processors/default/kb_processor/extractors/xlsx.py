"""
XLSX extractor (stub).

Uses ``docling`` to extract cell data from Excel workbooks.

TODO (T36):
  - Use ``docling.DocumentConverter`` to convert the XLSX file.
  - Export each sheet as a Markdown table via ``doc.export_to_markdown()``.
  - Emit sheet/table count in ``metadata``.
"""

from __future__ import annotations

import logging
from pathlib import Path

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)

_HANDLED_EXTENSIONS = frozenset({".xlsx", ".xls"})


class XlsxExtractor(BaseExtractor):
    """Extract cell data from an ``.xlsx`` file using ``docling``."""

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for ``.xlsx`` / ``.xls`` files."""
        return path.suffix.lower() in _HANDLED_EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Extract content from the XLSX file at *input_path*.

        .. note::
            This is a **stub**.  Replace with a real ``docling`` implementation in T36.

        Raises
        ------
        ExtractionError
            If the file cannot be opened (e.g. not a valid XLSX archive).
        """
        logger.info("XlsxExtractor.extract called for %s (stub)", input_path)

        # TODO (T36): Implement real extraction using docling.
        #
        #   from docling.document_converter import DocumentConverter
        #   converter = DocumentConverter()
        #   result = converter.convert(str(input_path))
        #   doc = result.document
        #   content = doc.export_to_markdown()
        #   return ExtractionResult(
        #       content=content,
        #       metadata={"table_count": len(list(doc.tables))},
        #   )

        raise ExtractionError(
            f"XlsxExtractor is a stub — real implementation pending (T36). "
            f"File: {input_path}",
            retryable=False,
        )
