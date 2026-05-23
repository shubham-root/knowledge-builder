"""
Main processing pipeline (stub).

In the real implementation this module will:
  1. Detect the file type and delegate to the appropriate extractor.
  2. Segment the extracted content.
  3. Call the LLM to synthesise a markdown note.
  4. Use the atomic writer to place outputs in the vault.

For now it raises ``NotImplementedError`` so that ``__main__`` can surface a
clear, retryable error while the extractors and LLM integration are being built.
"""

from __future__ import annotations

import logging
from pathlib import Path

from .models import OutputEntry, ProcessorInput, ProcessorResultError, ProcessorResultOk

logger = logging.getLogger(__name__)

# Map file extensions to the extractor module name (populated in later tasks).
_EXTENSION_MAP: dict[str, str] = {
    ".pdf": "pdf",
    ".docx": "docx",
    ".xlsx": "xlsx",
    ".pptx": "pptx",
    ".ppt": "pptx",
    ".jpg": "image",
    ".jpeg": "image",
    ".png": "image",
}


def process(input: ProcessorInput) -> ProcessorResultOk | ProcessorResultError:
    """
    Run the full extraction → synthesis → write pipeline for *input*.

    Returns a :class:`~kb_processor.models.ProcessorResultOk` on success or a
    :class:`~kb_processor.models.ProcessorResultError` on failure.  Never raises.

    .. note::
        This is a **stub**.  Replace the body with real implementation once the
        extractors (T35–T37) and LLM pipeline (T38) are in place.
    """
    suffix = Path(input.input_path).suffix.lower()
    extractor_name = _EXTENSION_MAP.get(suffix)

    if extractor_name is None:
        logger.error(
            "Unsupported file extension %r for path %s",
            suffix,
            input.input_path,
        )
        return ProcessorResultError(
            status="error",
            error=f"Unsupported file extension: {suffix!r}",
            retryable=False,
            metadata={"step": "dispatch", "extension": suffix},
        )

    logger.info(
        "Pipeline stub: would dispatch %s to %r extractor (job_id=%d, attempt=%d)",
        input.input_path,
        extractor_name,
        input.job_id,
        input.attempt,
    )

    # ------------------------------------------------------------------
    # TODO (T35–T40): Replace this block with real pipeline execution:
    #
    #   from .extractors import pdf, docx, xlsx, pptx, image
    #   extractor = {
    #       "pdf": pdf.PdfExtractor,
    #       "docx": docx.DocxExtractor,
    #       ...
    #   }[extractor_name](input)
    #   extracted = extractor.extract()
    #   outputs = synthesise_and_write(extracted, input)
    #   return ProcessorResultOk(outputs=outputs, metadata=...)
    # ------------------------------------------------------------------

    return ProcessorResultError(
        status="error",
        error=(
            f"Pipeline not yet implemented for extractor {extractor_name!r}. "
            "This is a stub — real extractors are pending (T35–T40)."
        ),
        retryable=False,
        metadata={"step": "pipeline_stub", "extractor": extractor_name},
    )
