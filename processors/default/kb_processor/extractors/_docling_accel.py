"""
Shared helper for building docling ``DocumentConverter`` instances that
always run with the platform's hardware accelerator.

On macOS (Apple Silicon and Intel Macs with a supported GPU) this means
the **MPS** (Metal Performance Shaders) backend.  On other platforms we
fall back to docling's ``AUTO`` device which picks CUDA / MPS / CPU as
appropriate.

Reference
---------
https://docling-project.github.io/docling/examples/run_with_accelerator/

Notes
-----
* The accelerator is only consulted by docling's *ML-backed* pipelines
  (PDF and image: OCR, layout analysis, table-structure detection).
  DOCX / PPTX / XLSX use simple XML parsing pipelines and do not invoke
  the accelerator, so this helper is intentionally not used there.
* Knobs can be tuned with environment variables:
    - ``DOCLING_DEVICE``     — ``mps`` | ``cuda`` | ``cpu`` | ``auto``
                                (default: ``mps`` on Darwin, ``auto`` elsewhere)
    - ``DOCLING_NUM_THREADS`` — int, default ``8``

MPS × float64 caveat
--------------------
docling's layout detector (``RT-DETR v2`` via ``transformers``) allocates
``torch.float64`` tensors for some position-embedding math.  Apple's MPS
backend does **not** support ``float64`` and raises
``TypeError: Cannot convert a MPS Tensor to float64 dtype …`` for every
page — so a naive MPS run crashes at conversion time.

The upstream-recommended fix is the env var ``PYTORCH_ENABLE_MPS_FALLBACK=1``
which makes unsupported MPS ops silently fall back to CPU while the rest of
the model continues to run on the GPU.  We set this here at module-import
time, which happens **before** docling (and therefore torch) is first
imported by the lazy import inside :func:`make_accelerated_converter`,
guaranteeing the variable is in place when torch initialises its MPS
allocator.
"""

from __future__ import annotations

import os as _os

# IMPORTANT: must run before torch / docling / transformers are imported
# anywhere in the process (see module docstring above).
_os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")

import logging
import os
import platform
from functools import lru_cache
from typing import Any

logger = logging.getLogger(__name__)


def _resolve_device(accel_module: Any) -> Any:
    """Pick an :class:`AcceleratorDevice` value based on env + platform."""
    AcceleratorDevice = accel_module.AcceleratorDevice  # noqa: N806

    raw = os.environ.get("DOCLING_DEVICE", "").strip().lower()
    if not raw:
        # Default: MPS on macOS, AUTO everywhere else.
        raw = "mps" if platform.system() == "Darwin" else "auto"

    mapping = {
        "mps": getattr(AcceleratorDevice, "MPS", None),
        "cuda": getattr(AcceleratorDevice, "CUDA", None),
        "cpu": getattr(AcceleratorDevice, "CPU", None),
        "auto": getattr(AcceleratorDevice, "AUTO", None),
    }
    device = mapping.get(raw)
    if device is None:
        logger.warning(
            "Unknown / unsupported DOCLING_DEVICE=%r — falling back to AUTO", raw
        )
        device = AcceleratorDevice.AUTO
    return device


def _num_threads() -> int:
    raw = os.environ.get("DOCLING_NUM_THREADS", "").strip()
    if not raw:
        return 8
    try:
        n = int(raw)
        return n if n > 0 else 8
    except ValueError:
        return 8


@lru_cache(maxsize=2)
def _build_pdf_pipeline_options(do_ocr: bool = True) -> Any:
    """Construct ``PdfPipelineOptions`` with accelerator + OCR controls.

    ``do_ocr`` toggles docling's OCR pipeline.  Pass ``False`` for PDFs that
    are already text-native (selectable text on every sampled page) — this
    bypasses OCR entirely and shrinks 200-page-book extractions from ~1 hour
    to ~1 minute on M-series GPUs.

    Cached on ``do_ocr`` so we don't re-import docling submodules on every
    conversion.
    """
    # Lazy imports — these modules are only needed when an actual extraction
    # runs, and importing them at module scope would defeat the lazy-import
    # pattern used by the PDF / image extractors.
    from docling.datamodel import accelerator_options as accel_module  # noqa: PLC0415
    from docling.datamodel.pipeline_options import PdfPipelineOptions  # noqa: PLC0415

    AcceleratorOptions = accel_module.AcceleratorOptions  # noqa: N806

    device = _resolve_device(accel_module)
    threads = _num_threads()

    accelerator_options = AcceleratorOptions(num_threads=threads, device=device)

    pipeline_options = PdfPipelineOptions()
    pipeline_options.accelerator_options = accelerator_options
    pipeline_options.do_ocr = do_ocr
    pipeline_options.do_table_structure = True
    # Without this, docling parses pictures structurally but does NOT keep
    # the raster data, so `picture.get_image(doc)` returns None.  Enabling
    # it costs a small amount of memory + decode time per figure but is the
    # only way to actually save figure PNGs into the work_dir.
    pipeline_options.generate_picture_images = True
    # ``table_structure_options`` exists in docling >= 2.0; guard for safety.
    tso = getattr(pipeline_options, "table_structure_options", None)
    if tso is not None and hasattr(tso, "do_cell_matching"):
        tso.do_cell_matching = True

    logger.info(
        "docling accelerator configured: device=%s, num_threads=%d, do_ocr=%s",
        getattr(device, "value", device),
        threads,
        do_ocr,
    )
    return pipeline_options


def make_accelerated_converter(*, do_ocr: bool = True) -> Any:
    """Return a ``DocumentConverter`` with the accelerator wired up for
    the PDF and IMAGE input formats.

    Parameters
    ----------
    do_ocr:
        Pass ``False`` to skip OCR for text-native PDFs (selectable text on
        every page).  Default ``True`` (preserves prior behaviour).

    Falls back to a plain ``DocumentConverter()`` if the docling version
    in use does not expose the accelerator API (older releases) — we log
    a warning in that case so the degradation is visible.
    """
    from docling.document_converter import (  # noqa: PLC0415
        DocumentConverter,
        PdfFormatOption,
    )
    from docling.datamodel.base_models import InputFormat  # noqa: PLC0415

    try:
        pipeline_options = _build_pdf_pipeline_options(do_ocr=do_ocr)
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "Could not build accelerated docling pipeline (%s) — "
            "falling back to default DocumentConverter()",
            exc,
        )
        return DocumentConverter()

    format_options: dict[Any, Any] = {
        InputFormat.PDF: PdfFormatOption(pipeline_options=pipeline_options),
    }
    # IMAGE format shares the PDF-style pipeline in docling v2.
    image_fmt = getattr(InputFormat, "IMAGE", None)
    if image_fmt is not None:
        format_options[image_fmt] = PdfFormatOption(pipeline_options=pipeline_options)

    return DocumentConverter(format_options=format_options)
