"""
Image extractor using docling for OCR and optionally the OpenAI vision API.

**Baseline (always available):** :mod:`docling` performs OCR on the image and
extracts any visible text.  No API key or network access required.

**Optional enhancement:** When ``OPENAI_API_KEY`` is set and the ``openai``
package is installed (``pip install kb-processor[vision]``), the image is also
sent to a vision LLM (default: ``gpt-4o-mini``) for a richer semantic
description.  Any API failure is logged as a warning and the result falls back
silently to the docling-only output — it is never treated as an error.

Environment variables
---------------------
OPENAI_API_KEY
    When set, enables the vision-LLM enhancement.
KB_VISION_MODEL
    Override the vision model (default: ``gpt-4o-mini``).
"""

from __future__ import annotations

import base64
import logging
import os
from pathlib import Path
from typing import Any

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)

# Only .jpg/.jpeg/.png are in scope for this extractor (per T37 spec).
_HANDLED_EXTENSIONS: frozenset[str] = frozenset({".jpg", ".jpeg", ".png"})

# ---------------------------------------------------------------------------
# Optional dependency: openai  (pip install kb-processor[vision])
# ---------------------------------------------------------------------------
try:
    import openai as _openai  # noqa: F401

    _OPENAI_AVAILABLE = True
except ImportError:
    _OPENAI_AVAILABLE = False

# ---------------------------------------------------------------------------
# Optional dependency: Pillow  (listed in core deps but guard defensively)
# ---------------------------------------------------------------------------
try:
    from PIL import Image as _PILImage
    from PIL.ExifTags import TAGS as _EXIF_TAGS

    _PIL_AVAILABLE = True
except ImportError:  # pragma: no cover
    _PIL_AVAILABLE = False


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _pillow_metadata(path: Path) -> dict[str, Any]:
    """Return best-effort image metadata from Pillow.

    Never raises — returns an empty dict on any failure so that the main
    extraction path is not interrupted by metadata-collection errors.
    """
    if not _PIL_AVAILABLE:
        return {}
    try:
        with _PILImage.open(path) as img:
            meta: dict[str, Any] = {
                "width": img.size[0],
                "height": img.size[1],
                "format": img.format or path.suffix.lstrip(".").upper(),
                "mode": img.mode,
            }
            # EXIF — Pillow 7+ public API
            try:
                raw_exif = img.getexif()
                if raw_exif:
                    decoded: dict[str, Any] = {}
                    for tag_id, value in raw_exif.items():
                        tag_name = _EXIF_TAGS.get(tag_id, str(tag_id))
                        # Skip binary blobs (MakerNote, UserComment, etc.)
                        if isinstance(value, bytes):
                            continue
                        decoded[tag_name] = value
                    if decoded:
                        meta["exif"] = decoded
            except Exception as exc:  # noqa: BLE001
                logger.debug("EXIF extraction skipped for %s: %s", path, exc)
            return meta
    except Exception as exc:  # noqa: BLE001
        logger.debug("Pillow metadata extraction failed for %s: %s", path, exc)
        return {}


def _docling_ocr(path: Path) -> str:
    """Run docling on *path* and return the exported Markdown text.

    Raises
    ------
    ExtractionError
        On any docling failure (corrupted image, unsupported format, etc.).
        ``retryable`` is ``True`` for transient OS-level I/O errors and
        ``False`` for content-level failures.
    """
    try:
        from docling.document_converter import DocumentConverter  # type: ignore[import]
    except ImportError as exc:
        raise ExtractionError(
            f"docling is not installed — cannot extract image {path}: {exc}",
            retryable=False,
        ) from exc

    try:
        converter = DocumentConverter()
        result = converter.convert(str(path))
        return result.document.export_to_markdown()
    except OSError as exc:
        # Transient filesystem / permissions error — worth retrying.
        raise ExtractionError(
            f"OS error while processing image {path}: {exc}",
            retryable=True,
        ) from exc
    except Exception as exc:  # noqa: BLE001
        # Corrupted image, unsupported encoding, docling internal error, etc.
        raise ExtractionError(
            f"docling failed to process image {path}: {exc}",
            retryable=False,
        ) from exc


def _llm_describe(path: Path, model: str) -> str | None:
    """Send the image to the OpenAI vision API and return the description.

    Returns ``None`` on any error so the caller can fall back to docling-only
    output without raising.

    Parameters
    ----------
    path:
        Image file to describe.
    model:
        Vision model name (e.g. ``"gpt-4o-mini"``).
    """
    if not _OPENAI_AVAILABLE:
        return None

    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        return None

    try:
        import openai  # noqa: PLC0415 (deferred import, package is optional)

        img_bytes = path.read_bytes()
        b64 = base64.b64encode(img_bytes).decode()

        # Choose MIME type from extension.
        _mime: dict[str, str] = {
            ".jpg": "image/jpeg",
            ".jpeg": "image/jpeg",
            ".png": "image/png",
        }
        mime_type = _mime.get(path.suffix.lower(), "image/jpeg")

        client = openai.OpenAI(api_key=api_key)
        response = client.chat.completions.create(
            model=model,
            messages=[
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": f"data:{mime_type};base64,{b64}",
                                "detail": "high",
                            },
                        },
                        {
                            "type": "text",
                            "text": (
                                "Describe this image in detail for an Obsidian knowledge-base note. "
                                "Include: what is shown, any visible text or labels, the context or "
                                "purpose of the image, and any notable visual details. "
                                "Use clear, concise prose."
                            ),
                        },
                    ],
                }
            ],
            max_tokens=1024,
        )
        description: str | None = response.choices[0].message.content
        return description if description else None

    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "OpenAI vision API call failed for %s — falling back to docling-only output. "
            "Error: %s",
            path,
            exc,
        )
        return None


# ---------------------------------------------------------------------------
# Public extractor class
# ---------------------------------------------------------------------------


class ImageExtractor(BaseExtractor):
    """Extract content from an image file.

    **Baseline:** :mod:`docling` performs OCR and exports any visible text as
    Markdown.  This path always works — no API key required.

    **Optional enhancement:** When ``OPENAI_API_KEY`` is set in the
    environment and the ``openai`` package is installed, a vision LLM
    (``gpt-4o-mini`` by default, overridable via ``KB_VISION_MODEL``) produces
    a richer semantic description that is prepended to the OCR text.

    Graceful degradation rules
    --------------------------
    * No ``OPENAI_API_KEY`` → docling OCR only (not an error).
    * ``openai`` package not installed → docling OCR only (not an error).
    * OpenAI API error → log ``WARNING``, use docling OCR only (not an error).
    * Corrupted image → :class:`~.base.ExtractionError` from docling (caller
      should mark the job as non-retryable failed).

    Parameters
    ----------
    use_openai:
        If ``True`` (the default) the extractor will attempt to call the
        vision LLM when ``OPENAI_API_KEY`` is set.  Set to ``False`` to
        force docling-only output regardless of the environment.
    """

    def __init__(self, *, use_openai: bool = True) -> None:
        self._use_openai = use_openai

    # ------------------------------------------------------------------
    # BaseExtractor interface
    # ------------------------------------------------------------------

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for ``.jpg``, ``.jpeg``, and ``.png`` files."""
        return path.suffix.lower() in _HANDLED_EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:  # noqa: ARG002
        """Extract content from the image at *input_path*.

        Steps
        -----
        1. Collect image metadata via Pillow (size, format, EXIF) — best-effort.
        2. Run docling OCR to extract any visible text (always executed).
        3. Optionally call the OpenAI vision API for a richer description.
        4. Assemble the combined Markdown content and return an
           :class:`~.base.ExtractionResult`.

        Parameters
        ----------
        input_path:
            Absolute path to the source image.
        work_dir:
            Per-job scratch directory (reserved for future multi-asset
            workflows; not written to by the current implementation).

        Returns
        -------
        ExtractionResult
            ``content`` contains the combined Markdown, ``metadata`` carries
            image dimensions, format, EXIF fields, and LLM provenance info.

        Raises
        ------
        ExtractionError
            When docling cannot process the image (e.g. corrupted file, I/O
            error).  The ``retryable`` attribute indicates whether the job
            should be requeued.
        """
        logger.info("ImageExtractor: processing %s", input_path)

        # ------------------------------------------------------------------ #
        # Step 1 — Pillow metadata (lightweight, never raises)
        # ------------------------------------------------------------------ #
        pil_meta = _pillow_metadata(input_path)

        # ------------------------------------------------------------------ #
        # Step 2 — docling OCR (may raise ExtractionError)
        # ------------------------------------------------------------------ #
        ocr_text = _docling_ocr(input_path)
        logger.debug(
            "docling OCR: %d chars extracted from %s", len(ocr_text), input_path
        )

        # ------------------------------------------------------------------ #
        # Step 3 — Optional vision-LLM enhancement
        # ------------------------------------------------------------------ #
        model = os.environ.get("KB_VISION_MODEL", "gpt-4o-mini")
        llm_description: str | None = None

        if self._use_openai:
            api_key_present = bool(os.environ.get("OPENAI_API_KEY"))
            if not api_key_present:
                logger.debug(
                    "OPENAI_API_KEY not set — using docling-only output for %s",
                    input_path,
                )
            elif not _OPENAI_AVAILABLE:
                logger.debug(
                    "OPENAI_API_KEY is set but the openai package is not installed "
                    "(install with: pip install kb-processor[vision]); "
                    "using docling-only output for %s",
                    input_path,
                )
            else:
                llm_description = _llm_describe(input_path, model)
                if llm_description:
                    logger.debug(
                        "Vision LLM (%s) produced %d chars for %s",
                        model,
                        len(llm_description),
                        input_path,
                    )

        # ------------------------------------------------------------------ #
        # Step 4 — Assemble Markdown content
        # ------------------------------------------------------------------ #
        parts: list[str] = []

        if llm_description:
            parts.append("## Image Description\n")
            parts.append(llm_description.strip())

        ocr_stripped = ocr_text.strip()
        if ocr_stripped:
            if parts:
                # Separate the two sections
                parts.append("\n\n## OCR Text\n")
            parts.append(ocr_stripped)

        if not parts:
            # Image contains no detectable text and LLM produced no output.
            parts.append(
                f"*No text detected in image `{input_path.name}`.*"
            )

        content = "\n".join(parts)

        # ------------------------------------------------------------------ #
        # Step 5 — Build metadata dict
        # ------------------------------------------------------------------ #
        metadata: dict[str, Any] = {
            "extractor": "image",
            "source_file": input_path.name,
            "docling_chars": len(ocr_text),
        }
        # Merge Pillow fields (width, height, format, mode, exif if present)
        metadata.update(pil_meta)
        if llm_description:
            metadata["llm_model"] = model
            metadata["llm_description_chars"] = len(llm_description)

        logger.info(
            "ImageExtractor: finished %s — %d content chars, llm=%s",
            input_path.name,
            len(content),
            llm_description is not None,
        )
        return ExtractionResult(content=content, metadata=metadata)
