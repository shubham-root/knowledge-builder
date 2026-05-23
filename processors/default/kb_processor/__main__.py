"""
Entry point for the Knowledge Builder processor.

Usage::

    python3 -m kb_processor <input_path> <work_dir>

Reads a JSON descriptor from **stdin** (see :mod:`kb_processor.models` for the
schema), dispatches to :func:`kb_processor.pipeline.process`, and writes the
JSON result as the **last line of stdout**.

Protocol
--------
*  All lines printed to stdout before the last line are progress log lines.
   Their format is ``[kb-processor] <message>`` and they are safe to ignore.
*  The **last line** of stdout is always a single JSON object matching the
   contract in PLAN.md §8:

   Success::

       {"status": "ok", "outputs": [...], "metadata": {...}}

   Failure::

       {"status": "error", "error": "...", "retryable": true, "metadata": {...}}

*  All detailed Python logging is directed to **stderr** so it never
   interferes with the stdout JSON contract.

Exit codes
----------
*  ``0`` — ``status == "ok"``
*  ``1`` — ``status == "error"`` or an unhandled exception
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import sys
from pathlib import Path
from typing import Callable, NoReturn

from .models import ProcessorInput, ProcessorResultError, ProcessorResultOk
from . import pipeline

# ---------------------------------------------------------------------------
# Logging — stderr only, never pollutes the stdout JSON contract.
# ---------------------------------------------------------------------------
logging.basicConfig(
    stream=sys.stderr,
    level=os.environ.get("KB_LOG_LEVEL", "INFO").upper(),
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
logger = logging.getLogger("kb_processor")


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _progress(msg: str) -> None:
    """Print a progress line to stdout (before the final JSON result)."""
    print(f"[kb-processor] {msg}", flush=True)


def _emit(result: "ProcessorResultOk | ProcessorResultError") -> None:
    """Print the result JSON as the last line of stdout and flush."""
    print(result.to_json_line(), flush=True)


def _error(
    error: str,
    *,
    retryable: bool,
    step: str = "",
    **extra: object,
) -> ProcessorResultError:
    """Build a :class:`ProcessorResultError` with optional metadata."""
    metadata: dict = {}
    if step:
        metadata["step"] = step
    metadata.update(extra)
    return ProcessorResultError(
        error=error,
        retryable=retryable,
        metadata=metadata if metadata else None,
    )


def _classify_exception(exc: BaseException) -> bool:
    """Return ``True`` if *exc* should be treated as a retryable failure.

    Error taxonomy
    ~~~~~~~~~~~~~~
    *  **Non-retryable** — permanent misconfiguration / bad content:

       - :class:`PermissionError` — OS-level access denied.
       - :class:`IsADirectoryError` — path confusion (processor bug).
       - :class:`json.JSONDecodeError` — malformed stdin input.
       - ``pydantic.ValidationError`` — schema mismatch.
       - ``PathViolation`` (from :mod:`kb_processor.writer`) — output path
         invariant violated (processor bug).
       - ``ExtractionError(retryable=False)`` — permanent file-format error.
       - ``LLMAPIError(retryable=False)`` — bad credentials / unknown model.

    *  **Retryable** — transient conditions worth a retry after backoff:

       - :class:`TimeoutError` — subprocess/network timeout.
       - :class:`ConnectionError` / :class:`OSError` with transient errno.
       - ``LLMAPIError(retryable=True)`` — rate-limit, server 5xx.
       - Any unrecognised exception (assume transient).
    """
    exc_type = type(exc).__name__
    exc_msg = str(exc).lower()

    # ── Non-retryable by exception type ─────────────────────────────── #
    if isinstance(exc, (PermissionError, IsADirectoryError)):
        return False

    if isinstance(exc, json.JSONDecodeError):
        return False

    # Pydantic ValidationError (avoid hard import)
    if exc_type == "ValidationError":
        return False

    # PathViolation from writer.py (avoid circular import — check by name)
    if exc_type == "PathViolation":
        return False

    # ExtractionError / LLMAPIError with explicit retryable=False attribute
    retryable_attr = getattr(exc, "retryable", None)
    if retryable_attr is False:
        return False

    # ── Retryable ────────────────────────────────────────────────────── #
    # Explicit retryable=True attribute
    if retryable_attr is True:
        return True

    # Common transient exception types
    if isinstance(exc, (TimeoutError, ConnectionError, BrokenPipeError)):
        return True

    # OSError/IOError: classify by errno keyword heuristic
    if isinstance(exc, OSError):
        # ENOSPC, EIO, ETIMEDOUT, network errors → retryable
        retryable_errnos = {
            # fmt: off
            28,   # ENOSPC — no space left on device
            5,    # EIO — I/O error
            110,  # ETIMEDOUT (Linux)
            60,   # ETIMEDOUT (macOS/BSD)
            # fmt: on
        }
        if exc.errno in retryable_errnos:
            return True
        # Non-retryable OS errors (bad file number, not a file, etc.)
        non_retryable_errnos = {
            1,    # EPERM
            13,   # EACCES
            9,    # EBADF
            21,   # EISDIR
        }
        if exc.errno in non_retryable_errnos:
            return False
        return True  # unknown OSError → assume transient

    # Unknown exception → optimistically retryable (assume transient crash)
    return True


# ---------------------------------------------------------------------------
# Main entry point
# ---------------------------------------------------------------------------

def main() -> NoReturn:
    """CLI entry point.

    Called by ``python3 -m kb_processor`` and the ``kb-processor`` console
    script registered in ``pyproject.toml``.

    Reads JSON from stdin, dispatches to the pipeline, writes result JSON to
    stdout, and exits with an appropriate code.
    """

    # ------------------------------------------------------------------
    # 0. Parse positional command-line args.
    #    The Rust daemon invokes:
    #        <processor.command> <input_path> <work_dir>
    #    and ALSO sends the full JSON on stdin.  The positional args are
    #    supplementary context; stdin is always the authoritative source.
    #    We capture the args here so they can be used as a fallback if the
    #    stdin JSON is absent or incomplete.
    # ------------------------------------------------------------------
    argv_input_path: Path | None = None
    argv_work_dir: Path | None = None

    args = sys.argv[1:]
    if len(args) >= 1:
        argv_input_path = Path(args[0])
        logger.debug("argv input_path: %s", argv_input_path)
    if len(args) >= 2:
        argv_work_dir = Path(args[1])
        logger.debug("argv work_dir: %s", argv_work_dir)

    # ------------------------------------------------------------------
    # 1. Read ALL of stdin as a single JSON object.
    # ------------------------------------------------------------------
    try:
        raw = sys.stdin.read()
    except Exception as exc:  # noqa: BLE001
        logger.exception("Failed to read from stdin")
        _emit(_error(
            f"Failed to read from stdin: {exc}",
            retryable=True,
            step="stdin_read",
        ))
        sys.exit(1)

    raw = raw.strip()

    # If stdin is empty but argv provides the paths, we cannot construct a
    # valid ProcessorInput (missing vault_root, content_hash, job_id …).
    # Emit a clear error instead of silently doing nothing.
    if not raw:
        msg = (
            "Empty stdin: the processor requires a JSON object on stdin. "
            "Ensure the daemon is sending the ProcessorInput JSON."
        )
        if argv_input_path:
            msg += f"  (argv input_path={argv_input_path})"
        logger.error(msg)
        _emit(_error(msg, retryable=False, step="stdin_read"))
        sys.exit(1)

    # ------------------------------------------------------------------
    # 2. Parse JSON and validate into ProcessorInput.
    # ------------------------------------------------------------------
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        logger.error("Invalid JSON on stdin: %s", exc)
        _emit(_error(
            f"Invalid JSON on stdin: {exc}",
            retryable=False,
            step="json_parse",
        ))
        sys.exit(1)

    try:
        processor_input = ProcessorInput.model_validate(data)
    except Exception as exc:  # noqa: BLE001  (pydantic.ValidationError)
        logger.error("Failed to parse processor input: %s", exc)
        _emit(_error(
            f"Failed to parse processor input: {type(exc).__name__}: {exc}",
            retryable=False,
            step="input_validation",
        ))
        sys.exit(1)

    # ------------------------------------------------------------------
    # 3. Emit the "starting" progress line.
    # ------------------------------------------------------------------
    filename = processor_input.input_path.name
    _progress(f"Starting processing: {filename}")

    logger.info(
        "Processing job_id=%d attempt=%d path=%s",
        processor_input.job_id,
        processor_input.attempt,
        processor_input.input_path,
    )

    # ------------------------------------------------------------------
    # 4. Run the pipeline.
    #    Pass _progress as the callback so each pipeline step emits its
    #    own "Step N/4: …" marker to stdout in real time.
    # ------------------------------------------------------------------
    try:
        result: ProcessorResultOk | ProcessorResultError = asyncio.run(
            pipeline.process(processor_input, progress=_progress)
        )
    except KeyboardInterrupt:
        # SIGINT during processing — surface as a retryable error.
        logger.warning("Processing interrupted by KeyboardInterrupt")
        _emit(_error(
            "Processing interrupted (KeyboardInterrupt)",
            retryable=True,
            step="pipeline",
        ))
        sys.exit(1)
    except Exception as exc:  # noqa: BLE001
        # Unhandled exception escaped the pipeline's own error handling.
        # Classify and emit as structured error.
        retryable = _classify_exception(exc)
        logger.exception(
            "Unhandled exception in pipeline.process (retryable=%s)", retryable
        )
        _emit(_error(
            f"Unhandled pipeline exception: {type(exc).__name__}: {exc}",
            retryable=retryable,
            step="pipeline",
            exception_type=type(exc).__name__,
        ))
        sys.exit(1)

    # ------------------------------------------------------------------
    # 5. Emit the result JSON as the LAST LINE of stdout.
    # ------------------------------------------------------------------
    _emit(result)

    if result.status == "ok":
        logger.info(
            "Job %d completed successfully — %d output(s) written",
            processor_input.job_id,
            len(result.outputs),
        )
        sys.exit(0)
    else:
        logger.error(
            "Job %d failed (retryable=%s): %s",
            processor_input.job_id,
            result.retryable,
            result.error,
        )
        sys.exit(1)


if __name__ == "__main__":
    main()
