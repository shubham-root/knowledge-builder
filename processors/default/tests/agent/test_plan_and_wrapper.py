"""
Unit tests for kb_processor.agent.plan + the kb-obsidian shell wrapper.

The wrapper is exercised as a real subprocess (it's the contract between
us and pi).  Read commands fall through to a stub ``obsidian`` we plant
on PATH; write commands are inspected via the JSONL plan file the
wrapper appends to.

Run::

    ~/.local/share/kb/venv/bin/python3 \\
        processors/default/tests/agent/test_plan_and_wrapper.py
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# Package-relative imports.
_HERE = Path(__file__).resolve()
_SRC  = _HERE.parents[2]            # processors/default
sys.path.insert(0, str(_SRC))

from kb_processor.agent.plan import (         # noqa: E402  pylint: disable=wrong-import-position
    Plan,
    PlanEntry,
    PlanParseError,
    read_plan,
)


# Path to the actual wrapper script under test.
_WRAPPER_PATH = (
    _SRC / "kb_processor" / "agent" / "wrappers" / "kb-obsidian"
)


# ---------------------------------------------------------------------------
# Plan reader tests
# ---------------------------------------------------------------------------


class PlanReaderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "p.jsonl"

    def _write_lines(self, lines: list[str]) -> None:
        self.path.write_text("\n".join(lines) + "\n", encoding="utf-8")

    def test_missing_file_returns_empty_plan(self) -> None:
        plan = read_plan(self.path)
        self.assertEqual(len(plan), 0)
        self.assertEqual(plan.summary(), "(empty plan — agent proposed no mutations)")

    def test_parses_well_formed_entries(self) -> None:
        self._write_lines([
            json.dumps({"ts": 1, "mode": "shadow", "cmd": "create",
                        "args": ["path=Foo.md", "content=hi"], "applied": False}),
            json.dumps({"ts": 2, "mode": "shadow", "cmd": "append",
                        "args": ["file=Foo", "content=more"], "applied": False}),
            json.dumps({"ts": 3, "mode": "apply", "cmd": "delete",
                        "args": ["file=Bar"], "applied": True, "exit_code": 0}),
        ])
        plan = read_plan(self.path)
        self.assertEqual(len(plan), 3)
        self.assertEqual(
            sorted(plan.by_kind().keys()),
            ["append", "create", "delete"],
        )
        self.assertEqual(plan.entries[2].applied, True)
        self.assertEqual(plan.entries[2].exit_code, 0)

    def test_blank_lines_ignored(self) -> None:
        self._write_lines([
            "",
            json.dumps({"ts": 1, "mode": "shadow", "cmd": "create",
                        "args": [], "applied": False}),
            "",
        ])
        plan = read_plan(self.path)
        self.assertEqual(len(plan), 1)

    def test_missing_required_key_raises(self) -> None:
        self._write_lines([
            json.dumps({"ts": 1, "mode": "shadow", "args": [], "applied": False}),
        ])
        with self.assertRaises(PlanParseError):
            read_plan(self.path)

    def test_invalid_json_raises(self) -> None:
        self.path.write_text("not json at all\n", encoding="utf-8")
        with self.assertRaises(PlanParseError):
            read_plan(self.path)

    def test_top_level_array_rejected(self) -> None:
        self.path.write_text(json.dumps([{"ts": 1}]) + "\n", encoding="utf-8")
        with self.assertRaises(PlanParseError):
            read_plan(self.path)

    def test_invalid_mode_rejected(self) -> None:
        self._write_lines([
            json.dumps({"ts": 1, "mode": "live", "cmd": "create",
                        "args": [], "applied": False}),
        ])
        with self.assertRaises(PlanParseError):
            read_plan(self.path)

    def test_kind_categorisation(self) -> None:
        self.assertEqual(
            PlanEntry(ts=0, mode="shadow", cmd="create",  args=(), applied=False).kind, "create",
        )
        self.assertEqual(
            PlanEntry(ts=0, mode="shadow", cmd="rename",  args=(), applied=False).kind, "rename",
        )
        self.assertEqual(
            PlanEntry(ts=0, mode="shadow", cmd="property:set", args=(), applied=False).kind,
            "property_set",
        )
        self.assertEqual(
            PlanEntry(ts=0, mode="shadow", cmd="some-future-cmd", args=(), applied=False).kind,
            "other",
        )


# ---------------------------------------------------------------------------
# Wrapper subprocess tests
# ---------------------------------------------------------------------------


def _make_stub_obsidian(tmp_dir: Path, *, exit_code: int = 0, output: str = "stub-ok\n") -> Path:
    """Create a fake `obsidian` executable that records its argv and
    prints ``output`` to stdout.

    Returns the directory containing the stub so callers can prepend
    it to PATH.
    """
    bin_dir = tmp_dir / "stub-bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    stub = bin_dir / "obsidian"
    log  = tmp_dir / "stub.log"
    stub.write_text(
        "#!/bin/sh\n"
        f'echo "$@" > "{log}"\n'
        f'printf "%s" "{output}"\n'
        f"exit {exit_code}\n",
        encoding="utf-8",
    )
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return bin_dir


def _run_wrapper(
    *args: str,
    plan_file: Path,
    mode: str,
    stub_bin_dir: Path,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    env = {
        **os.environ,
        "PATH":           f"{stub_bin_dir}{os.pathsep}{os.environ['PATH']}",
        "KB_PLAN_FILE":   str(plan_file),
        "KB_AGENT_MODE":  mode,
    }
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        [sys.executable, str(_WRAPPER_PATH), *args],
        env=env,
        capture_output=True,
        text=True,
    )


class WrapperTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tmp_dir   = Path(self.tmp.name)
        self.plan_file = self.tmp_dir / "plan.jsonl"
        self.bin_dir   = _make_stub_obsidian(self.tmp_dir)

    # ── Read passthrough ──

    def test_read_command_passes_through(self) -> None:
        r = _run_wrapper(
            "files", "total",
            plan_file=self.plan_file, mode="shadow", stub_bin_dir=self.bin_dir,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("stub-ok", r.stdout)
        # No plan entries for read commands.
        self.assertFalse(self.plan_file.exists() and self.plan_file.read_text().strip())

    def test_read_passes_through_args_unchanged(self) -> None:
        _run_wrapper(
            "search", 'query=meeting notes', "format=json",
            plan_file=self.plan_file, mode="shadow", stub_bin_dir=self.bin_dir,
        )
        log = (self.tmp_dir / "stub.log").read_text()
        # The stub records `$@`, so 'query=meeting notes' becomes one token.
        self.assertIn("search", log)
        self.assertIn("query=meeting notes", log)
        self.assertIn("format=json", log)

    def test_read_passes_through_nonzero_exit(self) -> None:
        bad_bin = _make_stub_obsidian(self.tmp_dir, exit_code=2, output="boom\n")
        r = _run_wrapper(
            "read", "file=Foo",
            plan_file=self.plan_file, mode="shadow", stub_bin_dir=bad_bin,
        )
        self.assertEqual(r.returncode, 2)

    # ── Write in shadow mode ──

    def test_write_in_shadow_logs_plan_and_skips_obsidian(self) -> None:
        r = _run_wrapper(
            "create", "path=Topics/Foo.md", 'content=Hello, world',
            plan_file=self.plan_file, mode="shadow", stub_bin_dir=self.bin_dir,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(
            (self.tmp_dir / "stub.log").exists(),
            "obsidian must NOT be invoked in shadow mode for write commands",
        )
        plan = read_plan(self.plan_file)
        self.assertEqual(len(plan), 1)
        self.assertEqual(plan.entries[0].cmd, "create")
        self.assertEqual(plan.entries[0].mode, "shadow")
        self.assertFalse(plan.entries[0].applied)
        self.assertIn("Hello, world", plan.entries[0].args[1])

    def test_shadow_returns_mock_success_json(self) -> None:
        r = _run_wrapper(
            "delete", "file=Foo",
            plan_file=self.plan_file, mode="shadow", stub_bin_dir=self.bin_dir,
        )
        out = json.loads(r.stdout.strip())
        self.assertEqual(out["status"], "ok")
        self.assertEqual(out["mode"],   "shadow")
        self.assertEqual(out["cmd"],    "delete")

    # ── Write in apply mode ──

    def test_write_in_apply_passes_through(self) -> None:
        r = _run_wrapper(
            "create", "path=Foo.md", "content=hi",
            plan_file=self.plan_file, mode="apply", stub_bin_dir=self.bin_dir,
        )
        self.assertEqual(r.returncode, 0)
        self.assertTrue((self.tmp_dir / "stub.log").exists())
        plan = read_plan(self.plan_file)
        self.assertEqual(len(plan), 1)
        self.assertTrue(plan.entries[0].applied)
        self.assertEqual(plan.entries[0].exit_code, 0)

    def test_apply_records_failure(self) -> None:
        bad_bin = _make_stub_obsidian(self.tmp_dir, exit_code=3, output="nope\n")
        r = _run_wrapper(
            "delete", "file=Foo",
            plan_file=self.plan_file, mode="apply", stub_bin_dir=bad_bin,
        )
        self.assertEqual(r.returncode, 3)
        plan = read_plan(self.plan_file)
        self.assertEqual(len(plan), 1)
        self.assertFalse(plan.entries[0].applied)
        self.assertEqual(plan.entries[0].exit_code, 3)

    # ── Blocked / unknown ──

    def test_blocked_command_rejected(self) -> None:
        r = _run_wrapper(
            "eval", 'code=42',
            plan_file=self.plan_file, mode="apply", stub_bin_dir=self.bin_dir,
        )
        self.assertEqual(r.returncode, 1)
        out = json.loads(r.stdout.strip())
        self.assertEqual(out["status"], "error")
        self.assertIn("blocked", out["error"])
        # Real obsidian must not have been invoked.
        self.assertFalse((self.tmp_dir / "stub.log").exists())

    def test_unknown_command_rejected(self) -> None:
        r = _run_wrapper(
            "frobnicate",
            plan_file=self.plan_file, mode="shadow", stub_bin_dir=self.bin_dir,
        )
        self.assertEqual(r.returncode, 1)
        out = json.loads(r.stdout.strip())
        self.assertEqual(out["status"], "error")
        self.assertIn("not in the kb-obsidian allowlist", out["error"])

    # ── Env validation ──

    def test_missing_plan_file_env_rejected(self) -> None:
        env = {
            **os.environ,
            "PATH":          f"{self.bin_dir}{os.pathsep}{os.environ['PATH']}",
            "KB_AGENT_MODE": "shadow",
        }
        env.pop("KB_PLAN_FILE", None)
        r = subprocess.run(
            [sys.executable, str(_WRAPPER_PATH), "files"],
            env=env, capture_output=True, text=True,
        )
        self.assertEqual(r.returncode, 1)
        out = json.loads(r.stdout.strip())
        self.assertIn("KB_PLAN_FILE", out["error"])

    def test_invalid_mode_rejected(self) -> None:
        r = _run_wrapper(
            "files",
            plan_file=self.plan_file, mode="bogus", stub_bin_dir=self.bin_dir,
        )
        self.assertEqual(r.returncode, 1)
        out = json.loads(r.stdout.strip())
        self.assertIn("KB_AGENT_MODE", out["error"])

    # ── No env mutation ──

    def test_shadow_does_not_create_artefacts_in_vault(self) -> None:
        """Belt-and-braces: even if the stub is configured oddly, shadow
        mode must never invoke obsidian, full stop."""
        for cmd, args in [
            ("create",   ["path=foo.md", "content=x"]),
            ("append",   ["file=foo",    "content=x"]),
            ("delete",   ["file=foo"]),
            ("move",     ["file=foo",    "to=bar"]),
            ("rename",   ["file=foo",    "name=baz"]),
            ("property:set",    ["name=tag", "value=t",  "file=foo"]),
            ("property:remove", ["name=tag", "file=foo"]),
        ]:
            with self.subTest(cmd=cmd):
                # Fresh stub log per iteration.
                if (self.tmp_dir / "stub.log").exists():
                    (self.tmp_dir / "stub.log").unlink()
                r = _run_wrapper(
                    cmd, *args,
                    plan_file=self.plan_file,
                    mode="shadow",
                    stub_bin_dir=self.bin_dir,
                )
                self.assertEqual(r.returncode, 0, f"{cmd} failed: {r.stderr}")
                self.assertFalse(
                    (self.tmp_dir / "stub.log").exists(),
                    f"{cmd}: obsidian was invoked in shadow mode!",
                )

    # ── Argument syntax & path invariant guards ──

    def test_dash_prefixed_argument_rejected(self) -> None:
        for bad_arg in ["--file=Foo", "--file", "-f"]:
            with self.subTest(arg=bad_arg):
                r = _run_wrapper(
                    "read", bad_arg,
                    plan_file=self.plan_file,
                    mode="shadow",
                    stub_bin_dir=self.bin_dir,
                )
                self.assertEqual(r.returncode, 1, f"{bad_arg} should fail")
                out = json.loads(r.stdout.strip())
                self.assertIn("POSIX-style", out["error"])

    def test_path_inside_sources_dir_rejected(self) -> None:
        """Write commands whose ``path=...`` resolves under sources_dir
        must be rejected, regardless of mode."""
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        sources_dir.mkdir(parents=True)

        for path_arg in [
            "path=Sources/widgets.md",
            f"path={sources_dir}/widgets.md",
        ]:
            with self.subTest(path=path_arg):
                r = _run_wrapper(
                    "create", path_arg, "content=hi",
                    plan_file=self.plan_file,
                    mode="shadow",
                    stub_bin_dir=self.bin_dir,
                    extra_env={
                        "KB_VAULT_ROOT":  str(vault_root),
                        "KB_SOURCES_DIR": str(sources_dir),
                    },
                )
                self.assertEqual(r.returncode, 1, f"{path_arg} should fail")
                out = json.loads(r.stdout.strip())
                self.assertIn("sources_dir", out["error"])

    def test_path_outside_vault_root_rejected(self) -> None:
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        sources_dir.mkdir(parents=True)

        outside = self.tmp_dir / "outside" / "foo.md"
        r = _run_wrapper(
            "create", f"path={outside}", "content=hi",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
            },
        )
        self.assertEqual(r.returncode, 1)
        out = json.loads(r.stdout.strip())
        self.assertIn("vault_root", out["error"])

    def test_relative_path_under_vault_root_accepted(self) -> None:
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        sources_dir.mkdir(parents=True)
        r = _run_wrapper(
            "create", "path=Notes/Hi.md", "content=hi",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
            },
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    # ── KB_AGENT_ROOT enforcement ──

    def test_path_outside_agent_root_rejected(self) -> None:
        """Even when a write path is inside the vault and outside
        sources_dir, it must also be inside ``KB_AGENT_ROOT`` (the
        agent's mutation sandbox)."""
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        agent_root  = vault_root / "KnowledgeBase"
        sources_dir.mkdir(parents=True)
        agent_root.mkdir(parents=True)

        for path_arg in [
            "path=Notes/Hi.md",                       # vault root, but outside KB
            "path=Topics/Foo.md",                     # vault root, but outside KB
            f"path={vault_root}/somewhere-else.md",   # absolute, outside KB
        ]:
            with self.subTest(path=path_arg):
                r = _run_wrapper(
                    "create", path_arg, "content=hi",
                    plan_file=self.plan_file,
                    mode="shadow",
                    stub_bin_dir=self.bin_dir,
                    extra_env={
                        "KB_VAULT_ROOT":  str(vault_root),
                        "KB_SOURCES_DIR": str(sources_dir),
                        "KB_AGENT_ROOT":  str(agent_root),
                    },
                )
                self.assertEqual(r.returncode, 1, f"{path_arg} should fail")
                out = json.loads(r.stdout.strip())
                self.assertIn("agent_root", out["error"])

    def test_path_inside_agent_root_accepted(self) -> None:
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        agent_root  = vault_root / "KnowledgeBase"
        sources_dir.mkdir(parents=True)
        agent_root.mkdir(parents=True)

        for path_arg in [
            "path=KnowledgeBase/Foo.md",
            "path=KnowledgeBase/Topics/Bar.md",
            f"path={agent_root}/abs.md",
        ]:
            with self.subTest(path=path_arg):
                if self.plan_file.exists():
                    self.plan_file.unlink()
                r = _run_wrapper(
                    "create", path_arg, "content=hi",
                    plan_file=self.plan_file,
                    mode="shadow",
                    stub_bin_dir=self.bin_dir,
                    extra_env={
                        "KB_VAULT_ROOT":  str(vault_root),
                        "KB_SOURCES_DIR": str(sources_dir),
                        "KB_AGENT_ROOT":  str(agent_root),
                    },
                )
                self.assertEqual(r.returncode, 0, r.stderr)

    def test_agent_root_enforcement_skipped_when_unset(self) -> None:
        """When ``KB_AGENT_ROOT`` is not exported (legacy daemon, tests),
        the agent_root constraint is fail-safe: no enforcement, vault
        and sources_dir checks still apply."""
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        sources_dir.mkdir(parents=True)
        r = _run_wrapper(
            "create", "path=Notes/Hi.md", "content=hi",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
                # KB_AGENT_ROOT intentionally absent
            },
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    # ── file= and name= path-like enforcement (regression for the
    #     `file=Sources/Foo.md` bypass we observed in the live e2e test)

    def test_file_arg_with_slash_validated_against_agent_root(self) -> None:
        """When a write command uses ``file=<path>`` (path-style) with a
        ``/`` in the value, the wrapper must run the same vault /
        sources_dir / agent_root checks as it does for ``path=``."""
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        agent_root  = vault_root / "KnowledgeBase"
        sources_dir.mkdir(parents=True)
        agent_root.mkdir(parents=True)

        # `file=Sources/Foo.md` resolves into sources_dir; reject.
        r = _run_wrapper(
            "create", "file=Sources/Foo.md", "content=hi",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
                "KB_AGENT_ROOT":  str(agent_root),
            },
        )
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("sources_dir", json.loads(r.stdout)["error"])

        # `file=Legal Documents/Foo.md` resolves outside agent_root; reject.
        r = _run_wrapper(
            "create", "file=Legal Documents/Foo.md", "content=hi",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
                "KB_AGENT_ROOT":  str(agent_root),
            },
        )
        self.assertEqual(r.returncode, 1, r.stdout)
        self.assertIn("agent_root", json.loads(r.stdout)["error"])

    def test_bare_file_wikilink_skips_path_validation(self) -> None:
        """`file=Foo` (no slash) is a wikilink-style targetting; the
        wrapper cannot pre-resolve it without running obsidian, so it
        is allowed to pass through to the obsidian binary."""
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        agent_root  = vault_root / "KnowledgeBase"
        sources_dir.mkdir(parents=True)
        agent_root.mkdir(parents=True)

        r = _run_wrapper(
            "append", "file=Foo", "content=more",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
                "KB_AGENT_ROOT":  str(agent_root),
            },
        )
        self.assertEqual(r.returncode, 0, r.stdout)

    def test_name_arg_with_slash_validated(self) -> None:
        """`name=` is treated the same as `path=` when it contains a slash.
        Bare names (no slash) are unaffected."""
        vault_root  = self.tmp_dir / "vault"
        sources_dir = vault_root / "Sources"
        agent_root  = vault_root / "KnowledgeBase"
        sources_dir.mkdir(parents=True)
        agent_root.mkdir(parents=True)

        # name=Sources/Foo: should be rejected.
        r = _run_wrapper(
            "create", "name=Sources/Foo.md", "content=hi",
            plan_file=self.plan_file,
            mode="shadow",
            stub_bin_dir=self.bin_dir,
            extra_env={
                "KB_VAULT_ROOT":  str(vault_root),
                "KB_SOURCES_DIR": str(sources_dir),
                "KB_AGENT_ROOT":  str(agent_root),
            },
        )
        self.assertEqual(r.returncode, 1)
        self.assertIn("sources_dir", json.loads(r.stdout)["error"])


# ---------------------------------------------------------------------------


if __name__ == "__main__":
    unittest.main(verbosity=2)
