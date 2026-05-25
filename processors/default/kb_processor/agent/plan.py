"""
Plan reader/writer for the Knowledge Builder agent.

The wrapper script (:file:`agent/wrappers/kb-obsidian`) appends one JSON
object per intercepted mutation to a JSONL file at ``$KB_PLAN_FILE``.
This module is the *Python* side of that protocol: it reads, validates,
and serialises the plan for downstream consumers (the daemon's writer
stage and ``kb show`` ops output).

The wrapper and reader are deliberately decoupled — the wrapper is a
self-contained CLI script that knows nothing about Python imports beyond
``json``/``os``/``subprocess``.  This module imports nothing from the
wrapper.  Both must agree on the JSONL schema (documented below).

JSONL schema (one object per line)
----------------------------------
::

    {
        "ts":         <int>            # unix epoch seconds
        "mode":       "shadow" | "apply"
        "cmd":        <string>         # obsidian subcommand, e.g. "create"
        "args":       <list[str]>      # raw `key=value` / flag tokens as the
                                       # agent passed them
        "applied":    <bool>           # true only in apply mode after passthrough
        "exit_code":  <int>            # present only when applied=true (or false
                                       # after passthrough failed)
    }
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterator


# ── Public types ──────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class PlanEntry:
    """One staged mutation parsed from a kb-obsidian plan file."""
    ts:        int
    mode:      str            # "shadow" | "apply"
    cmd:       str
    args:      tuple[str, ...]
    applied:   bool
    exit_code: int | None = None

    @property
    def kind(self) -> str:
        """Coarse category for ops output (used by ``kb show``)."""
        if self.cmd in {"create"}:
            return "create"
        if self.cmd in {"append", "prepend", "daily:append", "daily:prepend"}:
            return "append"
        if self.cmd in {"property:set"}:
            return "property_set"
        if self.cmd in {"property:remove"}:
            return "property_remove"
        if self.cmd in {"move", "rename"}:
            return "rename"
        if self.cmd in {"delete"}:
            return "delete"
        if self.cmd in {"bookmark"}:
            return "bookmark"
        if self.cmd.startswith("base:"):
            return "base"
        return "other"


@dataclass
class Plan:
    """The complete set of mutations from one agent run."""
    path:    Path
    entries: list[PlanEntry] = field(default_factory=list)

    def __len__(self) -> int:
        return len(self.entries)

    def by_kind(self) -> dict[str, list[PlanEntry]]:
        out: dict[str, list[PlanEntry]] = {}
        for e in self.entries:
            out.setdefault(e.kind, []).append(e)
        return out

    def summary(self) -> str:
        if not self.entries:
            return "(empty plan — agent proposed no mutations)"
        bk = self.by_kind()
        parts = [f"{kind}={len(items)}" for kind, items in sorted(bk.items())]
        applied = sum(1 for e in self.entries if e.applied)
        head = f"plan({len(self.entries)} entries; applied={applied}): "
        return head + ", ".join(parts)


# ── Parsing ───────────────────────────────────────────────────────────────────


class PlanParseError(Exception):
    """Raised when a plan file contains a malformed JSONL line."""


def _coerce(raw: dict, line_no: int, path: Path) -> PlanEntry:
    """Validate the required keys and types of one JSONL record."""
    required = ("ts", "mode", "cmd", "args", "applied")
    for k in required:
        if k not in raw:
            raise PlanParseError(
                f"{path}:{line_no}: plan entry missing required key {k!r}: {raw!r}"
            )
    if not isinstance(raw["args"], list) or not all(isinstance(a, str) for a in raw["args"]):
        raise PlanParseError(
            f"{path}:{line_no}: plan entry 'args' must be list[str]: {raw!r}"
        )
    if raw["mode"] not in ("shadow", "apply"):
        raise PlanParseError(
            f"{path}:{line_no}: plan entry 'mode' must be 'shadow' or 'apply': {raw!r}"
        )
    return PlanEntry(
        ts        = int(raw["ts"]),
        mode      = str(raw["mode"]),
        cmd       = str(raw["cmd"]),
        args      = tuple(raw["args"]),
        applied   = bool(raw["applied"]),
        exit_code = (int(raw["exit_code"]) if "exit_code" in raw and raw["exit_code"] is not None else None),
    )


def read_plan(path: Path) -> Plan:
    """Load a plan from disk.  Returns an empty :class:`Plan` if the file
    does not exist (this is the common case when the agent finished
    without proposing any mutations).
    """
    plan = Plan(path=path)
    if not path.exists():
        return plan

    with path.open("r", encoding="utf-8") as f:
        for line_no, raw_line in enumerate(f, start=1):
            line = raw_line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError as exc:
                raise PlanParseError(
                    f"{path}:{line_no}: invalid JSON: {exc}"
                ) from exc
            if not isinstance(obj, dict):
                raise PlanParseError(
                    f"{path}:{line_no}: top-level value must be an object, "
                    f"got {type(obj).__name__}"
                )
            plan.entries.append(_coerce(obj, line_no, path))
    return plan


def iter_plan(path: Path) -> Iterator[PlanEntry]:
    """Streaming variant of :func:`read_plan` — yields one entry at a time
    without holding the entire file in memory.  Useful for very long plans
    (tens of thousands of operations).
    """
    if not path.exists():
        return
    with path.open("r", encoding="utf-8") as f:
        for line_no, raw_line in enumerate(f, start=1):
            line = raw_line.strip()
            if not line:
                continue
            obj = json.loads(line)
            yield _coerce(obj, line_no, path)
