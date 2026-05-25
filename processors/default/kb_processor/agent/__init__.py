"""
Knowledge Builder agent subpackage.

Components:
* :mod:`indexer`     — read-only SQLite FTS5 index of the vault.  Kept for
                       offline ops use; the production agent path now talks
                       to Obsidian's CLI directly via the ``kb-obsidian``
                       wrapper, so the indexer is no longer on the hot path.
* :mod:`plan`        — JSONL plan reader / writer protocol.
* :mod:`rpc_driver`  — spawns ``pi --mode rpc``, drives the integration
                       prompt, returns a :class:`Plan`.
* ``wrappers/kb-obsidian`` — policy wrapper around Obsidian's CLI.  In
                       shadow mode it intercepts mutations and records
                       them to the plan file; in apply mode it passes
                       through.
* ``skills/``        — skill files (``SKILL.md``, ``obsidian-cli.md``,
                       ``integration-playbook.md``) loaded by pi.

Public surface
--------------
* :class:`indexer.VaultIndex`
* :class:`plan.Plan`
* :class:`plan.PlanEntry`
* :func:`rpc_driver.run_agent`
* :class:`rpc_driver.AgentInput`
* :class:`rpc_driver.AgentResult`
"""

from __future__ import annotations

from .indexer    import VaultIndex, NoteRecord, IndexStats
from .plan       import Plan, PlanEntry, read_plan, iter_plan, PlanParseError
from .link_sweeper import (
    UnresolvedLink,
    SweepStats,
    sweep_files,
    sweep_links_in_text,
    files_touched_by_plan,
)
from .rpc_driver import (
    run_agent,
    AgentInput,
    AgentResult,
    AgentError,
    AgentBudgetError,
    MissingApiKeyError,
    PiNotFoundError,
    PiProtocolError,
    PiSpawnError,
    PlanCorruptError,
)

__all__ = [
    # Indexer (legacy; offline use)
    "VaultIndex",
    "NoteRecord",
    "IndexStats",
    # Plan
    "Plan",
    "PlanEntry",
    "read_plan",
    "iter_plan",
    "PlanParseError",
    # Link sweeper (post-run wikilink cleanup)
    "UnresolvedLink",
    "SweepStats",
    "sweep_files",
    "sweep_links_in_text",
    "files_touched_by_plan",
    # Driver
    "run_agent",
    "AgentInput",
    "AgentResult",
    "AgentError",
    "AgentBudgetError",
    "MissingApiKeyError",
    "PiNotFoundError",
    "PiProtocolError",
    "PiSpawnError",
    "PlanCorruptError",
]
