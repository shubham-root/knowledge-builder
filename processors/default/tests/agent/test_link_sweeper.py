"""
Unit tests for kb_processor.agent.link_sweeper.
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

_HERE = Path(__file__).resolve()
_SRC  = _HERE.parents[2]
sys.path.insert(0, str(_SRC))

from kb_processor.agent.link_sweeper import (             # noqa: E402
    UnresolvedLink,
    _is_resolved,
    _stringify_unresolved,
    files_touched_by_plan,
    sweep_files,
    sweep_links_in_text,
)
from kb_processor.agent.plan import Plan, PlanEntry        # noqa: E402


# ---------------------------------------------------------------------------
# Pure-text sweeping
# ---------------------------------------------------------------------------


class SweepTextTests(unittest.TestCase):
    def test_resolved_link_left_alone(self) -> None:
        existing = {"Foo", "Notes/Foo"}
        out, replaced = sweep_links_in_text(
            "See [[Foo]] for details.", existing,
        )
        self.assertEqual(out, "See [[Foo]] for details.")
        self.assertEqual(replaced, [])

    def test_unresolved_simple_link_replaced(self) -> None:
        out, replaced = sweep_links_in_text(
            "See [[Ghost]] for details.", set(),
        )
        self.assertIn("Ghost [possible linkout - elaboration needed]", out)
        self.assertNotIn("[[Ghost]]", out)
        self.assertEqual(len(replaced), 1)
        self.assertEqual(replaced[0].target, "Ghost")
        self.assertIsNone(replaced[0].alias)
        self.assertIsNone(replaced[0].section)

    def test_unresolved_with_alias_uses_alias_text(self) -> None:
        out, _ = sweep_links_in_text("Try [[Foo|the foo bar]].", set())
        self.assertIn("the foo bar [possible linkout - elaboration needed]", out)
        self.assertNotIn("[[Foo", out)

    def test_unresolved_with_section_uses_target_and_section(self) -> None:
        out, replaced = sweep_links_in_text("See [[Foo#Methods]].", set())
        self.assertIn("Foo (§Methods) [possible linkout - elaboration needed]", out)
        self.assertEqual(replaced[0].target, "Foo")
        self.assertEqual(replaced[0].section, "Methods")

    def test_unresolved_with_section_and_alias(self) -> None:
        out, _ = sweep_links_in_text("[[Foo#Sec|alias]] x", set())
        self.assertIn("alias (§Sec) [possible linkout - elaboration needed]", out)

    def test_path_style_resolved_link(self) -> None:
        existing = {"Topics/Foo", "Topics/Foo.md", "Foo"}
        out, replaced = sweep_links_in_text("[[Topics/Foo]]", existing)
        self.assertEqual(out, "[[Topics/Foo]]")
        self.assertEqual(replaced, [])

    def test_explicit_md_extension_resolved(self) -> None:
        existing = {"Foo"}
        out, _ = sweep_links_in_text("[[Foo.md]]", existing)
        # Should not be replaced — Foo.md without extension equals "Foo"
        # which is in `existing`.
        self.assertEqual(out, "[[Foo.md]]")

    def test_multiple_links_mixed_resolution(self) -> None:
        existing = {"Foo", "Bar"}
        out, replaced = sweep_links_in_text(
            "Refs: [[Foo]], [[Bar]], [[Ghost]], and [[Phantom|p]].",
            existing,
        )
        # Foo, Bar resolved; Ghost, Phantom unresolved.
        self.assertEqual(len(replaced), 2)
        self.assertIn("[[Foo]]", out)
        self.assertIn("[[Bar]]", out)
        self.assertIn("Ghost [possible linkout - elaboration needed]", out)
        self.assertIn("p [possible linkout - elaboration needed]", out)

    # ── Code-block preservation ──

    def test_inline_code_links_preserved(self) -> None:
        existing: set[str] = set()
        out, replaced = sweep_links_in_text(
            "Use `[[Ghost]]` syntax for wikilinks.", existing,
        )
        self.assertEqual(out, "Use `[[Ghost]]` syntax for wikilinks.")
        self.assertEqual(replaced, [])

    def test_fenced_code_links_preserved(self) -> None:
        existing: set[str] = set()
        body = (
            "Example:\n"
            "```markdown\n"
            "Link to [[Ghost]] in code\n"
            "```\n"
            "But this real [[Ghost]] is unresolved.\n"
        )
        out, replaced = sweep_links_in_text(body, existing)
        # The one inside the code block is intact.
        self.assertIn("Link to [[Ghost]] in code", out)
        # The one outside is rewritten.
        self.assertIn("real Ghost [possible linkout - elaboration needed]", out)
        self.assertEqual(len(replaced), 1)

    def test_multiline_fenced_block(self) -> None:
        existing: set[str] = set()
        body = (
            "```\n"
            "[[A]]\n"
            "[[B]]\n"
            "[[C]]\n"
            "```\n"
            "End: [[D]]\n"
        )
        out, replaced = sweep_links_in_text(body, existing)
        self.assertIn("[[A]]", out)
        self.assertIn("[[B]]", out)
        self.assertIn("[[C]]", out)
        self.assertEqual(len(replaced), 1)
        self.assertEqual(replaced[0].target, "D")

    # ── Idempotency ──

    def test_sweeping_already_swept_text_is_noop(self) -> None:
        text = "See Ghost [possible linkout - elaboration needed]."
        out, replaced = sweep_links_in_text(text, set())
        self.assertEqual(out, text)
        self.assertEqual(replaced, [])

    # ── Edge cases ──

    def test_empty_input(self) -> None:
        out, replaced = sweep_links_in_text("", set())
        self.assertEqual(out, "")
        self.assertEqual(replaced, [])

    def test_no_links_input(self) -> None:
        body = "# Title\n\nJust prose, no links.\n"
        out, replaced = sweep_links_in_text(body, set())
        self.assertEqual(out, body)
        self.assertEqual(replaced, [])


# ---------------------------------------------------------------------------
# _is_resolved + _stringify_unresolved
# ---------------------------------------------------------------------------


class HelperTests(unittest.TestCase):
    def test_is_resolved_basename(self) -> None:
        self.assertTrue (_is_resolved("Foo",       {"Foo"}))
        self.assertFalse(_is_resolved("Foo",       {"Bar"}))

    def test_is_resolved_with_md_suffix(self) -> None:
        self.assertTrue(_is_resolved("Foo.md", {"Foo"}))

    def test_is_resolved_path_form(self) -> None:
        existing = {"Topics/Foo", "Topics/Foo.md"}
        self.assertTrue(_is_resolved("Topics/Foo",    existing))
        self.assertTrue(_is_resolved("Topics/Foo.md", existing))

    def test_is_resolved_implicit_md(self) -> None:
        # The link is "Foo" and the index has "Foo.md".
        self.assertTrue(_is_resolved("Foo", {"Foo.md"}))

    def test_stringify_simple(self) -> None:
        link = UnresolvedLink(target="Foo", alias=None, section=None)
        self.assertEqual(
            _stringify_unresolved(link),
            "Foo [possible linkout - elaboration needed]",
        )

    def test_stringify_alias(self) -> None:
        link = UnresolvedLink(target="Foo", alias="bar", section=None)
        self.assertEqual(
            _stringify_unresolved(link),
            "bar [possible linkout - elaboration needed]",
        )

    def test_stringify_section(self) -> None:
        link = UnresolvedLink(target="Foo", alias=None, section="Methods")
        self.assertEqual(
            _stringify_unresolved(link),
            "Foo (§Methods) [possible linkout - elaboration needed]",
        )


# ---------------------------------------------------------------------------
# files_touched_by_plan
# ---------------------------------------------------------------------------


class FilesTouchedTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.vault = Path(self.tmp.name)

    def _entry(self, cmd: str, *args: str, applied: bool = True) -> PlanEntry:
        return PlanEntry(
            ts=0, mode="apply", cmd=cmd, args=tuple(args),
            applied=applied, exit_code=0 if applied else None,
        )

    def test_extracts_create_path(self) -> None:
        out = files_touched_by_plan(
            [self._entry("create", "path=KnowledgeBase/Foo.md", "content=hi")],
            self.vault,
        )
        self.assertEqual(out, [self.vault / "KnowledgeBase/Foo.md"])

    def test_extracts_append_with_file(self) -> None:
        out = files_touched_by_plan(
            [self._entry("append", "file=KnowledgeBase/Foo.md", "content=more")],
            self.vault,
        )
        self.assertEqual(out, [self.vault / "KnowledgeBase/Foo.md"])

    def test_skips_unapplied(self) -> None:
        out = files_touched_by_plan(
            [self._entry("create", "path=Foo.md", applied=False)],
            self.vault,
        )
        self.assertEqual(out, [])

    def test_skips_read_commands(self) -> None:
        out = files_touched_by_plan(
            [self._entry("search", "query=foo")],
            self.vault,
        )
        self.assertEqual(out, [])

    def test_skips_property_set(self) -> None:
        # property:set targets an existing file by file=; it does not
        # *create* anything new.  We don't sweep it.
        out = files_touched_by_plan(
            [self._entry(
                "property:set",
                "name=tags", "value=foo", "file=KnowledgeBase/Foo.md",
            )],
            self.vault,
        )
        self.assertEqual(out, [])


# ---------------------------------------------------------------------------
# Full sweep_files end-to-end
# ---------------------------------------------------------------------------


class SweepFilesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.vault   = Path(self.tmp.name) / "vault"
        self.sources = self.vault / "Sources"
        self.kb      = self.vault / "KnowledgeBase"
        self.kb.mkdir(parents=True)
        self.sources.mkdir(parents=True)

    def _write(self, rel: str, body: str) -> Path:
        p = self.vault / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")
        return p

    def test_replaces_unresolved_writes_back(self) -> None:
        target = self._write(
            "KnowledgeBase/Foo.md",
            "# Foo\n\nSee [[Ghost]] for the missing piece.\n",
        )
        stats = sweep_files(
            files       = [target],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.files_examined, 1)
        self.assertEqual(stats.files_modified, 1)
        self.assertEqual(stats.links_replaced, 1)

        body = target.read_text(encoding="utf-8")
        self.assertNotIn("[[Ghost]]", body)
        self.assertIn("Ghost [possible linkout - elaboration needed]", body)

    def test_keeps_resolved_link_to_vault_note(self) -> None:
        # Existing user note OUTSIDE KnowledgeBase, agent links to it.
        self._write("Notes/Existing.md",          "# Existing\n")
        target = self._write(
            "KnowledgeBase/Foo.md",
            "Refs [[Existing]].\n",
        )
        stats = sweep_files(
            files       = [target],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.links_replaced, 0)
        self.assertEqual(stats.files_modified, 0)
        self.assertIn("[[Existing]]", target.read_text(encoding="utf-8"))

    def test_skips_files_outside_agent_root(self) -> None:
        # Hostile case: caller passes a file path outside the agent's
        # mutation sandbox.  Sweeper must refuse to touch it.
        outside = self._write(
            "Notes/Outside.md",
            "# Outside\n\n[[Ghost]] should NOT be rewritten here.\n",
        )
        stats = sweep_files(
            files       = [outside],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.files_examined, 0)
        self.assertEqual(stats.files_modified, 0)
        # Content unchanged.
        self.assertIn("[[Ghost]]", outside.read_text(encoding="utf-8"))

    def test_skips_non_markdown_files(self) -> None:
        target = self._write("KnowledgeBase/data.json", '{"x": "[[Y]]"}')
        stats = sweep_files(
            files       = [target],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.files_examined, 0)
        self.assertEqual(stats.skipped_non_markdown, 1)
        self.assertIn("[[Y]]", target.read_text(encoding="utf-8"))

    def test_diagnostics_count_drift_to_nonexistent_path(self) -> None:
        # Plan recorded a path that never made it to disk (e.g. Obsidian
        # auto-disambiguated `Foo.md` -> `Foo 1.md`).  The sweeper must
        # log a warning *and* report this in stats.skipped_not_a_file so
        # the pipeline can surface plan/disk drift in the daemon log.
        nonexistent = self.kb / "Renamed.md"
        real        = self._write("KnowledgeBase/Real.md", "hi")
        stats = sweep_files(
            files       = [nonexistent, real],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.files_input,           2)
        self.assertEqual(stats.files_examined,        1)  # only `real`
        self.assertEqual(stats.skipped_not_a_file,    1)
        self.assertEqual(stats.skipped_outside_root,  0)
        # Metadata round-trip exposes the new keys.
        meta = stats.as_metadata()
        self.assertIn("link_sweep_input",                meta)
        self.assertIn("link_sweep_skipped_not_a_file",   meta)
        self.assertEqual(meta["link_sweep_skipped_not_a_file"], 1)

    def test_resolves_to_files_created_in_same_run(self) -> None:
        # The agent created BOTH `Main.md` and `Sub.md` in this run.
        # `Main.md` links to `[[Sub]]` — that must NOT be flagged.
        main = self._write(
            "KnowledgeBase/Main.md", "Top.  See [[Sub]] below.\n",
        )
        sub  = self._write("KnowledgeBase/Sub.md", "# Sub\n")

        stats = sweep_files(
            files       = [main, sub],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.links_replaced, 0)

    def test_files_under_sources_dir_excluded_from_index(self) -> None:
        # Even if a markdown file exists inside Sources/, the sweeper
        # must NOT consider it a valid wikilink target.  (Sources is
        # the input area; agent should never link to it.)
        self._write("Sources/Stale.md", "# Stale source\n")
        target = self._write(
            "KnowledgeBase/Foo.md", "Refs [[Stale]].\n",
        )
        stats = sweep_files(
            files       = [target],
            vault_root  = self.vault,
            sources_dir = self.sources,
            agent_root  = self.kb,
        )
        self.assertEqual(stats.links_replaced, 1)
        self.assertNotIn("[[Stale]]", target.read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------


if __name__ == "__main__":
    unittest.main(verbosity=2)
