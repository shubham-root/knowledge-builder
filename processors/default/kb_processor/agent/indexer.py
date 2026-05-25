"""
Vault indexer for the Knowledge Builder agent.

Walks the Obsidian vault (skipping ``sources_dir`` and ``.obsidian/``),
extracts each markdown note's title / headings / outgoing wikilinks /
frontmatter tags / first 2000 chars of body, and stores everything in a
SQLite FTS5 (BM25) virtual table for fast lexical search.

Design
------
* **Single-file SQLite** at ``vault.db`` (default:
  ``~/Library/Application Support/knowledge-builder/vault.db``).  Separate
  from the daemon's ``state.db`` so opening one does not contend with the
  other.
* **Incremental refresh** keyed on ``(path, mtime_ns, size)``.  An unchanged
  note is skipped entirely; a removed note has its row purged.  Full
  reindex of 1 000 notes takes <1 second.
* **FTS5 BM25** for ranking.  Index covers ``title``, ``headings``,
  ``tags``, ``links``, ``body_preview``.  All five are searchable; rank
  weights default to ``(10.0, 5.0, 5.0, 2.0, 1.0)``.
* **No external dependencies**.  Pure stdlib (``sqlite3``, ``re``, ``pathlib``).
* **Read-only public API** — the indexer never writes notes; it only
  catalogues them.  Mutations belong to the writer stage.

Public surface
--------------
* :class:`VaultIndex`         — connect-and-refresh + query API.
* :class:`NoteRecord`         — single note row.
* :class:`IndexStats`         — scan summary (counts + timings).

Quick start
-----------
::

    from kb_processor.agent.indexer import VaultIndex
    from pathlib import Path

    idx = VaultIndex.open(
        db_path     = Path("~/Library/Application Support/knowledge-builder/vault.db").expanduser(),
        vault_root  = Path("~/Documents/Obsidian").expanduser(),
        sources_dir = Path("~/Documents/Obsidian/Sources").expanduser(),
    )
    stats = idx.refresh()                      # incremental rescan
    print(stats)
    for hit in idx.search("deep work focus", limit=5):
        print(hit.path, hit.score)
"""

from __future__ import annotations

import logging
import re
import sqlite3
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Iterator

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Tunables
# ---------------------------------------------------------------------------

#: Maximum body characters indexed per note.  Beyond this only the prefix is
#: stored; queries can still match because Obsidian notes' relevant content
#: tends to live in the first ~2 000 chars (headers + first sections).
_MAX_BODY_CHARS: int = 2_000

#: Field weights for BM25 ranking, in column order matching the FTS5 table.
#: Higher = more important.  Title and headings dominate; body preview is a
#: tiebreaker.
_BM25_WEIGHTS: tuple[float, ...] = (10.0, 5.0, 5.0, 2.0, 1.0)

#: Filename / path patterns to skip during walk.
_SKIP_DIRS: frozenset[str] = frozenset({".obsidian", ".trash", ".git", "node_modules"})

# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class NoteRecord:
    """One indexed note.  ``path`` is absolute, all other fields are strings.

    ``score`` is populated only when the record is returned from
    :meth:`VaultIndex.search`; for other accessors (e.g. ``get_by_path``)
    it is ``None``.
    """

    path:         str
    title:        str
    headings:     str        # newline-separated
    tags:         str        # space-separated
    links:        str        # space-separated wikilink targets
    body_preview: str
    mtime_ns:     int
    size:         int
    score:        float | None = None


@dataclass
class IndexStats:
    """Summary statistics from a single :meth:`VaultIndex.refresh` call."""

    elapsed_ms:    float = 0.0
    scanned:       int   = 0
    inserted:      int   = 0
    updated:       int   = 0
    deleted:       int   = 0
    unchanged:     int   = 0
    errors:        list[tuple[str, str]] = field(default_factory=list)

    def __str__(self) -> str:  # noqa: D401
        return (
            f"IndexStats(scanned={self.scanned}, inserted={self.inserted}, "
            f"updated={self.updated}, deleted={self.deleted}, "
            f"unchanged={self.unchanged}, errors={len(self.errors)}, "
            f"elapsed_ms={self.elapsed_ms:.1f})"
        )


# ---------------------------------------------------------------------------
# Markdown / frontmatter parsing
# ---------------------------------------------------------------------------

_FRONTMATTER_RE   = re.compile(r"\A---\s*\n(.*?)\n---\s*\n", re.DOTALL)
_HEADING_RE       = re.compile(r"^\s*(#{1,6})\s+(.+?)\s*$", re.MULTILINE)
_WIKILINK_RE      = re.compile(r"\[\[([^\]\|#]+?)(?:[#\|][^\]]*)?\]\]")
# These three are restricted to non-newline whitespace ([ \t]*) so the
# greedy `\s*` does not consume the line break and bleed into following
# YAML lines (e.g. capturing `- alpha` from a list-form ``tags:`` block).
_FM_TITLE_RE      = re.compile(r"^[ \t]*title[ \t]*:[ \t]*(.+?)[ \t]*$",  re.MULTILINE | re.IGNORECASE)
_FM_TAGS_RE       = re.compile(r"^[ \t]*tags?[ \t]*:[ \t]*(.+?)[ \t]*$", re.MULTILINE | re.IGNORECASE)
_FM_TAG_LINE_RE   = re.compile(r"^[ \t]*-[ \t]*(.+?)[ \t]*$",              re.MULTILINE)
_INLINE_TAG_RE    = re.compile(r"(?<![\w/])#([A-Za-z][\w/\-]*)")


def _parse_note(path: Path, content: str) -> dict[str, object]:
    """Parse a markdown note's structured fields.

    Returns
    -------
    dict
        Keys: ``title``, ``headings`` (list[str]), ``tags`` (list[str]),
        ``links`` (list[str]), ``body_preview`` (str).
    """
    # Frontmatter (if any).
    fm: str = ""
    body: str = content
    m = _FRONTMATTER_RE.match(content)
    if m:
        fm   = m.group(1)
        body = content[m.end():]

    # Title: prefer frontmatter ``title:``, then first H1, then filename stem.
    title: str = ""
    if fm:
        tm = _FM_TITLE_RE.search(fm)
        if tm:
            title = tm.group(1).strip().strip('"').strip("'")
    if not title:
        h1 = re.search(r"^\s*#\s+(.+?)\s*$", body, re.MULTILINE)
        if h1:
            title = h1.group(1).strip()
    if not title:
        title = path.stem

    # Headings (all levels) for FTS.  Strip the leading hashes.
    headings: list[str] = [m.group(2).strip() for m in _HEADING_RE.finditer(body)]

    # Tags: frontmatter ``tags:`` (inline list, YAML list, or bare string)
    # plus inline ``#tag`` occurrences in the body.
    tags: set[str] = set()
    if fm:
        tm = _FM_TAGS_RE.search(fm)
        if tm:
            raw = tm.group(1).strip()
            if raw.startswith("[") and raw.endswith("]"):
                # JSON-ish inline list
                for t in raw.strip("[]").split(","):
                    t = t.strip().strip('"').strip("'")
                    if t:
                        tags.add(t)
            elif "," in raw:
                for t in raw.split(","):
                    t = t.strip().strip('"').strip("'")
                    if t:
                        tags.add(t)
            else:
                t = raw.strip().strip('"').strip("'")
                if t:
                    tags.add(t)
        # YAML-list form: tags:\n  - foo\n  - bar
        for tm2 in _FM_TAG_LINE_RE.finditer(fm):
            t = tm2.group(1).strip().strip('"').strip("'")
            if t:
                tags.add(t)
    for itm in _INLINE_TAG_RE.finditer(body):
        tags.add(itm.group(1))

    # Outgoing wikilinks.
    links: list[str] = [
        m.group(1).strip() for m in _WIKILINK_RE.finditer(body)
    ]

    # Body preview.
    preview = body[:_MAX_BODY_CHARS]

    return {
        "title":        title,
        "headings":     headings,
        "tags":         sorted(tags),
        "links":        links,
        "body_preview": preview,
    }


# ---------------------------------------------------------------------------
# VaultIndex
# ---------------------------------------------------------------------------


class VaultIndex:
    """Read-only-public, write-internal SQLite-FTS5 index of an Obsidian vault.

    The constructor opens (and migrates if needed) the SQLite file but does
    NOT scan the vault.  Call :meth:`refresh` to bring the index up to date.

    Thread safety
    -------------
    A :class:`VaultIndex` owns one ``sqlite3.Connection``.  Use one instance
    per thread (or wrap calls in a lock).  All public methods are
    re-entrant on a single thread.
    """

    # ── Construction ─────────────────────────────────────────────────────

    @classmethod
    def open(
        cls,
        db_path:     Path,
        vault_root:  Path,
        sources_dir: Path,
    ) -> "VaultIndex":
        """Open (creating if absent) the index DB at ``db_path`` for the
        given vault.  Runs the schema migration; does NOT scan.
        """
        db_path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(str(db_path), isolation_level=None)  # autocommit
        conn.execute("PRAGMA journal_mode = WAL;")
        conn.execute("PRAGMA synchronous  = NORMAL;")
        return cls(
            conn=conn,
            vault_root=vault_root.resolve(),
            sources_dir=sources_dir.resolve(),
        )

    def __init__(
        self,
        conn:        sqlite3.Connection,
        vault_root:  Path,
        sources_dir: Path,
    ) -> None:
        self.conn        = conn
        self.vault_root  = vault_root
        self.sources_dir = sources_dir
        self._migrate()

    # ── Schema ───────────────────────────────────────────────────────────

    _SCHEMA_VERSION = 1

    def _migrate(self) -> None:
        """Idempotent schema setup."""
        cur = self.conn.cursor()

        # Plain table for canonical row data and incremental refresh keys.
        cur.execute("""
            CREATE TABLE IF NOT EXISTS notes (
                path         TEXT PRIMARY KEY,
                title        TEXT NOT NULL,
                headings     TEXT NOT NULL,   -- newline-separated
                tags         TEXT NOT NULL,   -- space-separated
                links        TEXT NOT NULL,   -- space-separated targets
                body_preview TEXT NOT NULL,
                mtime_ns     INTEGER NOT NULL,
                size         INTEGER NOT NULL,
                indexed_at   INTEGER NOT NULL
            );
        """)
        cur.execute("CREATE INDEX IF NOT EXISTS idx_notes_mtime ON notes(mtime_ns);")

        # FTS5 virtual table mirroring the searchable text columns.  We do
        # NOT use the ``content=notes`` external-content mode because the
        # extra trigger scaffolding adds complexity for no measurable win
        # at vault sizes <10 k notes.  Instead we keep the FTS rows in
        # lockstep manually inside refresh().
        cur.execute("""
            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts
            USING fts5(
                path UNINDEXED,
                title,
                headings,
                tags,
                links,
                body_preview,
                tokenize = 'porter unicode61 remove_diacritics 2'
            );
        """)

        cur.execute("""
            CREATE TABLE IF NOT EXISTS schema_version (
                version    INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
        """)

        cur.execute("SELECT max(version) FROM schema_version;")
        row = cur.fetchone()
        current = row[0] if row and row[0] is not None else 0
        if current < self._SCHEMA_VERSION:
            cur.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?, ?);",
                (self._SCHEMA_VERSION, int(time.time())),
            )

    # ── Refresh ──────────────────────────────────────────────────────────

    def refresh(self) -> IndexStats:
        """Walk the vault and bring the index up to date.

        Skips ``sources_dir`` and any path under ``_SKIP_DIRS``.  Skips
        non-``.md`` files.  Detects deletions by diffing ``notes.path``
        against the live filesystem.

        Returns an :class:`IndexStats` describing what changed.
        """
        t0 = time.perf_counter()
        stats = IndexStats()

        live_paths: set[str] = set()
        for note_path in self._iter_markdown_files():
            stats.scanned += 1
            try:
                stat = note_path.stat()
            except OSError as exc:
                stats.errors.append((str(note_path), f"stat: {exc}"))
                continue

            mtime_ns = stat.st_mtime_ns
            size     = stat.st_size
            abspath  = str(note_path)
            live_paths.add(abspath)

            existing = self._get_keys(abspath)
            if existing == (mtime_ns, size):
                stats.unchanged += 1
                continue

            try:
                content = note_path.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError) as exc:
                stats.errors.append((abspath, f"read: {exc}"))
                continue

            try:
                parsed = _parse_note(note_path, content)
            except Exception as exc:  # noqa: BLE001
                stats.errors.append((abspath, f"parse: {exc}"))
                continue

            if existing is None:
                self._insert(abspath, parsed, mtime_ns, size)
                stats.inserted += 1
            else:
                self._update(abspath, parsed, mtime_ns, size)
                stats.updated += 1

        # Deletions: anything in `notes` not in `live_paths`.
        cur = self.conn.cursor()
        cur.execute("SELECT path FROM notes;")
        all_paths = {row[0] for row in cur.fetchall()}
        for stale in all_paths - live_paths:
            self._delete(stale)
            stats.deleted += 1

        stats.elapsed_ms = (time.perf_counter() - t0) * 1000.0
        logger.info("Vault index refresh: %s", stats)
        return stats

    # ── Read API ─────────────────────────────────────────────────────────

    def get_by_path(self, abs_path: str) -> NoteRecord | None:
        cur = self.conn.cursor()
        cur.execute(
            "SELECT path, title, headings, tags, links, body_preview, "
            "mtime_ns, size FROM notes WHERE path = ?;",
            (abs_path,),
        )
        row = cur.fetchone()
        return self._row_to_record(row) if row else None

    def list_notes(
        self,
        folder: str | None = None,
        tag:    str | None = None,
        limit:  int        = 200,
    ) -> list[NoteRecord]:
        """Return notes filtered by folder prefix and/or tag membership.

        ``folder`` is matched as a prefix against absolute paths
        (anchored to ``vault_root``).  ``tag`` is matched as a whole-word
        member of the space-separated tags column.
        """
        sql = (
            "SELECT path, title, headings, tags, links, body_preview, "
            "mtime_ns, size FROM notes"
        )
        clauses: list[str]    = []
        params:  list[object] = []

        if folder:
            folder_abs = str((self.vault_root / folder).resolve())
            clauses.append("path LIKE ? || '%'")
            params.append(folder_abs.rstrip("/") + "/")
        if tag:
            # Whole-word match on space-separated tags column.
            clauses.append(
                "(' ' || tags || ' ') LIKE '% ' || ? || ' %'"
            )
            params.append(tag)

        if clauses:
            sql += " WHERE " + " AND ".join(clauses)
        sql += " ORDER BY mtime_ns DESC LIMIT ?;"
        params.append(limit)

        cur = self.conn.cursor()
        cur.execute(sql, params)
        return [self._row_to_record(r) for r in cur.fetchall()]

    def search(self, query: str, limit: int = 20) -> list[NoteRecord]:
        """BM25-ranked full-text search across title / headings / tags /
        links / body_preview.

        ``query`` is forwarded to FTS5 verbatim, so it supports the full
        FTS5 query syntax (``deep work``, ``"deep work"``, ``focus OR
        attention``, ``title:foo``, etc.).  See
        https://www.sqlite.org/fts5.html#full_text_query_syntax.

        Empty / whitespace-only queries return ``[]``.
        """
        q = query.strip()
        if not q:
            return []

        weights = ", ".join(f"{w:g}" for w in _BM25_WEIGHTS)
        sql = (
            "SELECT n.path, n.title, n.headings, n.tags, n.links, "
            "n.body_preview, n.mtime_ns, n.size, "
            f"bm25(notes_fts, {weights}) AS score "
            "FROM notes_fts JOIN notes n ON n.path = notes_fts.path "
            "WHERE notes_fts MATCH ? "
            "ORDER BY score ASC LIMIT ?;"
        )
        cur = self.conn.cursor()
        try:
            cur.execute(sql, (q, limit))
            rows = cur.fetchall()
        except sqlite3.OperationalError as exc:
            # Malformed FTS5 query — caller passed unescaped operators.
            logger.warning("FTS5 query failed for %r: %s", query, exc)
            return []
        return [self._row_to_record(r, score=r[8]) for r in rows]

    def list_tags(self) -> list[tuple[str, int]]:
        """Return ``(tag, count)`` pairs sorted by count desc."""
        cur = self.conn.cursor()
        cur.execute("SELECT tags FROM notes WHERE tags <> '';")
        from collections import Counter
        counter: Counter[str] = Counter()
        for (tags_str,) in cur.fetchall():
            for t in tags_str.split():
                counter[t] += 1
        return counter.most_common()

    def stats(self) -> dict[str, int]:
        """Return summary counts for ops/observability."""
        cur = self.conn.cursor()
        cur.execute("SELECT count(*) FROM notes;")
        total = cur.fetchone()[0]
        return {"total_notes": total}

    # ── Internals ────────────────────────────────────────────────────────

    def _iter_markdown_files(self) -> Iterator[Path]:
        """Yield every ``.md`` file under ``vault_root`` not under
        ``sources_dir`` or any ``_SKIP_DIRS`` directory.

        Symlinks are followed but cycles are avoided by tracking visited
        absolute paths.
        """
        sources_str = str(self.sources_dir)
        seen: set[str] = set()
        stack: list[Path] = [self.vault_root]
        while stack:
            d = stack.pop()
            try:
                entries = list(d.iterdir())
            except (OSError, PermissionError) as exc:
                logger.debug("skip %s: %s", d, exc)
                continue
            for entry in entries:
                try:
                    real = str(entry.resolve())
                except OSError:
                    continue
                if real in seen:
                    continue
                seen.add(real)

                if entry.is_dir():
                    if entry.name in _SKIP_DIRS:
                        continue
                    if real == sources_str or real.startswith(sources_str + "/"):
                        continue
                    stack.append(entry)
                elif entry.is_file() and entry.suffix.lower() == ".md":
                    if real.startswith(sources_str + "/"):
                        continue
                    yield entry

    def _get_keys(self, abs_path: str) -> tuple[int, int] | None:
        """Return ``(mtime_ns, size)`` for an indexed note, or ``None`` if
        the note is not yet in the index.
        """
        cur = self.conn.cursor()
        cur.execute(
            "SELECT mtime_ns, size FROM notes WHERE path = ?;", (abs_path,),
        )
        row = cur.fetchone()
        return (int(row[0]), int(row[1])) if row else None

    def _insert(
        self,
        abs_path: str,
        parsed:   dict[str, object],
        mtime_ns: int,
        size:     int,
    ) -> None:
        title        = str(parsed["title"])
        headings_str = "\n".join(parsed["headings"])              # type: ignore[arg-type]
        tags_str     = " ".join(parsed["tags"])                   # type: ignore[arg-type]
        links_str    = " ".join(parsed["links"])                  # type: ignore[arg-type]
        body         = str(parsed["body_preview"])

        cur = self.conn.cursor()
        cur.execute(
            "INSERT INTO notes "
            "(path, title, headings, tags, links, body_preview, mtime_ns, size, indexed_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);",
            (abs_path, title, headings_str, tags_str, links_str, body,
             mtime_ns, size, int(time.time())),
        )
        cur.execute(
            "INSERT INTO notes_fts "
            "(path, title, headings, tags, links, body_preview) "
            "VALUES (?, ?, ?, ?, ?, ?);",
            (abs_path, title, headings_str, tags_str, links_str, body),
        )

    def _update(
        self,
        abs_path: str,
        parsed:   dict[str, object],
        mtime_ns: int,
        size:     int,
    ) -> None:
        title        = str(parsed["title"])
        headings_str = "\n".join(parsed["headings"])              # type: ignore[arg-type]
        tags_str     = " ".join(parsed["tags"])                   # type: ignore[arg-type]
        links_str    = " ".join(parsed["links"])                  # type: ignore[arg-type]
        body         = str(parsed["body_preview"])

        cur = self.conn.cursor()
        cur.execute(
            "UPDATE notes SET title=?, headings=?, tags=?, links=?, "
            "body_preview=?, mtime_ns=?, size=?, indexed_at=? WHERE path=?;",
            (title, headings_str, tags_str, links_str, body,
             mtime_ns, size, int(time.time()), abs_path),
        )
        cur.execute("DELETE FROM notes_fts WHERE path = ?;", (abs_path,))
        cur.execute(
            "INSERT INTO notes_fts "
            "(path, title, headings, tags, links, body_preview) "
            "VALUES (?, ?, ?, ?, ?, ?);",
            (abs_path, title, headings_str, tags_str, links_str, body),
        )

    def _delete(self, abs_path: str) -> None:
        cur = self.conn.cursor()
        cur.execute("DELETE FROM notes     WHERE path = ?;", (abs_path,))
        cur.execute("DELETE FROM notes_fts WHERE path = ?;", (abs_path,))

    @staticmethod
    def _row_to_record(
        row:   Iterable[object] | None,
        score: float | None = None,
    ) -> NoteRecord:
        if row is None:
            raise ValueError("row is None")
        r = list(row)
        return NoteRecord(
            path         = str(r[0]),
            title        = str(r[1]),
            headings     = str(r[2]),
            tags         = str(r[3]),
            links        = str(r[4]),
            body_preview = str(r[5]),
            mtime_ns     = int(r[6]),  # type: ignore[arg-type]
            size         = int(r[7]),  # type: ignore[arg-type]
            score        = score,
        )

    # ── Resource cleanup ─────────────────────────────────────────────────

    def close(self) -> None:
        try:
            self.conn.close()
        except sqlite3.Error:
            pass

    def __enter__(self) -> "VaultIndex":
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()
