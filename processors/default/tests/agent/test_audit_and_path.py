"""
Unit tests for the rpc_driver vault-diff audit and PATH-staging helpers.

These are pure-Python unit tests — no pi subprocess, no LLM calls.
"""

from __future__ import annotations

import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

_HERE = Path(__file__).resolve()
_SRC  = _HERE.parents[2]
sys.path.insert(0, str(_SRC))

from kb_processor.agent.plan import Plan, PlanEntry        # noqa: E402
from kb_processor.agent.rpc_driver import (                # noqa: E402
    _AGENT_PATH_BINARIES,
    _audit_vault_diff,
    _planned_paths,
    _snapshot_vault,
    _stage_wrapper_on_path,
)


# ---------------------------------------------------------------------------
# Vault snapshot
# ---------------------------------------------------------------------------


class SnapshotTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.vault   = Path(self.tmp.name) / "vault"
        self.sources = self.vault / "Sources"
        (self.vault / ".obsidian").mkdir(parents=True)
        (self.vault / "Notes").mkdir(parents=True)
        self.sources.mkdir()

        # Plant some files.
        (self.vault / "Notes" / "user.md").write_text("user note\n", encoding="utf-8")
        (self.vault / "Notes" / "another.md").write_text("# another\n", encoding="utf-8")
        (self.sources / "input.pdf").write_text("PDF\n", encoding="utf-8")
        (self.vault / ".obsidian" / "workspace").write_text("ignore me\n")

    def test_snapshot_excludes_sources_and_obsidian(self) -> None:
        snap = _snapshot_vault(self.vault, self.sources)
        keys = set(snap.keys())
        self.assertTrue(any("user.md" in k for k in keys))
        self.assertTrue(any("another.md" in k for k in keys))
        # No source PDFs, no .obsidian state.
        self.assertFalse(any("input.pdf" in k for k in keys))
        self.assertFalse(any(".obsidian" in k for k in keys))

    def test_snapshot_returns_mtime_and_size_tuple(self) -> None:
        snap = _snapshot_vault(self.vault, self.sources)
        for v in snap.values():
            self.assertEqual(len(v), 2)
            self.assertIsInstance(v[0], int)
            self.assertIsInstance(v[1], int)


# ---------------------------------------------------------------------------
# Audit logic
# ---------------------------------------------------------------------------


def _plan_with_creates(vault_root: Path, *paths: str) -> Plan:
    """Build a fake Plan whose entries are ``create path=<each>``."""
    p = Plan(path=Path("/dev/null"))
    for rel in paths:
        p.entries.append(PlanEntry(
            ts=0, mode="shadow", cmd="create",
            args=(f"path={rel}", "content=stub"),
            applied=False, exit_code=None,
        ))
    return p


class AuditTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.vault = Path(self.tmp.name) / "vault"
        self.vault.mkdir()
        self.sources = self.vault / "Sources"
        self.sources.mkdir()

    def _write(self, rel: str, body: str = "x\n") -> Path:
        p = self.vault / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")
        return p

    # ── Happy path ──

    def test_clean_run_returns_empty_rogue_list(self) -> None:
        self._write("Notes/user.md")
        before = _snapshot_vault(self.vault, self.sources)
        # No changes between snapshots.
        after = _snapshot_vault(self.vault, self.sources)
        plan  = _plan_with_creates(self.vault)
        self.assertEqual(_audit_vault_diff(before, after, plan, self.vault), [])

    def test_planned_create_is_not_rogue(self) -> None:
        before = _snapshot_vault(self.vault, self.sources)
        # Simulate the agent creating exactly the file in its plan.
        time.sleep(0.01)
        new_file = self._write("KnowledgeBase/Foo.md")
        after = _snapshot_vault(self.vault, self.sources)
        plan = _plan_with_creates(self.vault, "KnowledgeBase/Foo.md")
        self.assertEqual(_audit_vault_diff(before, after, plan, self.vault), [])

    # ── Rogue write detection ──

    def test_rogue_create_outside_plan_is_flagged(self) -> None:
        before = _snapshot_vault(self.vault, self.sources)
        time.sleep(0.01)
        # Agent wrote `Legal Documents/...` via raw bash — not in plan.
        rogue = self._write("Legal Documents/Properties/Foo.md")
        after = _snapshot_vault(self.vault, self.sources)
        plan = _plan_with_creates(self.vault, "KnowledgeBase/SomethingElse.md")
        result = _audit_vault_diff(before, after, plan, self.vault)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].resolve(), rogue.resolve())

    def test_rogue_modification_is_flagged(self) -> None:
        target = self._write("Notes/user.md", "old\n")
        before = _snapshot_vault(self.vault, self.sources)
        time.sleep(0.01)
        # Agent overwrote the user note.
        target.write_text("HIJACKED\n", encoding="utf-8")
        os.utime(target, None)
        after = _snapshot_vault(self.vault, self.sources)
        plan = _plan_with_creates(self.vault)
        result = _audit_vault_diff(before, after, plan, self.vault)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].resolve(), target.resolve())

    def test_rogue_deletion_is_flagged(self) -> None:
        target = self._write("Notes/user.md")
        before = _snapshot_vault(self.vault, self.sources)
        time.sleep(0.01)
        target.unlink()
        after = _snapshot_vault(self.vault, self.sources)
        plan = _plan_with_creates(self.vault)
        result = _audit_vault_diff(before, after, plan, self.vault)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].resolve(), target.resolve())

    def test_multiple_rogue_writes_aggregated(self) -> None:
        before = _snapshot_vault(self.vault, self.sources)
        time.sleep(0.01)
        a = self._write("Legal/Foo.md")
        b = self._write("Other/Bar.md")
        c = self._write("And/Baz.md")
        after = _snapshot_vault(self.vault, self.sources)
        plan = _plan_with_creates(self.vault)
        result = _audit_vault_diff(before, after, plan, self.vault)
        self.assertEqual(len(result), 3)
        resolved = {p.resolve() for p in result}
        self.assertIn(a.resolve(), resolved)
        self.assertIn(b.resolve(), resolved)
        self.assertIn(c.resolve(), resolved)


# ---------------------------------------------------------------------------
# Wrapper PATH staging
# ---------------------------------------------------------------------------


class WrapperPathTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def test_kb_obsidian_present(self) -> None:
        wrappers_dir = _stage_wrapper_on_path(Path(self.tmp.name))
        kb_link = wrappers_dir / "kb-obsidian"
        self.assertTrue(kb_link.exists() or kb_link.is_symlink())
        # The link target is our package's wrapper script.
        target = os.readlink(kb_link)
        self.assertIn("kb-obsidian", target)

    def test_readonly_utilities_symlinked_when_available(self) -> None:
        wrappers_dir = _stage_wrapper_on_path(Path(self.tmp.name))
        # On macOS/Linux these always exist.
        for name in ("cat", "head", "tail", "sed", "grep", "sh", "bash"):
            link = wrappers_dir / name
            self.assertTrue(
                link.exists() or link.is_symlink(),
                f"{name} must be present in the agent's PATH dir",
            )

    def test_dangerous_utilities_NOT_in_wrapper_dir(self) -> None:
        wrappers_dir = _stage_wrapper_on_path(Path(self.tmp.name))
        # These would let the agent write to the vault directly.
        # ``python3``, ``node``, ``npm``, ``npx`` are intentionally
        # present — documented in _AGENT_PATH_BINARIES — because pi (and
        # the kb-obsidian wrapper, which is a Python script) need them.
        # The post-run vault-diff audit catches any abuse via those
        # interpreters.
        for name in ("mkdir", "cp", "mv", "rm", "tee", "touch", "chmod",
                     "git", "curl", "wget", "ssh", "pip"):
            link = wrappers_dir / name
            self.assertFalse(
                link.exists() or link.is_symlink(),
                f"{name} must NOT be on the agent's PATH (it is)",
            )

    def test_resaging_is_idempotent(self) -> None:
        d = Path(self.tmp.name)
        out1 = _stage_wrapper_on_path(d)
        out2 = _stage_wrapper_on_path(d)
        self.assertEqual(out1, out2)
        # Re-staging should not error or duplicate anything.
        self.assertTrue((out1 / "kb-obsidian").exists())


# ---------------------------------------------------------------------------
# planned_paths
# ---------------------------------------------------------------------------


class PlannedPathsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.vault = Path(self.tmp.name) / "v"
        self.vault.mkdir()

    def test_extracts_path_from_create_args(self) -> None:
        plan = Plan(path=Path("/dev/null"))
        plan.entries.append(PlanEntry(
            ts=0, mode="shadow", cmd="create",
            args=("path=KnowledgeBase/Foo.md", "content=hi"),
            applied=False,
        ))
        out = _planned_paths(plan, self.vault)
        self.assertIn(str((self.vault / "KnowledgeBase/Foo.md").resolve()), out)

    def test_extracts_to_for_move(self) -> None:
        plan = Plan(path=Path("/dev/null"))
        plan.entries.append(PlanEntry(
            ts=0, mode="shadow", cmd="move",
            args=("file=Old", "to=KnowledgeBase/Archive/"),
            applied=False,
        ))
        out = _planned_paths(plan, self.vault)
        self.assertTrue(any("KnowledgeBase/Archive" in p for p in out))

    def test_records_with_and_without_md_suffix(self) -> None:
        plan = Plan(path=Path("/dev/null"))
        plan.entries.append(PlanEntry(
            ts=0, mode="shadow", cmd="create",
            args=("path=KnowledgeBase/Foo", "content=hi"),  # no .md suffix
            applied=False,
        ))
        out = _planned_paths(plan, self.vault)
        # Both forms should be present (Obsidian appends .md automatically).
        self.assertTrue(any(p.endswith("Foo") for p in out))
        self.assertTrue(any(p.endswith("Foo.md") for p in out))


# ---------------------------------------------------------------------------


if __name__ == "__main__":
    unittest.main(verbosity=2)
