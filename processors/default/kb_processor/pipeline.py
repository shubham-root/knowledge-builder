"""
Main processing pipeline for the Knowledge Builder processor.

Pipeline steps
--------------
a. DETECT    — determine file type from extension and select extractor.
b. EXTRACT   — call the appropriate extractor (pdf / docx / xlsx / pptx / image).
c. STAGE     — write the extracted markdown to ``work_dir/extracted.md`` so
              the agent's bash tool can `cat` it.  Also copy figure assets
              into ``work_dir/assets/`` for potential later reference; the
              agent decides whether to import any of them into the vault.
d. INTEGRATE — spawn the pi-mediated agent (see kb_processor.agent.rpc_driver),
              which surveys the vault via the Obsidian CLI through the
              ``kb-obsidian`` policy wrapper, decides where the new content
              belongs, and either writes (apply mode) or records a plan
              (shadow mode).
e. RETURN    — assemble :class:`ProcessorResult` with plan metadata.

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
from typing import Any, Callable

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


async def process(
    inp: ProcessorInput,
    progress: "Callable[[str], None] | None" = None,
) -> "ProcessorResultOk | ProcessorResultError":
    """Run the full extract → synthesize → write pipeline for *inp*.

    Never raises — all errors are captured and returned as
    :class:`~kb_processor.models.ProcessorResultError`.

    Parameters
    ----------
    inp:
        Processor input received from the Rust daemon (JSON-on-stdin).
    progress:
        Optional callable that receives a progress-step label string.  The
        entry point (``__main__``) passes a function that prints
        ``[kb-processor] <label>`` to stdout so operators can monitor long
        jobs.  Defaults to ``None`` (no-op).

    Returns
    -------
    ProcessorResultOk | ProcessorResultError
        On success: all output paths, byte counts, model, token usage.
        On failure: human-readable error, retryable flag, pipeline step.
    """
    def _report(label: str) -> None:
        if progress is not None:
            try:
                progress(label)
            except Exception:  # noqa: BLE001
                pass  # never let a progress callback crash the pipeline
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
    _report("Step 1/4: Extracting content...")
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

    # ── c. STAGE ──────────────────────────────────────────────────────── #
    #
    # Write the extracted markdown to a deterministic path inside the
    # per-job ``work_dir`` so the agent's bash tool can ``cat`` it.  This
    # is the ONLY place the daemon-side processor writes a file outside
    # the vault; it is intentionally outside ``vault_root`` so it is not
    # treated as an output and does not need to satisfy the vault-path
    # invariant.
    _report("Step 2/3: Staging extracted content for the agent...")
    extracted_md_path: Path = work_dir / "extracted.md"

    # Build a small front-matter block so the agent has unambiguous
    # provenance information at the top of the file it cat()s.
    fm_lines: list[str] = [
        "---",
        f"source_basename: {input_path.name!r}",
        f"file_type: {input_path.suffix.lstrip('.').upper() or 'UNKNOWN'}",
        f"extractor: {extractor_name}",
        f"job_id: {inp.job_id}",
    ]
    extracted_pages = extracted.metadata.get("page_count")
    if extracted_pages:
        fm_lines.append(f"page_count: {extracted_pages}")
    if extracted.images:
        fm_lines.append(f"figure_count: {len(extracted.images)}")
    fm_lines.append("---\n\n")

    body = "\n".join(fm_lines) + extracted.content
    if extracted.images:
        body += "\n\n## Extracted figures (work_dir copies)\n\n"
        for img in extracted.images:
            body += f"- `{img}`\n"

    try:
        extracted_md_path.write_text(body, encoding="utf-8")
    except OSError as exc:
        logger.exception("Failed to stage extracted.md")
        return ProcessorResultError(
            error=f"Failed to stage extracted markdown: {exc}",
            retryable=True,
            metadata={"step": "stage"},
        )

    logger.info(
        "Staged extracted.md (%d bytes, %d figures) at %s",
        len(body),
        len(extracted.images),
        extracted_md_path,
    )

    # ── d. INTEGRATE ──────────────────────────────────────────────────── #
    _report("Step 3/3: Integrating into vault via agent...")

    model: str = os.environ.get("KB_LLM_MODEL", _DEFAULT_LLM_MODEL)
    agent_mode = os.environ.get("KB_AGENT_MODE", "apply").strip().lower()
    if agent_mode not in ("shadow", "apply"):
        logger.warning(
            "KB_AGENT_MODE=%r is invalid; defaulting to 'apply'", agent_mode,
        )
        agent_mode = "apply"

    # Lazy-import the agent driver so unrelated unit tests of the pipeline
    # don't pay its (cheap) import cost or pull in the rpc_driver's
    # transitive dependencies.
    from kb_processor.agent import (   # noqa: PLC0415
        AgentBudgetError,
        AgentError,
        AgentInput,
        MissingApiKeyError,
        PiNotFoundError,
        run_agent,
    )

    try:
        # Resolve agent_root: prefer the explicit value from ProcessorInput,
        # fall back to the legacy default `vault_root/KnowledgeBase` so older
        # daemon builds (without the new field) keep working.
        agent_root = inp.agent_root
        if agent_root is None or str(agent_root) in (".", "", "/"):
            agent_root = vault_root / "KnowledgeBase"
        # Make sure the directory exists; the daemon should have done this
        # but processors may run standalone (e.g. from the command line).
        try:
            agent_root.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            return ProcessorResultError(
                error=f"Could not create agent_root {agent_root}: {exc}",
                retryable=False,
                metadata={"step": "integrate"},
            )

        agent_result = await asyncio.to_thread(
            run_agent,
            AgentInput(
                extracted_path  = extracted_md_path,
                work_dir        = work_dir,
                vault_root      = vault_root,
                sources_dir     = sources_dir,
                agent_root      = agent_root,
                source_basename = input_path.name,
                model           = model,
                job_id          = inp.job_id,
                mode            = agent_mode,
            ),
        )
    except MissingApiKeyError as exc:
        logger.error("Agent refused to spawn: %s", exc)
        return ProcessorResultError(
            error=f"Agent missing credentials: {exc}",
            retryable=False,                # operator must add the key
            metadata={"step": "integrate", "agent_mode": agent_mode},
        )
    except PiNotFoundError as exc:
        logger.error("pi binary missing: %s", exc)
        return ProcessorResultError(
            error=f"pi binary not found: {exc}",
            retryable=False,
            metadata={"step": "integrate", "agent_mode": agent_mode},
        )
    except AgentBudgetError as exc:
        logger.warning("Agent ran out of budget: %s", exc)
        return ProcessorResultError(
            error=f"Agent budget exhausted before producing a plan: {exc}",
            retryable=True,
            metadata={"step": "integrate", "agent_mode": agent_mode},
        )
    except AgentError as exc:
        logger.exception("Agent failed")
        return ProcessorResultError(
            error=f"Agent error: {type(exc).__name__}: {exc}",
            retryable=True,
            metadata={"step": "integrate", "agent_mode": agent_mode},
        )

    logger.info(
        "Agent done — turns=%d elapsed=%.1fs %s aborted=%s",
        agent_result.turns,
        agent_result.elapsed_secs,
        agent_result.plan.summary(),
        agent_result.aborted,
    )

    # ── d.5 EMPTY-PLAN GUARD ───────────────────────────────────────────── #
    #
    # If the agent finished without proposing any mutations, treat that
    # as a soft failure regardless of mode.  In shadow mode this gives
    # us a preserved work_dir for postmortem (the daemon's pool retains
    # work_dir on failure but cleans it on success).  In apply mode it
    # keeps the row visible in `kb list --status failed` instead of
    # silently passing.
    #
    # `retryable=True` so the standard backoff kicks in; `max_attempts`
    # in worker config limits the blast radius (default 3 attempts).
    if len(agent_result.plan) == 0 and not agent_result.aborted:
        final_text = (agent_result.final_assistant_text or "").strip()
        excerpt = (final_text[:400] + ("…" if len(final_text) > 400 else "")) if final_text else "(no final assistant message)"
        logger.warning(
            "Agent produced an empty plan after %d turn(s); marking as "
            "retryable failure so the work_dir is preserved.",
            agent_result.turns,
        )
        return ProcessorResultError(
            error=(
                f"Agent ran for {agent_result.turns} turn(s) in {agent_mode} "
                f"mode but proposed no mutations.  Final message: {excerpt}"
            ),
            retryable=True,
            metadata={
                "step":               "integrate",
                "reason":             "empty_plan",
                "agent_mode":         agent_mode,
                "agent_turns":        agent_result.turns,
                "agent_elapsed_secs": round(agent_result.elapsed_secs, 2),
                "agent_log":          str(agent_result.agent_log),
                "plan_file":          str(agent_result.plan_file),
                "extracted_md":       str(extracted_md_path),
                "agent_provider":     agent_result.metadata.get("provider"),
                "agent_model":        agent_result.metadata.get("model"),
            },
        )

    # ── d.5 LINK SWEEP ─────────────────────────────────────────────────── #
    #
    # In apply mode, post-process every file the agent created or modified
    # and replace any unresolved ``[[wikilink]]`` (target note does not
    # exist) with plain-text ``Target [possible linkout - elaboration
    # needed]``.  This is the deterministic backstop for the prompt-level
    # wikilink discipline taught in SKILL.md — a rogue / non-compliant LLM
    # cannot leave the knowledge graph with dangling links.
    #
    # Skipped in shadow mode (nothing has been written yet).
    # Skipped on empty plans (the empty-plan guard above already
    # returned ProcessorResultError).
    sweep_stats_meta: dict[str, object] = {
        "link_sweep_examined": 0,
        "link_sweep_modified": 0,
        "link_sweep_replaced": 0,
    }
    if agent_mode == "apply" and len(agent_result.plan) > 0:
        # Lazy import — keeps the cold-start cost off shadow-only paths.
        from kb_processor.agent.link_sweeper import (   # noqa: PLC0415
            files_touched_by_plan,
            sweep_files,
        )

        touched = files_touched_by_plan(
            agent_result.plan.entries, vault_root,
        )
        if touched:
            try:
                sweep_stats = sweep_files(
                    files       = touched,
                    vault_root  = vault_root,
                    sources_dir = sources_dir,
                    agent_root  = agent_root,
                )
                sweep_stats_meta = sweep_stats.as_metadata()
                if sweep_stats.links_replaced > 0:
                    logger.info(
                        "link_sweep: rewrote %d unresolved link(s) across "
                        "%d file(s); examples: %s",
                        sweep_stats.links_replaced,
                        sweep_stats.files_modified,
                        sweep_stats.examples[:5],
                    )
                else:
                    logger.info(
                        "link_sweep: clean (%d file(s) examined, no "
                        "unresolved wikilinks)",
                        sweep_stats.files_examined,
                    )
            except Exception as exc:    # noqa: BLE001  defensive
                # The sweeper is best-effort; a failure here should NOT
                # turn a successful agent run into a job failure.  Log
                # and surface in metadata.
                logger.exception("link_sweep: unexpected failure")
                sweep_stats_meta = {
                    "link_sweep_examined": 0,
                    "link_sweep_modified": 0,
                    "link_sweep_replaced": 0,
                    "link_sweep_error":    f"{type(exc).__name__}: {exc}",
                }

    # ── e. RETURN ─────────────────────────────────────────────────────── #
    #
    # Output collection differs by mode:
    #
    #   shadow → no vault outputs (nothing was actually written).  The
    #            plan file path lives in metadata for `kb show <id>`.
    #
    #   apply  → outputs are the vault paths of the entries the agent
    #            successfully applied (cmd in {create, append, prepend,
    #            move, rename}).  We extract them from the plan entries
    #            so the daemon can record them in `outputs` and reject
    #            any that violate the vault-path invariant (defence in
    #            depth — the wrapper already blocked sources_dir paths).
    outputs: list[OutputEntry] = []
    if agent_mode == "apply":
        for entry in agent_result.plan.entries:
            if not entry.applied:
                continue
            # Pull `path=` first; some commands use `file=` only.
            kv: dict[str, str] = {}
            for tok in entry.args:
                eq = tok.find("=")
                if eq > 0:
                    kv[tok[:eq]] = tok[eq + 1:]
            raw = kv.get("path") or kv.get("to") or kv.get("file") or ""
            if not raw:
                continue
            p = Path(raw)
            if not p.is_absolute():
                p = vault_root / p
            try:
                size_bytes = p.stat().st_size if p.exists() else 0
            except OSError:
                size_bytes = 0
            outputs.append(OutputEntry(
                path  = str(p),
                kind  = entry.kind,        # "create" | "append" | "rename" | …
                bytes = size_bytes,
            ))

    result_metadata: dict[str, Any] = {
        "extractor":           extractor_name,
        "model":               model,
        "agent_mode":          agent_mode,
        "agent_turns":         agent_result.turns,
        "agent_elapsed_secs":  round(agent_result.elapsed_secs, 2),
        "agent_aborted":       agent_result.aborted,
        "plan_file":           str(agent_result.plan_file),
        "agent_log":           str(agent_result.agent_log),
        "plan_summary":        agent_result.plan.summary(),
        "plan_entry_count":    len(agent_result.plan),
        "extracted_md":        str(extracted_md_path),
        "rogue_writes_count":  len(agent_result.rogue_writes),
        "rogue_writes":        [str(p) for p in agent_result.rogue_writes[:20]],
        # Tell the daemon's worker pool to retain the work_dir on success
        # when running in shadow mode — the plan file lives there and the
        # operator needs to be able to read it via `kb show`.  Apply mode
        # mutations are durable in the vault, so retention is unnecessary
        # there and the work_dir can be cleaned as usual.
        "retain_work_dir":     (agent_mode == "shadow"),
        # Link-sweep results: how many unresolved wikilinks the post-run
        # sweeper rewrote to placeholder text.  Always present; zeros in
        # shadow mode and on empty plans.
        **sweep_stats_meta,
    }
    if agent_result.final_assistant_text:
        # Cap so a chatty model doesn't blow processor_meta size.
        result_metadata["agent_final_text"] = agent_result.final_assistant_text[:4000]
    # Inherit agent-side metadata (provider, model, mode).
    for k, v in agent_result.metadata.items():
        result_metadata.setdefault(f"agent_{k}", v)
    # Carry forward useful scalar extraction metadata.
    for k, v in extracted.metadata.items():
        if isinstance(v, (str, int, float, bool)):
            result_metadata.setdefault(k, v)

    logger.info(
        "Pipeline complete — job_id=%d mode=%s plan_entries=%d outputs=%d",
        inp.job_id, agent_mode, len(agent_result.plan), len(outputs),
    )

    return ProcessorResultOk(outputs=outputs, metadata=result_metadata)
