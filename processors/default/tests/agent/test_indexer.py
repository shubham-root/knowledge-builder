"""
Unit tests for kb_processor.agent.indexer.

Build a synthetic vault under a tempdir, index it, exercise every public
method, then mutate / delete files and confirm incremental refresh
detects the changes.

Run::

    ~/.local/share/kb/venv/bin/python3 -m pytest \\
        processors/default/tests/agent/test_indexer.py -v

If pytest is not in the venv, the file also runs as a plain script:

    ~/.local/share/kb/venv/bin/python3 \\
        processors/default/tests/agent/test_indexer.py
"""

from __future__ import annotations

import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

# Make the processor package importable when the file is run as a script.
_HERE = Path(__file__).resolve()
_SRC  = _HERE.parents[2]            # processors/default
sys.path.insert(0, str(_SRC))

from kb_processor.agent.indexer import (        # noqa: E402  pylint: disable=wrong-import-position
    VaultIndex,
    NoteRecord,
    IndexStats,
    _parse_note,
)


# ---------------------------------------------------------------------------
# _parse_note unit tests
# ---------------------------------------------------------------------------


class ParseNoteTests(unittest.TestCase):
    def test_title_from_frontmatter(self) -> None:
        content = '---\ntitle: "Deep Work Routine"\ntags: [focus, productivity]\n---\n# Body H1\nContent.'
        out = _parse_note(Path("/v/Note.md"), content)
        self.assertEqual(out["title"], "Deep Work Routine")
        self.assertIn("focus", out["tags"])             # type: ignore[arg-type]
        self.assertIn("productivity", out["tags"])      # type: ignore[arg-type]

    def test_title_falls_back_to_h1(self) -> None:
        content = "Some prefix\n# Hello World\nbody"
        out = _parse_note(Path("/v/x.md"), content)
        self.assertEqual(out["title"], "Hello World")

    def test_title_falls_back_to_filename(self) -> None:
        out = _parse_note(Path("/v/Untitled.md"), "no headings, no fm")
        self.assertEqual(out["title"], "Untitled")

    def test_headings_collected(self) -> None:
        content = "# A\n\n## B\n\n### C\nbody"
        out = _parse_note(Path("/v/h.md"), content)
        self.assertEqual(out["headings"], ["A", "B", "C"])

    def test_inline_tags_and_frontmatter_tags_merged(self) -> None:
        content = (
            "---\n"
            "tags:\n"
            "  - alpha\n"
            "  - beta\n"
            "---\n"
            "Hello #gamma and #delta and not_a_tag\n"
            "but #alpha again\n"
        )
        out = _parse_note(Path("/v/t.md"), content)
        self.assertSetEqual(set(out["tags"]), {"alpha", "beta", "gamma", "delta"})  # type: ignore[arg-type]

    def test_wikilinks_extracted_with_alias_and_section(self) -> None:
        content = "Refs [[Other Note]] [[Page#Section]] [[Page|Alias]]."
        out = _parse_note(Path("/v/w.md"), content)
        self.assertEqual(out["links"], ["Other Note", "Page", "Page"])

    def test_body_preview_truncates(self) -> None:
        long_body = "a" * 5000
        out = _parse_note(Path("/v/long.md"), long_body)
        self.assertEqual(len(out["body_preview"]), 2000)  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# VaultIndex integration tests
# ---------------------------------------------------------------------------


class VaultIndexTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.vault    = Path(self.tmp.name) / "vault"
        self.sources  = self.vault / "Sources"
        self.db       = Path(self.tmp.name) / "vault.db"
        self.sources.mkdir(parents=True)
        (self.vault / "Notes").mkdir()
        (self.vault / ".obsidian").mkdir()

        # User notes (under vault but NOT under Sources).
        self._write(self.vault / "Notes" / "Deep Work.md",
                    '---\ntitle: "Deep Work Routine"\ntags: [focus]\n---\n'
                    '# Deep Work Routine\n\nFlow state is everything. [[Cal Newport]].')
        self._write(self.vault / "Notes" / "Reading List.md",
                    "# Reading List\n\nBooks I want to read.\n\n## Non-fiction\n#books\n")
        self._write(self.vault / "Index.md", "# Index\n[[Deep Work]] [[Reading List]]")

        # Things that MUST be skipped:
        self._write(self.sources / "ignore-me.md",
                    "# Should be skipped\nUnder sources_dir")
        self._write(self.vault / ".obsidian" / "workspace.md",
                    "# Also skipped\nObsidian internal")

    @staticmethod
    def _write(p: Path, body: str) -> None:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body, encoding="utf-8")

    # ── Refresh + skip semantics ─────────────────────────────────────────

    def test_refresh_indexes_only_user_notes(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            stats: IndexStats = idx.refresh()
            self.assertEqual(stats.errors, [])
            self.assertEqual(stats.inserted, 3)         # Deep Work, Reading List, Index
            self.assertEqual(stats.unchanged, 0)
            self.assertEqual(stats.deleted, 0)

            self.assertEqual(idx.stats(), {"total_notes": 3})

            # Make sure neither sources_dir nor .obsidian content leaked.
            for r in idx.list_notes(limit=99):
                self.assertNotIn("/Sources/",   r.path)
                self.assertNotIn("/.obsidian/", r.path)

    def test_search_returns_relevant_match(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            hits = idx.search("flow state")
            self.assertGreater(len(hits), 0)
            self.assertIn("Deep Work", hits[0].title)

    def test_search_via_title(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            hits = idx.search("reading")
            self.assertGreater(len(hits), 0)
            self.assertEqual(hits[0].title, "Reading List")

    def test_list_tags(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            tags = dict(idx.list_tags())
            self.assertIn("focus", tags)
            self.assertIn("books", tags)

    def test_list_notes_by_folder(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            notes = idx.list_notes(folder="Notes")
            paths = [n.path for n in notes]
            self.assertEqual(len(notes), 2)
            for p in paths:
                self.assertIn("/Notes/", p)

    def test_list_notes_by_tag(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            notes = idx.list_notes(tag="focus")
            self.assertEqual(len(notes), 1)
            self.assertEqual(notes[0].title, "Deep Work Routine")

    # ── Incremental refresh ──────────────────────────────────────────────

    def test_unchanged_files_skipped_on_second_refresh(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            stats = idx.refresh()
            self.assertEqual(stats.inserted, 0)
            self.assertEqual(stats.updated,  0)
            self.assertEqual(stats.deleted,  0)
            self.assertEqual(stats.unchanged, 3)

    def test_modified_file_marked_updated(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            target = self.vault / "Notes" / "Deep Work.md"
            time.sleep(0.01)            # ensure mtime advances
            target.write_text("# Deep Work Routine\n\nNew thoughts: shallow work hurts.")
            stats = idx.refresh()
            self.assertEqual(stats.updated,   1)
            self.assertEqual(stats.unchanged, 2)

            hits = idx.search("shallow")
            self.assertGreater(len(hits), 0)
            self.assertEqual(hits[0].title, "Deep Work Routine")

    def test_deleted_file_purged(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            (self.vault / "Index.md").unlink()
            stats = idx.refresh()
            self.assertEqual(stats.deleted, 1)
            self.assertEqual(idx.stats(), {"total_notes": 2})

    # ── Bad input safety ─────────────────────────────────────────────────

    def test_malformed_fts_query_returns_empty(self) -> None:
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            idx.refresh()
            self.assertEqual(idx.search(""),     [])
            # Reserved character; should not raise, returns [].
            self.assertEqual(idx.search('"unbalanced'),  [])

    def test_unicode_decode_error_recorded(self) -> None:
        bad = self.vault / "Notes" / "binary.md"
        bad.write_bytes(b"\xff\xfe\xfd not valid utf-8")
        with VaultIndex.open(self.db, self.vault, self.sources) as idx:
            stats = idx.refresh()
            self.assertGreater(len(stats.errors), 0)
            # The good notes still indexed.
            self.assertGreaterEqual(stats.inserted, 3)


# ---------------------------------------------------------------------------
# Script entry-point fallback (no pytest required)
# ---------------------------------------------------------------------------


if __name__ == "__main__":
    unittest.main(verbosity=2)
