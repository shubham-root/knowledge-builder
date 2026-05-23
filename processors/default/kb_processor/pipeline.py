"""
Main processing pipeline for the Knowledge Builder processor.

Pipeline steps
--------------
a. DETECT   — determine file type from extension and select extractor.
b. EXTRACT  — call the appropriate extractor (pdf / docx / xlsx / pptx / image).
c. SYNTHESIZE — send extracted content to an LLM to produce a structured
               Obsidian markdown note.
d. PLACE    — determine output paths inside the vault (never inside sources_dir).
e. WRITE    — write markdown + copy image assets atomically via :class:`AtomicWriter`.
f. RETURN   — build and return :class:`ProcessorResult`.

LLM backend selection
----------------------
The pipeline supports three LLM backends (in preference order):

1. **litellm** (``pip install kb-processor[llm]``) — unified interface that
   handles OpenAI, Anthropic, AWS Bedrock, and many other providers.
2. **openai** (``pip install kb-processor[vision]``) — direct OpenAI client.
3. **anthropic** (``pip install anthropic``) — direct Anthropic client.

If none of the above packages are installed the pipeline returns a
non-retryable error with install instructions.

Environment variables
---------------------
``KB_LLM_MODEL``
    Model identifier (default: ``gpt-4o-mini``). Pass any model name
    supported by your chosen backend:

    * OpenAI:   ``gpt-4o``, ``gpt-4o-mini``, ``o1-mini``
    * Anthropic: ``claude-3-5-sonnet-20241022``, ``claude-3-haiku-20240307``
    * Bedrock:  ``bedrock/anthropic.claude-3-sonnet-20240229-v1:0``

``KB_LLM_MAX_CONTENT_CHARS``
    Maximum characters of extracted text to include in the LLM prompt
    (default: ``50000``).  Content beyond this length is truncated.

``OPENAI_API_KEY``
    Required when using the openai or litellm backend with OpenAI models.

``ANTHROPIC_API_KEY``
    Required when using the anthropic or litellm backend with Anthropic models.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
import unicodedata
from pathlib import Path
from typing import Any

from .extractors.base import ExtractionError, ExtractionResult
from .extractors.docx import DocxExtractor
from .extractors.image import ImageExtractor
from .extractors.pdf import PdfExtractor
from .extractors.pptx import PptxExtractor
from .extractors.xlsx import XlsxExtractor
from .models import OutputEntry, ProcessorInput, ProcessorResultError, ProcessorResultOk
from .writer import AtomicWriter, PathViolation, WriteError

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Configuration constants
# ---------------------------------------------------------------------------

#: Default LLM model if ``KB_LLM_MODEL`` is not set.
_DEFAULT_LLM_MODEL: str = "gpt-4o-mini"

#: Maximum extracted-content characters to send to the LLM.
_MAX_CONTENT_CHARS: int = int(os.environ.get("KB_LLM_MAX_CONTENT_CHARS", "50000"))

#: Extension-based category fallback used when the LLM response contains no
#: ``category:`` field in its YAML frontmatter.
_CATEGORY_BY_EXT: dict[str, str] = {
    ".pdf":  "Documents",
    ".docx": "Documents",
    ".xlsx": "Data",
    ".xls":  "Data",
    ".pptx": "Presentations",
    ".ppt":  "Presentations",
    ".jpg":  "Media",
    ".jpeg": "Media",
    ".png":  "Media",
}

_DEFAULT_CATEGORY: str = "Uncategorized"

# ---------------------------------------------------------------------------
# Extractor registry (instantiated once at module level)
# ---------------------------------------------------------------------------

_EXTRACTORS: list[Any] = [
    PdfExtractor(),
    DocxExtractor(),
    XlsxExtractor(),
    PptxExtractor(),
    ImageExtractor(),
]

# ---------------------------------------------------------------------------
# LLM backend detection
# ---------------------------------------------------------------------------

_LLM_BACKEND: str | None = None

try:
    import litellm as _litellm_mod  # noqa: F401
    _LLM_BACKEND = "litellm"
    logger.debug("LLM backend: litellm")
except ImportError:
    pass

if _LLM_BACKEND is None:
    try:
        import openai as _openai_mod  # noqa: F401
        _LLM_BACKEND = "openai"
        logger.debug("LLM backend: openai")
    except ImportError:
        pass

if _LLM_BACKEND is None:
    try:
        import anthropic as _anthropic_mod  # noqa: F401
        _LLM_BACKEND = "anthropic"
        logger.debug("LLM backend: anthropic")
    except ImportError:
        pass

if _LLM_BACKEND is None:
    logger.warning(
        "No LLM backend available.  Synthesis will fail.  "
        "Install one of: pip install 'kb-processor[llm]'  (litellm) | "
        "pip install 'kb-processor[vision]'  (openai) | "
        "pip install anthropic"
    )


# ---------------------------------------------------------------------------
# Custom exceptions
# ---------------------------------------------------------------------------


class LLMAPIError(Exception):
    """Raised when the LLM API call fails or no backend is available.

    Parameters
    ----------
    message:
        Human-readable failure description.
    retryable:
        ``True``  — transient failure (network, rate-limit, timeout).
        ``False`` — permanent failure (bad config, missing package, auth error).
    """

    def __init__(self, message: str, *, retryable: bool = True) -> None:
        super().__init__(message)
        self.retryable = retryable


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _sanitize_dirname(name: str) -> str:
    """Sanitize *name* for safe use as a macOS directory component.

    * Normalises Unicode to NFC.
    * Replaces ``/`` and NUL bytes with ``-``.
    * Strips control characters.
    * Replaces ``:`` (disallowed on HFS+).
    * Trims leading/trailing whitespace.
    * Caps length at 64 characters.
    * Falls back to :data:`_DEFAULT_CATEGORY` if the result is empty.
    """
    name = unicodedata.normalize("NFC", name)
    name = re.sub(r"[/\x00]", "-", name)          # path separator + NUL
    name = re.sub(r"[\x01-\x1f\x7f]", "", name)   # control characters
    name = name.replace(":", "-")                   # HFS+ disallows ':'
    name = name.strip()
    if len(name) > 64:
        name = name[:64].rstrip()
    return name or _DEFAULT_CATEGORY


# Compiled once at module load for speed.
_FRONTMATTER_RE = re.compile(r"^---[ \t]*\r?\n(.*?)\r?\n---", re.DOTALL)
_CATEGORY_RE = re.compile(r"^\s*category\s*:\s*(.+?)\s*$", re.MULTILINE)


def _parse_category(markdown: str) -> str | None:
    """Extract the ``category:`` value from YAML frontmatter.

    Returns ``None`` if the frontmatter is absent or contains no ``category``
    field so the caller can fall back to the extension-based default.
    """
    stripped = markdown.lstrip()
    fm_match = _FRONTMATTER_RE.match(stripped)
    if not fm_match:
        return None
    frontmatter = fm_match.group(1)
    cat_match = _CATEGORY_RE.search(frontmatter)
    if not cat_match:
        return None
    raw = cat_match.group(1).strip().strip("\"'")
    return raw if raw else None


def _build_synthesis_prompt(
    content: str,
    source_filename: str,
    file_type: str,
    image_filenames: list[str],
    extraction_metadata: dict[str, Any],
) -> list[dict[str, Any]]:
    """Build the LLM messages list for Obsidian note synthesis.

    Truncates *content* to :data:`_MAX_CONTENT_CHARS` before embedding it in
    the prompt.  Adds an image-reference hint section when *image_filenames*
    is non-empty.  Includes selected keys from *extraction_metadata* to help
    the LLM produce richer output.

    Parameters
    ----------
    content:
        Raw Markdown/text extracted by the file extractor.
    source_filename:
        Base filename of the source file (e.g. ``"report.pdf"``).
    file_type:
        Human-readable file type label (e.g. ``"PDF"``, ``"DOCX"``).
    image_filenames:
        Filenames of image assets extracted to ``work_dir``.  The LLM is
        instructed to reference these with ``![[filename]]`` syntax.
    extraction_metadata:
        Metadata dict returned by the extractor (page count, title, …).

    Returns
    -------
    list[dict[str, Any]]
        OpenAI-compatible messages list with ``system`` + ``user`` entries.
    """
    # --- truncation -------------------------------------------------------
    truncated = len(content) > _MAX_CONTENT_CHARS
    if truncated:
        content = content[:_MAX_CONTENT_CHARS]

    # --- image reference hint ---------------------------------------------
    image_section = ""
    if image_filenames:
        file_list = "\n".join(f"  - {fn}" for fn in image_filenames)
        image_section = (
            f"\n\nEXTRACTED IMAGES (embed with ![[filename]] where appropriate):\n"
            f"{file_list}\n"
        )

    # --- metadata hint ----------------------------------------------------
    meta_parts: list[str] = []
    for key in ("title", "authors", "page_count", "table_count", "slide_count",
                "width", "height", "format"):
        val = extraction_metadata.get(key)
        if val is not None:
            if isinstance(val, list):
                val = ", ".join(str(v) for v in val)
            meta_parts.append(f"  {key}: {val}")
    metadata_hint = "\nDOCUMENT METADATA:\n" + "\n".join(meta_parts) if meta_parts else ""

    truncation_note = (
        "\n\n*(Source content was truncated to fit context window.)*"
        if truncated
        else ""
    )

    # --- system prompt ----------------------------------------------------
    system_prompt = (
        "You are an expert knowledge curator who creates well-structured, "
        "comprehensive Obsidian markdown notes for a personal knowledge base. "
        "Your notes are clear, well-organised, and make effective use of "
        "Obsidian features like wiki-links and embedded images."
    )

    # --- user prompt ------------------------------------------------------
    user_prompt = (
        f"Create a well-structured Obsidian markdown note from the extracted "
        f"document content below.\n\n"
        f"REQUIREMENTS:\n"
        f"1. Start with YAML frontmatter (--- delimited) containing:\n"
        f"   - title: (concise, descriptive note title)\n"
        f"   - category: (single word or short phrase — e.g. Research, Technology,\n"
        f"     Finance, Science, Reference, Data, Presentations, Media)\n"
        f"   - tags: (YAML list of 3–7 lowercase topic tags)\n"
        f"2. Immediately after the closing ---, add a primary `# Title` heading\n"
        f"   that matches the frontmatter title.\n"
        f"3. Organise the body into logical `##` sections with clear headings.\n"
        f"4. Preserve every table as a Markdown GFM pipe table with a header row.\n"
        f"5. Wrap key concepts, proper nouns, and important topics in [[wiki-links]].\n"
        f"6. Include key concepts and summaries in each section."
        f"{image_section}"
        f"7. Add a closing line of `#hashtag`-style inline tags for the most\n"
        f"   important topics (e.g. `#machine-learning #neural-networks`).\n"
        f"\n"
        f"SOURCE FILE: {source_filename} ({file_type})"
        f"{metadata_hint}"
        f"{truncation_note}"
        f"\n\nEXTRACTED CONTENT:\n{content}\n\n"
        f"OUTPUT: A complete Obsidian note starting with the YAML frontmatter."
    )

    return [
        {"role": "system", "content": system_prompt},
        {"role": "user", "content": user_prompt},
    ]


def _call_llm_sync(
    messages: list[dict[str, Any]],
    model: str,
) -> tuple[str, dict[str, Any]]:
    """Synchronous LLM completion call.

    Tries the detected backend (:data:`_LLM_BACKEND`) in order:
    ``litellm`` → ``openai`` → ``anthropic``.

    Parameters
    ----------
    messages:
        OpenAI-compatible messages list.
    model:
        Model identifier (e.g. ``"gpt-4o-mini"``).

    Returns
    -------
    tuple[str, dict[str, Any]]
        ``(response_text, usage_dict)`` where *usage_dict* contains
        ``tokens_in`` and/or ``tokens_out`` when the backend reports them.

    Raises
    ------
    LLMAPIError
        On any failure.  ``retryable`` is ``False`` for missing package /
        authentication errors, ``True`` for transient network/rate-limit errors.
    """
    if _LLM_BACKEND is None:
        raise LLMAPIError(
            "No LLM backend installed.  Install one of:\n"
            "  pip install 'kb-processor[llm]'    # litellm (all providers)\n"
            "  pip install 'kb-processor[vision]' # openai\n"
            "  pip install anthropic               # anthropic",
            retryable=False,
        )

    try:
        if _LLM_BACKEND == "litellm":
            return _call_litellm(messages, model)
        elif _LLM_BACKEND == "openai":
            return _call_openai(messages, model)
        elif _LLM_BACKEND == "anthropic":
            return _call_anthropic(messages, model)
        else:
            raise LLMAPIError(f"Unknown LLM backend: {_LLM_BACKEND!r}", retryable=False)

    except LLMAPIError:
        raise  # Already classified — pass through.
    except Exception as exc:
        _classify_and_raise_llm_error(exc, model)


def _extract_usage(response_usage: Any) -> dict[str, Any]:
    """Extract token-usage statistics from an API response usage object."""
    usage: dict[str, Any] = {}
    if response_usage is None:
        return usage
    for attr, key in (
        ("prompt_tokens", "tokens_in"),
        ("input_tokens", "tokens_in"),
        ("completion_tokens", "tokens_out"),
        ("output_tokens", "tokens_out"),
    ):
        val = getattr(response_usage, attr, None)
        if val is not None:
            usage[key] = int(val)
    return usage


def _classify_and_raise_llm_error(exc: Exception, model: str) -> None:
    """Re-raise *exc* as a :class:`LLMAPIError` with a retryable classification."""
    msg = str(exc)
    msg_lower = msg.lower()

    # Permanent failures (misconfiguration, bad credentials, unsupported model)
    permanent_hints = (
        "authentication", "unauthorized", "invalid api key", "api key",
        "permission", "model not found", "does not exist", "not supported",
    )
    if any(h in msg_lower for h in permanent_hints):
        raise LLMAPIError(
            f"LLM API permanent failure (model={model!r}): {exc}",
            retryable=False,
        ) from exc

    # Transient failures (network, timeout, rate-limit, server error)
    raise LLMAPIError(
        f"LLM API transient failure (model={model!r}): {type(exc).__name__}: {exc}",
        retryable=True,
    ) from exc


def _call_litellm(
    messages: list[dict[str, Any]],
    model: str,
) -> tuple[str, dict[str, Any]]:
    """Call the LLM via litellm."""
    try:
        import litellm  # noqa: PLC0415
    except ImportError as exc:
        raise LLMAPIError(
            "litellm is not installed — run: pip install 'kb-processor[llm]'",
            retryable=False,
        ) from exc

    try:
        response = litellm.completion(
            model=model,
            messages=messages,
            max_tokens=4096,
        )
    except Exception as exc:
        _classify_and_raise_llm_error(exc, model)

    text: str = response.choices[0].message.content or ""  # type: ignore[union-attr]
    usage = _extract_usage(getattr(response, "usage", None))
    return text, usage


def _call_openai(
    messages: list[dict[str, Any]],
    model: str,
) -> tuple[str, dict[str, Any]]:
    """Call the LLM via the openai SDK."""
    try:
        import openai  # noqa: PLC0415
    except ImportError as exc:
        raise LLMAPIError(
            "openai is not installed — run: pip install 'kb-processor[vision]'",
            retryable=False,
        ) from exc

    api_key = os.environ.get("OPENAI_API_KEY")
    try:
        client = openai.OpenAI(api_key=api_key)
        response = client.chat.completions.create(
            model=model,
            messages=messages,  # type: ignore[arg-type]
            max_tokens=4096,
        )
    except Exception as exc:
        _classify_and_raise_llm_error(exc, model)

    text = response.choices[0].message.content or ""  # type: ignore[union-attr]
    usage = _extract_usage(getattr(response, "usage", None))
    return text, usage


def _call_anthropic(
    messages: list[dict[str, Any]],
    model: str,
) -> tuple[str, dict[str, Any]]:
    """Call the LLM via the anthropic SDK.

    Converts the OpenAI-compatible messages format to Anthropic format
    (separate ``system`` string + ``messages`` list without system entries).
    """
    try:
        import anthropic  # noqa: PLC0415
    except ImportError as exc:
        raise LLMAPIError(
            "anthropic is not installed — run: pip install anthropic",
            retryable=False,
        ) from exc

    # Separate system message from user messages.
    system_content = ""
    user_messages: list[dict[str, Any]] = []
    for msg in messages:
        if msg["role"] == "system":
            system_content = msg["content"]
        else:
            user_messages.append(msg)

    api_key = os.environ.get("ANTHROPIC_API_KEY")
    try:
        client = anthropic.Anthropic(api_key=api_key)
        create_kwargs: dict[str, Any] = {
            "model": model,
            "max_tokens": 4096,
            "messages": user_messages,
        }
        if system_content:
            create_kwargs["system"] = system_content
        response = client.messages.create(**create_kwargs)
    except Exception as exc:
        _classify_and_raise_llm_error(exc, model)

    text = response.content[0].text if response.content else ""  # type: ignore[union-attr]
    usage = _extract_usage(getattr(response, "usage", None))
    return text, usage


# ---------------------------------------------------------------------------
# Main pipeline entry point
# ---------------------------------------------------------------------------


async def process(inp: ProcessorInput) -> ProcessorResultOk | ProcessorResultError:
    """Run the full extract → synthesize → write pipeline for *inp*.

    Never raises — all errors are captured and returned as
    :class:`~kb_processor.models.ProcessorResultError`.

    Parameters
    ----------
    inp:
        Processor input received from the Rust daemon (JSON-on-stdin).

    Returns
    -------
    ProcessorResultOk | ProcessorResultError
        On success: all output paths, byte counts, model, token usage.
        On failure: human-readable error, retryable flag, pipeline step.
    """
    input_path: Path = inp.input_path
    vault_root: Path = inp.vault_root
    sources_dir: Path = inp.sources_dir
    work_dir: Path = inp.work_dir

    logger.info(
        "Pipeline start — job_id=%d attempt=%d path=%s",
        inp.job_id,
        inp.attempt,
        input_path,
    )

    # ── a. DETECT ─────────────────────────────────────────────────────── #
    extractor = None
    for candidate in _EXTRACTORS:
        if candidate.can_handle(input_path):
            extractor = candidate
            break

    if extractor is None:
        suffix = input_path.suffix.lower()
        logger.error("No extractor for extension %r: %s", suffix, input_path)
        return ProcessorResultError(
            error=f"No extractor found for file type {suffix!r}",
            retryable=False,
            metadata={"step": "detect", "extension": suffix},
        )

    extractor_name = type(extractor).__name__
    logger.info("Using %s for %s", extractor_name, input_path.name)

    # ── b. EXTRACT ────────────────────────────────────────────────────── #
    logger.info("EXTRACT — calling %s.extract()", extractor_name)
    try:
        extracted: ExtractionResult = await asyncio.to_thread(
            extractor.extract, input_path, work_dir
        )
    except ExtractionError as exc:
        logger.warning(
            "Extraction failed (retryable=%s) for %s: %s",
            exc.retryable,
            input_path,
            exc,
        )
        return ProcessorResultError(
            error=f"Extraction failed: {exc}",
            retryable=exc.retryable,
            metadata={"step": "extract", "extractor": extractor_name},
        )
    except Exception as exc:  # noqa: BLE001
        logger.exception("Unexpected extraction error for %s", input_path)
        return ProcessorResultError(
            error=f"Unexpected extraction error: {type(exc).__name__}: {exc}",
            retryable=True,
            metadata={"step": "extract", "extractor": extractor_name},
        )

    logger.info(
        "Extracted %d content chars, %d image(s) from %s",
        len(extracted.content),
        len(extracted.images),
        input_path.name,
    )

    # ── c. SYNTHESIZE ─────────────────────────────────────────────────── #
    model: str = os.environ.get("KB_LLM_MODEL", _DEFAULT_LLM_MODEL)
    image_filenames: list[str] = [img.name for img in extracted.images]

    messages = _build_synthesis_prompt(
        content=extracted.content,
        source_filename=input_path.name,
        file_type=input_path.suffix.lstrip(".").upper() or "UNKNOWN",
        image_filenames=image_filenames,
        extraction_metadata=extracted.metadata,
    )

    logger.info("SYNTHESIZE — calling LLM model=%s", model)
    try:
        synthesized_markdown, llm_usage = await asyncio.to_thread(
            _call_llm_sync, messages, model
        )
    except LLMAPIError as exc:
        logger.warning(
            "LLM synthesis failed (retryable=%s): %s", exc.retryable, exc
        )
        return ProcessorResultError(
            error=f"LLM synthesis failed: {exc}",
            retryable=exc.retryable,
            metadata={"step": "synthesize", "model": model},
        )
    except Exception as exc:  # noqa: BLE001
        logger.exception("Unexpected LLM synthesis error for %s", input_path)
        return ProcessorResultError(
            error=f"Unexpected synthesis error: {type(exc).__name__}: {exc}",
            retryable=True,
            metadata={"step": "synthesize", "model": model},
        )

    if not synthesized_markdown or not synthesized_markdown.strip():
        logger.warning("LLM returned empty response for %s", input_path)
        return ProcessorResultError(
            error="LLM returned an empty response",
            retryable=True,
            metadata={"step": "synthesize", "model": model},
        )

    logger.info(
        "Synthesis complete — %d chars, usage=%s",
        len(synthesized_markdown),
        llm_usage,
    )

    # ── d. PLACE ──────────────────────────────────────────────────────── #
    #  Derive category from the LLM's YAML frontmatter, fall back to
    #  extension-based default.
    raw_category = _parse_category(synthesized_markdown)
    if not raw_category:
        raw_category = _CATEGORY_BY_EXT.get(
            input_path.suffix.lower(), _DEFAULT_CATEGORY
        )
    category: str = _sanitize_dirname(raw_category)

    stem: str = input_path.stem  # source filename without extension
    note_filename: str = f"{stem}.md"
    assets_dirname: str = f"{stem}-assets"

    notes_base: Path = vault_root / "Notes" / category
    note_final_path: Path = notes_base / note_filename
    assets_dir: Path = notes_base / assets_dirname

    logger.info(
        "PLACE — category=%r note=%s assets=%s",
        category,
        note_final_path,
        assets_dir,
    )

    # ── e. WRITE ──────────────────────────────────────────────────────── #
    logger.info("WRITE — staging %d output(s)", 1 + len(extracted.images))

    writer = AtomicWriter(
        work_dir=work_dir,
        vault_root=vault_root,
        sources_dir=sources_dir,
    )

    try:
        # Stage the synthesized markdown note.
        writer.stage(synthesized_markdown, note_final_path, "markdown")

        # Stage copies of every extracted image asset.
        for img_path in extracted.images:
            img_dest: Path = assets_dir / img_path.name
            writer.stage_copy(img_path, img_dest, "asset")

        # Atomically commit all staged writes into the vault.
        records = writer.commit()

    except PathViolation as exc:
        # Output path invariant violated — this is a processor bug, not retryable.
        logger.error(
            "Output path invariant violated (BUG) for %s: %s",
            input_path,
            exc,
        )
        writer.rollback()
        return ProcessorResultError(
            error=f"Output path invariant violation (processor bug): {exc}",
            retryable=False,
            metadata={"step": "write", "note_path": str(note_final_path)},
        )
    except WriteError as exc:
        logger.warning("Atomic write failed for %s: %s", input_path, exc)
        writer.rollback()
        return ProcessorResultError(
            error=f"Failed to write outputs to vault: {exc}",
            retryable=True,
            metadata={"step": "write"},
        )
    except Exception as exc:  # noqa: BLE001
        logger.exception("Unexpected write error for %s", input_path)
        writer.rollback()
        return ProcessorResultError(
            error=f"Unexpected write error: {type(exc).__name__}: {exc}",
            retryable=True,
            metadata={"step": "write"},
        )

    # ── f. RETURN ─────────────────────────────────────────────────────── #
    outputs: list[OutputEntry] = [
        OutputEntry(path=rec.path, kind=rec.kind, bytes=rec.bytes)
        for rec in records
    ]

    # Build metadata: LLM info + token usage + selected extraction metadata.
    result_metadata: dict[str, Any] = {
        "model": model,
        "category": category,
        "extractor": extractor_name,
    }
    result_metadata.update(llm_usage)  # tokens_in, tokens_out (when available)
    # Merge scalar extraction metadata fields (skip nested dicts/lists).
    for k, v in extracted.metadata.items():
        if isinstance(v, (str, int, float, bool)):
            result_metadata[k] = v

    logger.info(
        "Pipeline complete — job_id=%d wrote %d output(s): %s",
        inp.job_id,
        len(outputs),
        [str(o.path) for o in outputs],
    )

    return ProcessorResultOk(outputs=outputs, metadata=result_metadata)
