"""
Entry point for the Knowledge Builder processor.

Usage::

    python3 -m kb_processor <input_path> <work_dir>

Reads a JSON descriptor from **stdin** (see :mod:`kb_processor.models` for the
schema), dispatches to :func:`kb_processor.pipeline.process`, and writes the
JSON result as the **last line of stdout**.

Exit codes:
    0 — processing succeeded (``status == "ok"``)
    1 — processing failed  (``status == "error"``) or an unexpected exception
        was raised.
"""

from __future__ import annotations

import json
import logging
import sys
from typing import NoReturn

from .models import ProcessorInput, ProcessorResultError
from . import pipeline

# ---------------------------------------------------------------------------
# Logging — write to stderr so it does not pollute the stdout JSON contract.
# ---------------------------------------------------------------------------
logging.basicConfig(
    stream=sys.stderr,
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
logger = logging.getLogger("kb_processor")


def _emit(result: "ProcessorResultError | kb_processor.models.ProcessorResultOk") -> None:
    """Print the result JSON as the last line of stdout and flush."""
    print(result.to_json_line(), flush=True)


def main() -> NoReturn:
    """
    CLI entry point — called by ``python3 -m kb_processor`` or the
    ``kb-processor`` console script.

    Reads JSON from stdin, dispatches to the pipeline, writes result JSON to
    stdout, and exits with an appropriate code.
    """
    # ------------------------------------------------------------------
    # 1. Read JSON from stdin.
    # ------------------------------------------------------------------
    try:
        raw = sys.stdin.read()
    except Exception as exc:
        _emit(
            ProcessorResultError(
                error=f"Failed to read from stdin: {exc}",
                retryable=True,
                metadata={"step": "stdin_read"},
            )
        )
        sys.exit(1)

    # ------------------------------------------------------------------
    # 2. Parse into ProcessorInput.
    # ------------------------------------------------------------------
    try:
        data = json.loads(raw)
        processor_input = ProcessorInput.model_validate(data)
    except json.JSONDecodeError as exc:
        _emit(
            ProcessorResultError(
                error=f"Invalid JSON on stdin: {exc}",
                retryable=False,
                metadata={"step": "json_parse"},
            )
        )
        sys.exit(1)
    except Exception as exc:
        _emit(
            ProcessorResultError(
                error=f"Failed to parse processor input: {exc}",
                retryable=False,
                metadata={"step": "input_validation"},
            )
        )
        sys.exit(1)

    logger.info(
        "Processing job_id=%d attempt=%d path=%s",
        processor_input.job_id,
        processor_input.attempt,
        processor_input.input_path,
    )

    # ------------------------------------------------------------------
    # 3. Run the pipeline.
    # ------------------------------------------------------------------
    try:
        result = pipeline.process(processor_input)
    except Exception as exc:
        logger.exception("Unhandled exception in pipeline.process")
        _emit(
            ProcessorResultError(
                error=f"Unhandled pipeline exception: {type(exc).__name__}: {exc}",
                retryable=True,
                metadata={"step": "pipeline"},
            )
        )
        sys.exit(1)

    # ------------------------------------------------------------------
    # 4. Emit result as the last line of stdout and exit.
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
