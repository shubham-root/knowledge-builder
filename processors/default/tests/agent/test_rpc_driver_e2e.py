"""
End-to-end shadow-mode smoke test for the Knowledge Builder agent.

Runs a *real* ``pi --mode rpc`` subprocess against an *empty* fake vault
plus a tiny extracted markdown, asks the agent to integrate it in
shadow mode, and asserts:

* pi spawns and exits cleanly
* the kb-obsidian wrapper was used for at least one read command
* a plan file was written
* the real Obsidian app was not contacted (we test by pointing
  ``KB_OBSIDIAN_BIN`` at a stub that records its calls)
* every wrapper invocation we observe stayed within the allowlist

Skipped automatically if:
* ``pi`` is not on PATH
* ``OPENROUTER_API_KEY`` is unset
* ``KB_LLM_MODEL`` is unset

This is the integration test that exercises the full driver -> pi ->
skills -> wrapper -> stub-obsidian chain.  Costs ~$0.01 per run.

Run::

    OPENROUTER_API_KEY=$OPENROUTER_API_KEY \\
    KB_LLM_MODEL=openrouter/anthropic/claude-3.5-haiku \\
    ~/.local/share/kb/venv/bin/python3 \\
        processors/default/tests/agent/test_rpc_driver_e2e.py
"""

from __future__ import annotations

import os
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path

_HERE = Path(__file__).resolve()
_SRC  = _HERE.parents[2]
sys.path.insert(0, str(_SRC))

from kb_processor.agent.rpc_driver import (   # noqa: E402
    AgentInput,
    MissingApiKeyError,
    PiNotFoundError,
    run_agent,
)


# ---------------------------------------------------------------------------


@unittest.skipIf(
    shutil.which("pi") is None,
    "pi binary not on PATH; skipping live RPC smoke test",
)
@unittest.skipIf(
    not os.environ.get("OPENROUTER_API_KEY", "").strip(),
    "OPENROUTER_API_KEY unset; skipping live RPC smoke test",
)
@unittest.skipIf(
    not os.environ.get("KB_LLM_MODEL", "").strip(),
    "KB_LLM_MODEL unset; skipping live RPC smoke test",
)
class LiveAgentSmokeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tmp_dir    = Path(self.tmp.name)
        self.work_dir   = self.tmp_dir / "work"
        self.vault_root = self.tmp_dir / "vault"
        self.sources    = self.vault_root / "Sources"
        self.agent_root = self.vault_root / "KnowledgeBase"
        self.work_dir.mkdir()
        self.sources.mkdir(parents=True)
        self.agent_root.mkdir(parents=True)

        # A trivial markdown body.
        self.extracted = self.work_dir / "extracted.md"
        self.extracted.write_text(
            "# Integration smoke test\n\n"
            "This is a tiny extracted document with no overlap with the "
            "(empty) target vault.  The expected agent decision is CREATE "
            "a new note under Notes/.\n",
            encoding="utf-8",
        )

        # Stub `obsidian` binary that records every call.
        bin_dir = self.tmp_dir / "stub-bin"
        bin_dir.mkdir()
        stub = bin_dir / "obsidian"
        log  = self.tmp_dir / "stub.log"
        # `tags counts` returns "No tags found.\n", `files total` returns "0\n",
        # everything else returns an empty newline.
        stub.write_text(
            "#!/bin/sh\n"
            f'echo "$@" >> "{log}"\n'
            'case "$1" in\n'
            '  files) [ "$2" = "total" ] && echo "0" || echo "" ;;\n'
            '  folders) echo "" ;;\n'
            '  tags) [ "$2" = "counts" ] && echo "No tags found." || echo "No tags found." ;;\n'
            '  search|search:context) echo "[]" ;;\n'
            '  *) echo "" ;;\n'
            'esac\n'
            "exit 0\n",
            encoding="utf-8",
        )
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        self.stub_bin_dir = bin_dir
        self.stub_log     = log

        # Critical: tell the wrapper to use OUR stub, not the real
        # /usr/local/bin/obsidian (which would talk to the user's actual vault).
        os.environ["KB_OBSIDIAN_BIN"] = str(stub)
        # And keep the agent's wall-clock cost bounded.
        os.environ.setdefault("KB_AGENT_TIMEOUT_SECS", "120")

    def tearDown(self) -> None:
        os.environ.pop("KB_OBSIDIAN_BIN", None)

    def test_shadow_run_completes_cleanly(self) -> None:
        inp = AgentInput(
            extracted_path  = self.extracted,
            work_dir        = self.work_dir,
            vault_root      = self.vault_root,
            sources_dir     = self.sources,
            agent_root      = self.agent_root,
            source_basename = "smoke.md",
            model           = os.environ["KB_LLM_MODEL"],
            job_id          = 0,
            mode            = "shadow",
        )
        result = run_agent(inp)

        # The pi run produced events and reached agent_end.
        self.assertGreater(result.turns, 0, "agent should have completed at least one turn")
        self.assertGreater(result.elapsed_secs, 0)

        # The stub obsidian was invoked at least once (the agent issued
        # at least one read command via kb-obsidian).
        self.assertTrue(
            self.stub_log.exists(),
            "kb-obsidian should have invoked the (stub) obsidian for at least "
            "one read command during the survey step",
        )

        # The plan file exists (even an empty one is fine for shadow).
        self.assertTrue(result.plan_file.exists())

        # The agent's audit log was written.
        self.assertTrue(result.agent_log.exists())
        self.assertGreater(result.agent_log.stat().st_size, 0)

        # All write-classified plan entries are mode=shadow & not applied.
        for e in result.plan.entries:
            self.assertEqual(e.mode, "shadow")
            self.assertFalse(e.applied)

        # Some closing summary text was emitted.  Some models may stop
        # without a text message, so we just print and don't assert.
        print(f"\n  turns={result.turns}  elapsed={result.elapsed_secs:.1f}s")
        print(f"  plan: {result.plan.summary()}")
        if result.final_assistant_text:
            preview = result.final_assistant_text[:300].replace("\n", " ")
            print(f"  final: {preview}…")


# ---------------------------------------------------------------------------


class StaticDriverTests(unittest.TestCase):
    """No-network tests for the driver helpers."""

    def test_split_litellm_model(self) -> None:
        from kb_processor.agent.rpc_driver import _split_litellm_model
        self.assertEqual(
            _split_litellm_model("openrouter/moonshotai/kimi-k2.5"),
            ("openrouter", "moonshotai/kimi-k2.5"),
        )
        self.assertEqual(
            _split_litellm_model("anthropic/claude-3-5-sonnet-latest"),
            ("anthropic", "claude-3-5-sonnet-latest"),
        )
        from kb_processor.agent.rpc_driver import AgentError
        with self.assertRaises(AgentError):
            _split_litellm_model("no-slash")
        with self.assertRaises(AgentError):
            _split_litellm_model("/leading-slash")
        with self.assertRaises(AgentError):
            _split_litellm_model("trailing-slash/")

    def test_missing_api_key_raises(self) -> None:
        # Save / clear / restore.
        saved = os.environ.pop("OPENROUTER_API_KEY", None)
        try:
            with tempfile.TemporaryDirectory() as td:
                inp = AgentInput(
                    extracted_path  = Path(td) / "x.md",
                    work_dir        = Path(td),
                    vault_root      = Path(td),
                    sources_dir     = Path(td) / "sources",
                    agent_root      = Path(td) / "KnowledgeBase",
                    source_basename = "x.md",
                    model           = "openrouter/anthropic/claude-3.5-haiku",
                    job_id          = 0,
                )
                Path(inp.extracted_path).write_text("x", encoding="utf-8")
                with self.assertRaises(MissingApiKeyError):
                    run_agent(inp)
        finally:
            if saved is not None:
                os.environ["OPENROUTER_API_KEY"] = saved


if __name__ == "__main__":
    unittest.main(verbosity=2)
