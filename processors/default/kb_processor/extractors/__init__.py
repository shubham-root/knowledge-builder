"""
Extractors sub-package.

Each extractor module exposes a single class that inherits from
:class:`~kb_processor.extractors.base.BaseExtractor` and implements the
:meth:`~kb_processor.extractors.base.BaseExtractor.extract` method.

Available extractors
--------------------
- :mod:`kb_processor.extractors.pdf`   — PDF text and figure extraction
- :mod:`kb_processor.extractors.docx`  — DOCX (Word) text extraction
- :mod:`kb_processor.extractors.xlsx`  — XLSX (Excel) text extraction
- :mod:`kb_processor.extractors.pptx`  — PPTX (PowerPoint) text extraction
- :mod:`kb_processor.extractors.image` — Image description via vision LLM
"""

from .base import BaseExtractor, ExtractionError, ExtractionResult
from .pdf import PdfExtractor

__all__ = ["BaseExtractor", "ExtractionError", "ExtractionResult", "PdfExtractor"]
