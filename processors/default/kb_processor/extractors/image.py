"""
Image extractor (stub).

Uses ``docling`` for initial image analysis and optionally the ``openai``
vision API for richer LLM-generated descriptions.

TODO (T37):
  - Use ``docling.DocumentConverter`` to process the image file.
  - Optionally call the OpenAI vision API (``gpt-4o-mini``) for a detailed
    natural-language description when ``OPENAI_API_KEY`` is set.
  - Fall back to docling-only output when OpenAI is unavailable.
  - Populate ``metadata`` with image dimensions, format, and model used.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)

_HANDLED_EXTENSIONS = frozenset({".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".tiff", ".tif"})

# The openai package is optional — installed via ``pip install kb-processor[vision]``.
try:
    import openai as _openai  # noqa: F401
    _OPENAI_AVAILABLE = True
except ImportError:
    _OPENAI_AVAILABLE = False


class ImageExtractor(BaseExtractor):
    """
    Extract a description from an image file using ``docling`` and optionally
    the OpenAI vision API.

    Parameters
    ----------
    use_openai:
        If ``True`` (the default) and the ``openai`` package is installed and
        ``OPENAI_API_KEY`` is set, use the OpenAI vision API for a richer
        description.  Falls back to docling-only when not available.
    """

    def __init__(self, *, use_openai: bool = True) -> None:
        self._use_openai = use_openai

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for common image file extensions."""
        return path.suffix.lower() in _HANDLED_EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Extract a natural-language description of the image at *input_path*.

        .. note::
            This is a **stub**.  Replace with a real implementation in T37.

        Raises
        ------
        ExtractionError
            If the image cannot be opened or the API call fails.
        """
        logger.info("ImageExtractor.extract called for %s (stub)", input_path)

        openai_key_present = bool(os.environ.get("OPENAI_API_KEY"))
        use_vision_api = self._use_openai and _OPENAI_AVAILABLE and openai_key_present

        logger.debug(
            "openai_available=%s key_present=%s will_use_vision=%s",
            _OPENAI_AVAILABLE,
            openai_key_present,
            use_vision_api,
        )

        # TODO (T37): Implement real extraction.
        #
        # --- Option A: docling ---
        #   from docling.document_converter import DocumentConverter
        #   converter = DocumentConverter()
        #   result = converter.convert(str(input_path))
        #   content = result.document.export_to_markdown()
        #   return ExtractionResult(content=content, metadata={"extractor": "docling"})
        #
        # --- Option B: OpenAI vision API ---
        #   import base64; from PIL import Image as PILImage; import openai
        #   img_bytes = input_path.read_bytes()
        #   b64 = base64.b64encode(img_bytes).decode()
        #   with PILImage.open(input_path) as img: width, height = img.size; fmt = img.format
        #   client = openai.OpenAI()
        #   response = client.chat.completions.create(
        #       model="gpt-4o-mini",
        #       messages=[{"role":"user","content":[
        #           {"type":"image_url","image_url":{"url":f"data:image/png;base64,{b64}"}},
        #           {"type":"text","text":"Describe this image in detail for an Obsidian note."},
        #       ]}], max_tokens=1024)
        #   description = response.choices[0].message.content
        #   return ExtractionResult(content=description,
        #       metadata={"extractor":"openai_vision","model":"gpt-4o-mini","width":width,"height":height})

        raise ExtractionError(
            f"ImageExtractor is a stub — real implementation pending (T37). "
            f"File: {input_path}",
            retryable=False,
        )
