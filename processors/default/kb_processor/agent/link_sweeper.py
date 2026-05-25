"""
Post-run link sweeper for the Knowledge Builder agent.

After the agent finishes in *apply* mode, walk every file it created or
modified, find any ``[[wikilink]]`` that resolves to no existing note,
and rewrite it in-place to a plain-text placeholder of the form::

    Target [possible linkout - elaboration needed]

This is a deterministic backstop for prompt-level discipline.  The LLM
is told to avoid creating broken links in the first place
(``SKILL.md``); the sweeper guarantees the invariant regardless of
model compliance.

Design constraints
------------------
* Only runs in ``apply`` mode \u2014 in ``shadow`` mode the agent's content
  has not been written to disk so there is nothing to sweep.
* Only touches files inside ``agent_root`` (the agent's mutation
  sandbox).  User-authored notes elsewhere in the vault are out of
  scope; if they had broken links they were broken before the agent
  ran and not the agent's job to fix.
* Only touches files the agent's plan claims to have ``create``-d,
  ``append``-ed or ``prepend``-ed.  Read-only operations (``search``,
  ``read``, etc.) do not modify any file.
* Code-fenced blocks (```` ``` ... ``` ```` and ```` `inline` ````) are
  preserved verbatim \u2014 they may legitimately contain example
  ``[[wikilink]]`` syntax that should not be rewritten.
* Wikilinks pointing into the same file the agent just created are
  resolved against the union of (existing vault notes) \u222a (notes
  created by this run), so a self-referential link inside a fresh note
  is never flagged.

Wikilink syntax accepted
------------------------
* ``[[Target]]``                        bare link
* ``[[Target|alias]]``                  display alias
* ``[[Target#Section]]``                 section anchor
* ``[[Target#Section|alias]]``           both
* ``[[Path/With/Slashes/Target]]``       path-style link
* ``[[Target.md]]``                     explicit extension (tolerated)

Output format for unresolved links
----------------------------------
* ``[[Target]]``           \u2192 ``Target [possible linkout - elaboration needed]``
* ``[[Target|alias]]``     \u2192 ``alias [possible linkout - elaboration needed]``
* ``[[Target#Section]]``   \u2192 ``Target (\u00a7Section) [possible linkout - elaboration needed]``
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Regexes
# ---------------------------------------------------------------------------

#: Matches one wikilink.  Capture groups:
#:   1 = target (cannot contain ``]``, ``|`` or ``#``)
#:   2 = ``#section`` part including the leading ``#`` (optional)
#:   3 = ``section`` body without the ``#`` (optional)
#:   4 = ``|alias`` part including the leading ``|`` (optional)
#:   5 = ``alias`` body without the ``|`` (optional)
_WIKILINK_RE = re.compile(
    r"\[\["
    r"([^\]\|#]+?)"          # 1: target
    r"(#([^\]\|]+))?"        # 2,3: optional section
    r"(\|([^\]]+))?"         # 4,5: optional alias
    r"\]\]"
)

#: Fenced code blocks (``` ``` ... ``` ```).  ``re.DOTALL`` lets ``.``
#: match newlines so the entire block is captured.
_FENCED_CODE_RE = re.compile(r"```.*?```", re.DOTALL)

#: Inline code (`` `code` ``).  No newlines allowed.
_INLINE_CODE_RE = re.compile(r"`[^`\n]+`")

#: Sentinel used to stash code blocks while running the wikilink regex.
_BLOCK_SENTINEL = "\x00KBLINK\x00BLOCK\x00{idx}\x00"


# ---------------------------------------------------------------------------
# Public types
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class UnresolvedLink:
    """One wikilink that pointed to a non-existent note."""
    target:  str
    alias:   str | None
    section: str | None


@dataclass
class FileSweepResult:
    """Result of sweeping one file."""
    path:        Path
    replaced:    list[UnresolvedLink] = field(default_factory=list)
    new_content: str = ""

    @property
    def changed(self) -> bool:
        return bool(self.replaced)


@dataclass
class SweepStats:
    """Aggregate stats returned to the pipeline / metadata."""
    files_examined:  int = 0
    files_modified:  int = 0
    links_replaced:  int = 0
    examples:        list[str] = field(default_factory=list)
    #: Diagnostic counters for the case where the sweep walks the plan
    #: but ends up examining zero files.  Useful for distinguishing
    #: "plan was empty" from "plan/disk drift" (Obsidian's auto-
    #: disambiguation, agent issued non-create writes, etc.).
    files_input:     int = 0   # raw plan-derived paths handed to the sweep
    skipped_outside_root: int = 0
    skipped_not_a_file:   int = 0
    skipped_non_markdown: int = 0

    def as_metadata(self) -> dict[str, object]:
        return {
            "link_sweep_examined":             self.files_examined,
            "link_sweep_modified":             self.files_modified,
            "link_sweep_replaced":             self.links_replaced,
            "link_sweep_examples":             self.examples[:20],
            "link_sweep_input":                self.files_input,
            "link_sweep_skipped_outside_root": self.skipped_outside_root,
            "link_sweep_skipped_not_a_file":   self.skipped_not_a_file,
            "link_sweep_skipped_non_markdown": self.skipped_non_markdown,
        }


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _build_existing_index(vault_root: Path, sources_dir: Path) -> set[str]:
    """Return the set of identifiers a wikilink might resolve to.

    Includes:
      * basename without ``.md`` extension (``Foo.md`` \u2192 ``Foo``)
      * full vault-relative path without extension
        (``Topics/Foo.md`` \u2192 ``Topics/Foo``)
      * full vault-relative path WITH extension
        (``Topics/Foo.md`` \u2192 ``Topics/Foo.md``)

    These three forms cover every Obsidian wikilink resolution mode.
    Files under ``sources_dir`` are excluded (the agent must never
    target them).
    \"\"\"
    """
    out: set[str] = set()
    sources_str = str(sources_dir.resolve())

    if not vault_root.exists():
        return out

    for md in vault_root.rglob("*.md"):
        try:
            real = str(md.resolve())
        except OSError:
            continue
        if real == sources_str or real.startswith(sources_str + "/"):
            continue
        # Skip Obsidian's own metadata.
        rel_parts = md.relative_to(vault_root).parts
        if rel_parts and rel_parts[0] in {".obsidian", ".trash"}:
            continue

        rel = md.relative_to(vault_root)
        out.add(md.stem)                          # basename
        out.add(str(rel.with_suffix("")))         # path without .md
        out.add(str(rel))                         # path with .md
    return out


def _stringify_unresolved(link: UnresolvedLink) -> str:
    """Convert an unresolved wikilink to its plain-text replacement."""
    display = link.alias if link.alias else link.target
    if link.section:
        display = f"{display} (\u00a7{link.section})"
    return f"{display} [possible linkout - elaboration needed]"


def _is_resolved(target: str, existing: set[str]) -> bool:
    """Check whether a wikilink target resolves to an existing note."""
    if target in existing:
        return True
    # Tolerate ``[[Foo.md]]`` style (Obsidian accepts but rarely emits).
    if target.endswith(".md") and target[:-3] in existing:
        return True
    # Without ``.md`` suffix.
    if (target + ".md") in existing:
        return True
    return False


# ---------------------------------------------------------------------------
# Core: sweep one chunk of markdown
# ---------------------------------------------------------------------------


def sweep_links_in_text(
    content:  str,
    existing: set[str],
) -> tuple[str, list[UnresolvedLink]]:
    """Replace every unresolved wikilink in ``content`` with a placeholder.

    Returns
    -------
    (new_content, replaced)
        ``new_content`` is the rewritten markdown.  ``replaced`` is the
        list of links that were rewritten (in document order).  Code-
        fenced and inline-code spans are preserved verbatim.
    \"\"\"
    """
    # Stash code blocks behind sentinels so the wikilink regex can't
    # touch them.  Order matters: fenced before inline so that a
    # ```...``` block containing inline back-ticks isn't double-stashed.
    stash: list[str] = []

    def _stash(match: re.Match[str]) -> str:
        stash.append(match.group(0))
        return _BLOCK_SENTINEL.format(idx=len(stash) - 1)

    work = _FENCED_CODE_RE.sub(_stash, content)
    work = _INLINE_CODE_RE.sub(_stash, work)

    replaced: list[UnresolvedLink] = []

    def _swap(match: re.Match[str]) -> str:
        target  = match.group(1).strip()
        section = match.group(3).strip() if match.group(3) else None
        alias   = match.group(5).strip() if match.group(5) else None

        if _is_resolved(target, existing):
            return match.group(0)

        link = UnresolvedLink(target=target, alias=alias, section=section)
        replaced.append(link)
        return _stringify_unresolved(link)

    swept = _WIKILINK_RE.sub(_swap, work)

    # Restore code blocks.
    for i, block in enumerate(stash):
        swept = swept.replace(_BLOCK_SENTINEL.format(idx=i), block)

    return swept, replaced


# ---------------------------------------------------------------------------
# Public entry point
# ---------------------------------------------------------------------------


def sweep_files(
    files:        Iterable[Path],
    vault_root:   Path,
    sources_dir:  Path,
    agent_root:   Path,
) -> SweepStats:
    """Sweep every file in ``files`` for unresolved wikilinks.

    For each file inside ``agent_root`` that contains at least one
    unresolved wikilink, the file is rewritten in-place with placeholder
    text in lieu of the broken links.

    Files outside ``agent_root`` are silently skipped \u2014 the sweeper is
    not authorised to modify user-authored content.

    Returns aggregate :class:`SweepStats` for inclusion in the
    processor's result metadata.
    \"\"\"
    """
    stats = SweepStats()

    agent_root_resolved = agent_root.resolve()
    existing            = _build_existing_index(vault_root, sources_dir)

    seen: set[Path] = set()
    for raw in files:
        stats.files_input += 1
        try:
            path = raw.resolve()
        except OSError:
            continue
        if path in seen:
            continue
        seen.add(path)

        # Confine to agent_root \u2014 never modify anything outside.
        try:
            path.relative_to(agent_root_resolved)
        except ValueError:
            logger.debug(
                "link_sweep: skipping %s (outside agent_root %s)",
                path, agent_root_resolved,
            )
            stats.skipped_outside_root += 1
            continue

        if not path.is_file():
            # Plan path drifted from disk reality — e.g. Obsidian's
            # auto-disambiguation rewrote `Foo.md` to `Foo 1.md` when
            # `Foo.md` already existed, but the plan still records the
            # requested path.  Surface this loudly because it means a
            # newly-created note is sitting on disk under a different
            # name and may be carrying unresolved wikilinks the sweep
            # never gets to inspect.
            logger.warning(
                "link_sweep: plan path %s does not exist on disk "
                "(plan/disk drift; possibly Obsidian auto-disambiguation)",
                path,
            )
            stats.skipped_not_a_file += 1
            continue
        # Only markdown files have wikilinks worth sweeping.
        if path.suffix.lower() != ".md":
            stats.skipped_non_markdown += 1
            continue

        try:
            content = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            logger.warning("link_sweep: cannot read %s: %s", path, exc)
            continue

        stats.files_examined += 1

        new_content, replaced = sweep_links_in_text(content, existing)
        if not replaced:
            continue

        try:
            path.write_text(new_content, encoding="utf-8")
        except OSError as exc:
            logger.warning("link_sweep: cannot write %s: %s", path, exc)
            continue

        stats.files_modified += 1
        stats.links_replaced += len(replaced)
        for link in replaced[:5]:                      # cap per-file
            stats.examples.append(f"{path.name}: [[{link.target}]]")

        logger.info(
            "link_sweep: %s \u2014 replaced %d unresolved link(s): %s",
            path.name,
            len(replaced),
            [link.target for link in replaced],
        )

    return stats


# ---------------------------------------------------------------------------
# Plan helpers
# ---------------------------------------------------------------------------


def files_touched_by_plan(plan_entries: list, vault_root: Path) -> list[Path]:
    """Extract the absolute paths the plan claims to have created or
    modified (apply-mode entries with ``applied=True``).

    Read-only ops (``search``, ``read``, etc.) are excluded.
    """
    write_cmds = {"create", "append", "prepend"}
    out: list[Path] = []
    for entry in plan_entries:
        if not getattr(entry, "applied", False):
            continue
        if entry.cmd not in write_cmds:
            continue
        kv: dict[str, str] = {}
        for tok in entry.args:
            eq = tok.find("=")
            if eq > 0:
                kv[tok[:eq]] = tok[eq + 1:]
        raw = kv.get("path") or kv.get("file") or ""
        if not raw:
            continue
        p = Path(raw)
        if not p.is_absolute():
            p = vault_root / p
        out.append(p)
    return out
