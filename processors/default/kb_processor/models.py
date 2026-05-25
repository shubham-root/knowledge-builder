"""
Pydantic models for the Knowledge Builder processor contract (PLAN.md §8).

Input JSON (received on stdin):
    {
        "input_path": "/Users/me/Vault/Sources/foo.pdf",
        "content_hash": "sha256:9af1...",
        "vault_root": "/Users/me/Vault",
        "sources_dir": "/Users/me/Vault/Sources",
        "work_dir": "/Users/me/Library/Caches/knowledge-builder/jobs/9af1.../",
        "job_id": 12345,
        "attempt": 1
    }

Output JSON (last line of stdout on success):
    {
        "status": "ok",
        "outputs": [
            {"path": "/Users/me/Vault/Notes/Foo.md", "kind": "markdown", "bytes": 8421},
            {"path": "/Users/me/Vault/Notes/Foo-figures/p1.png", "kind": "asset", "bytes": 102934}
        ],
        "metadata": {"model": "gpt-4o-mini", "tokens_in": 12345, "tokens_out": 678}
    }

Output JSON on failure:
    {
        "status": "error",
        "error": "extractor 'pdf' failed: ...",
        "retryable": true,
        "metadata": {"step": "extract"}
    }
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Literal

from pydantic import BaseModel, Field


# ---------------------------------------------------------------------------
# Input model
# ---------------------------------------------------------------------------


class ProcessorInput(BaseModel):
    """JSON object received on stdin when the processor is invoked."""

    input_path: Path = Field(
        description="Absolute path to the source file being processed."
    )
    content_hash: str = Field(
        description='SHA-256 digest of the file, prefixed with "sha256:".'
    )
    vault_root: Path = Field(
        description="Absolute path to the Obsidian vault root directory."
    )
    sources_dir: Path = Field(
        description="Absolute path to the sources sub-directory inside the vault."
    )
    agent_root: Path | None = Field(
        default=None,
        description=(
            "Absolute path to the agent's mutation sandbox (a strict "
            "sub-directory of vault_root, disjoint from sources_dir).  "
            "All agent-driven writes are confined to this tree.  When "
            "absent, defaults to vault_root/KnowledgeBase."
        ),
    )
    work_dir: Path = Field(
        description=(
            "Absolute path to the per-job working directory.  "
            "The processor SHOULD write transient artefacts here; "
            "the daemon cleans this directory after success."
        )
    )
    job_id: int = Field(description="Numeric primary key of this job in the daemon DB.")
    attempt: int = Field(
        ge=1, description="1-based attempt counter (1 on first try, 2 on first retry, …)."
    )

    class Config:
        # Allow extra fields to be forward-compatible with future daemon versions.
        extra = "ignore"


# ---------------------------------------------------------------------------
# Output models
# ---------------------------------------------------------------------------


class OutputEntry(BaseModel):
    """A single file produced by the processor."""

    path: Path = Field(
        description=(
            "Absolute path to the output file.  "
            "MUST reside inside vault_root and MUST NOT reside inside sources_dir."
        )
    )
    kind: str = Field(
        description='Descriptor string, e.g. "markdown" or "asset".',
        examples=["markdown", "asset"],
    )
    bytes: int = Field(ge=0, description="Size of the output file in bytes.")


class ProcessorResultOk(BaseModel):
    """Successful processing result."""

    status: Literal["ok"] = "ok"
    outputs: list[OutputEntry] = Field(
        default_factory=list,
        description="List of files written by the processor.",
    )
    metadata: dict[str, Any] | None = Field(
        default=None,
        description="Optional free-form metadata (model name, token counts, …).",
    )

    def to_json_line(self) -> str:
        """Serialise to the compact JSON string expected on the last stdout line."""
        return self.model_dump_json(exclude_none=True)


class ProcessorResultError(BaseModel):
    """Failed processing result."""

    status: Literal["error"] = "error"
    error: str = Field(description="Human-readable description of the failure.")
    retryable: bool = Field(
        default=True,
        description=(
            "Whether the daemon should retry this job after a backoff delay.  "
            "Set to False for permanent failures (e.g. unsupported file format)."
        ),
    )
    metadata: dict[str, Any] | None = Field(
        default=None,
        description="Optional free-form metadata (e.g. which pipeline step failed).",
    )

    def to_json_line(self) -> str:
        """Serialise to the compact JSON string expected on the last stdout line."""
        return self.model_dump_json(exclude_none=True)


# Union type for callers that want to accept either variant.
ProcessorResult = ProcessorResultOk | ProcessorResultError
