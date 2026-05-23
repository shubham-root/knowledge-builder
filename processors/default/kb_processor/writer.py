"""
Atomic output writer for the Knowledge Builder processor.

Implements PLAN.md §3.9 and §8 "Rules":

    "Outputs MUST be written via temp-file + os.replace for atomicity."
    "The processor writes outputs to a per-job working directory and
     os.replace()s them into final vault locations only after all writes
     succeed."

Two APIs are provided:

1. **Standalone helpers** (legacy / simple use-cases):

   .. code-block:: python

       from kb_processor.writer import atomic_write_text, atomic_write_bytes

       bytes_written = atomic_write_text(dest_path, markdown_text, work_dir)
       bytes_written = atomic_write_bytes(dest_path, raw_bytes, work_dir)

2. **AtomicWriter** (all-or-nothing staging for multi-output jobs):

   .. code-block:: python

       from kb_processor.writer import AtomicWriter

       writer = AtomicWriter(work_dir, vault_root, sources_dir)
       writer.stage(markdown_text, vault_root / "Notes" / "Foo.md", "markdown")
       writer.stage_copy(extracted_img, vault_root / "Notes" / "fig1.png", "asset")

       try:
           records = writer.commit()   # all-or-nothing os.replace
       except WriteError:
           writer.rollback()           # clean up temps + any partial commits
           raise

Path invariant
--------------
Every output path MUST satisfy:

    resolved ⊂ vault_root  AND  resolved ⊄ sources_dir

Violations raise :class:`PathViolation` immediately in ``stage``/``stage_copy``
before any data is written to disk.
"""

from __future__ import annotations

import logging
import os
import shutil
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Union

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class PathViolation(Exception):
    """Raised when a staged output path violates the vault containment invariant.

    Either the path is outside ``vault_root`` or it is inside ``sources_dir``.
    This is a programmer / processor bug — never retryable.
    """


class WriteError(Exception):
    """Raised when a staged write cannot be committed to its final location.

    Wraps the underlying :class:`OSError` so callers can inspect ``__cause__``.
    """


# ---------------------------------------------------------------------------
# Data containers
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class StagedOutput:
    """Describes a single write that has been staged but not yet committed.

    Attributes:
        temp_path:  Absolute path to the temporary file inside ``work_dir``.
        final_path: Absolute canonical path where the file will land after commit.
        kind:       Descriptor string, e.g. ``"markdown"`` or ``"asset"``.
        bytes:      Number of bytes written to the temp file.
    """

    temp_path: Path
    final_path: Path
    kind: str
    bytes: int


@dataclass(frozen=True)
class OutputRecord:
    """A committed output suitable for inclusion in the JSON result.

    Mirrors the ``OutputEntry`` schema expected by the Rust daemon
    (PLAN.md §8) without pulling in the Pydantic dependency at commit time.

    Attributes:
        path:  Absolute canonical path of the committed file.
        kind:  Descriptor string, e.g. ``"markdown"`` or ``"asset"``.
        bytes: Size of the committed file in bytes.
    """

    path: Path
    kind: str
    bytes: int


# ---------------------------------------------------------------------------
# AtomicWriter
# ---------------------------------------------------------------------------


class AtomicWriter:
    """Stage multiple outputs, then commit all-or-nothing into the vault.

    **Lifecycle**::

        writer = AtomicWriter(work_dir, vault_root, sources_dir)
        # --- staging phase ---
        writer.stage(text, vault_root / "Notes" / "Foo.md", "markdown")
        writer.stage_copy(img_path, vault_root / "Notes" / "fig.png", "asset")
        # --- commit phase ---
        records = writer.commit()   # atomic os.replace for every staged write
        # On failure:
        # writer.rollback()         # delete temps + any partially-placed files

    All path arguments are resolved to canonical absolute paths via
    :meth:`_validate_path` before any I/O is performed.  Violations are
    reported immediately as :class:`PathViolation`.

    Parameters:
        work_dir:    Directory for temporary files.  Must be on the same
                     filesystem as the vault so that ``os.replace`` is truly
                     atomic (no EXDEV cross-device errors).
        vault_root:  Absolute path to the Obsidian vault root.
        sources_dir: Absolute path to the sources sub-directory inside the vault.
    """

    def __init__(self, work_dir: Path, vault_root: Path, sources_dir: Path) -> None:
        self.work_dir = work_dir
        self.vault_root = vault_root
        self.sources_dir = sources_dir

        # Pending writes: staged but not yet committed.
        self._staged: list[StagedOutput] = []

        # Writes that have been committed (os.replace succeeded).
        # Used by rollback() to undo a partial commit.
        self._committed: list[StagedOutput] = []

        # Ensure the working directory exists up front so all mkstemp calls
        # have a valid directory to write into.
        work_dir.mkdir(parents=True, exist_ok=True)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def stage(
        self,
        content: Union[str, bytes],
        final_path: Path,
        kind: str,
    ) -> StagedOutput:
        """Stage a write of *content* to *final_path*.

        Validates the path, writes *content* to a temp file in ``work_dir``,
        records the pending write, and returns a :class:`StagedOutput`
        descriptor.

        Parameters:
            content:    String or bytes to write.  Strings are encoded as UTF-8.
            final_path: Desired destination path inside the vault (not sources).
            kind:       Output kind label, e.g. ``"markdown"`` or ``"asset"``.

        Returns:
            A :class:`StagedOutput` describing the pending write.

        Raises:
            PathViolation: If *final_path* is outside the vault or inside
                sources.
            OSError: If the temp file cannot be created or written.
        """
        validated = self._validate_path(final_path)
        data: bytes = content.encode("utf-8") if isinstance(content, str) else content
        temp_path, nbytes = self._write_temp(data, validated.suffix or ".tmp")
        staged = StagedOutput(
            temp_path=temp_path,
            final_path=validated,
            kind=kind,
            bytes=nbytes,
        )
        self._staged.append(staged)
        logger.debug(
            "Staged %d bytes → %s (kind=%s, temp=%s)",
            nbytes,
            validated,
            kind,
            temp_path,
        )
        return staged

    def stage_copy(
        self,
        source_path: Path,
        final_path: Path,
        kind: str,
    ) -> StagedOutput:
        """Stage a copy of an existing file at *source_path* to *final_path*.

        Validates *final_path*, copies *source_path* into a temp file in
        ``work_dir``, records the pending write, and returns a
        :class:`StagedOutput` descriptor.

        Parameters:
            source_path: Existing file to copy.
            final_path:  Desired destination path inside the vault (not sources).
            kind:        Output kind label.

        Returns:
            A :class:`StagedOutput` describing the pending write.

        Raises:
            PathViolation: If *final_path* is outside the vault or inside
                sources.
            OSError: If the source cannot be read or the temp file cannot be
                written.
        """
        validated = self._validate_path(final_path)
        source_path = source_path.resolve()
        nbytes = source_path.stat().st_size  # read size before copy
        temp_path = self._copy_to_temp(source_path, validated.suffix or ".tmp")
        # Refresh byte count from the actual temp file (handles sparse files etc.)
        actual_bytes = temp_path.stat().st_size
        staged = StagedOutput(
            temp_path=temp_path,
            final_path=validated,
            kind=kind,
            bytes=actual_bytes,
        )
        self._staged.append(staged)
        logger.debug(
            "Staged copy %d bytes %s → %s (kind=%s, temp=%s)",
            actual_bytes,
            source_path,
            validated,
            kind,
            temp_path,
        )
        return staged

    def commit(self) -> list[OutputRecord]:
        """Commit all staged writes atomically into the vault.

        For each staged write:

        1. Create all parent directories of the final path.
        2. ``os.replace(temp_path, final_path)`` — atomic on the same
           filesystem (POSIX guarantee).

        If **any** replace fails:

        - All already-placed files from this commit are removed (best effort).
        - All remaining temp files are deleted.
        - :class:`WriteError` is raised wrapping the original :class:`OSError`.

        Returns:
            List of :class:`OutputRecord` instances (one per staged write)
            ready for inclusion in the JSON result payload.

        Raises:
            WriteError: If any ``os.replace`` fails.
        """
        if not self._staged:
            return []

        placed_this_commit: list[StagedOutput] = []

        try:
            for staged in self._staged:
                staged.final_path.parent.mkdir(parents=True, exist_ok=True)
                try:
                    os.replace(staged.temp_path, staged.final_path)
                except OSError as exc:
                    raise WriteError(
                        f"os.replace failed: {staged.temp_path} → "
                        f"{staged.final_path}: {exc}"
                    ) from exc
                placed_this_commit.append(staged)
                self._committed.append(staged)
                logger.debug(
                    "Committed %s → %s", staged.temp_path, staged.final_path
                )
        except WriteError:
            # Rollback the files placed in *this* commit attempt only.
            self._undo_placed(placed_this_commit)
            # Clean up remaining temp files (those not yet replaced).
            self._cleanup_temps(
                staged
                for staged in self._staged
                if staged not in placed_this_commit
            )
            raise

        # Clear the staged list — all writes are now committed.
        records = [
            OutputRecord(path=s.final_path, kind=s.kind, bytes=s.bytes)
            for s in self._staged
        ]
        self._staged.clear()
        return records

    def rollback(self) -> None:
        """Roll back all staged and committed writes.

        - Deletes all remaining temp files in ``work_dir`` that belong to
          staged (not yet committed) writes.
        - Deletes all files that were previously committed by :meth:`commit`
          (best effort; errors are logged but not re-raised).

        This method is safe to call multiple times.
        """
        # Remove un-committed temp files.
        self._cleanup_temps(self._staged)
        self._staged.clear()

        # Remove already-committed vault files (best effort).
        self._undo_placed(self._committed)
        self._committed.clear()

    # ------------------------------------------------------------------
    # Path validation
    # ------------------------------------------------------------------

    def _validate_path(self, path: Path) -> Path:
        """Resolve *path* and verify the vault containment invariant.

        Parameters:
            path: The desired output destination.

        Returns:
            The canonicalized (resolved) absolute path.

        Raises:
            PathViolation: If *path* is outside ``vault_root`` or inside
                ``sources_dir``.
        """
        resolved = path.resolve()
        vault_resolved = self.vault_root.resolve()
        sources_resolved = self.sources_dir.resolve()

        # Ensure vault_resolved ends with a separator so that startswith
        # comparisons are component-accurate (avoids false positives when
        # vault_root="/vault" and resolved="/vault-backup/notes/foo.md").
        vault_str = str(vault_resolved)
        if not vault_str.endswith(os.sep):
            vault_str += os.sep

        sources_str = str(sources_resolved)
        if not sources_str.endswith(os.sep):
            sources_str += os.sep

        resolved_str = str(resolved)
        # Allow exact match with vault_root itself (edge case; normally outputs
        # are always at least one level deeper, but we follow the spec strictly).
        if not (
            resolved_str.startswith(vault_str)
            or resolved_str == str(vault_resolved)
        ):
            raise PathViolation(f"Output outside vault: {resolved}")

        if resolved_str.startswith(sources_str) or resolved_str == str(
            sources_resolved
        ):
            raise PathViolation(f"Output inside sources: {resolved}")

        return resolved

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _write_temp(self, data: bytes, suffix: str) -> tuple[Path, int]:
        """Write *data* to a new temp file in ``work_dir``.

        Returns:
            ``(temp_path, bytes_written)``
        """
        fd, tmp_str = tempfile.mkstemp(suffix=suffix, dir=self.work_dir)
        tmp_path = Path(tmp_str)
        try:
            with os.fdopen(fd, "wb") as fh:
                fh.write(data)
                fh.flush()
                os.fsync(fh.fileno())
        except Exception:
            tmp_path.unlink(missing_ok=True)
            raise
        return tmp_path, len(data)

    def _copy_to_temp(self, source: Path, suffix: str) -> Path:
        """Copy *source* to a new temp file in ``work_dir``.

        Returns the path to the temp file.
        """
        fd, tmp_str = tempfile.mkstemp(suffix=suffix, dir=self.work_dir)
        tmp_path = Path(tmp_str)
        try:
            os.close(fd)  # shutil.copy2 opens the file itself
            shutil.copy2(source, tmp_path)
        except Exception:
            tmp_path.unlink(missing_ok=True)
            raise
        return tmp_path

    def _cleanup_temps(self, staged_items) -> None:
        """Delete temp files for *staged_items* (best effort)."""
        for staged in staged_items:
            try:
                Path(staged.temp_path).unlink(missing_ok=True)
                logger.debug("Cleaned up temp file %s", staged.temp_path)
            except OSError as exc:
                logger.warning(
                    "Could not clean up temp file %s: %s", staged.temp_path, exc
                )

    def _undo_placed(self, placed_items: list[StagedOutput]) -> None:
        """Delete already-committed vault files for *placed_items* (best effort)."""
        for staged in placed_items:
            try:
                Path(staged.final_path).unlink(missing_ok=True)
                logger.debug(
                    "Rolled back committed file %s", staged.final_path
                )
            except OSError as exc:
                logger.warning(
                    "Could not roll back committed file %s: %s",
                    staged.final_path,
                    exc,
                )


# ---------------------------------------------------------------------------
# Standalone helpers (preserved from T34 for simple single-write use-cases)
# ---------------------------------------------------------------------------


def atomic_write_text(
    dest: Path,
    content: str,
    work_dir: Path,
    encoding: str = "utf-8",
) -> int:
    """Write *content* to *dest* atomically via a temporary file in *work_dir*.

    Creates all parent directories of *dest* if they do not already exist.

    Returns the number of bytes written.
    Raises :class:`OSError` on any filesystem failure.
    """
    return _atomic_write(dest, content.encode(encoding), work_dir)


def atomic_write_bytes(dest: Path, data: bytes, work_dir: Path) -> int:
    """Write *data* to *dest* atomically via a temporary file in *work_dir*.

    Creates all parent directories of *dest* if they do not already exist.

    Returns the number of bytes written.
    Raises :class:`OSError` on any filesystem failure.
    """
    return _atomic_write(dest, data, work_dir)


def _atomic_write(dest: Path, data: bytes, work_dir: Path) -> int:
    """Core implementation: write *data* to *dest* atomically.

    Steps:

    1. ``dest.parent.mkdir(parents=True, exist_ok=True)``
    2. Write *data* to a ``NamedTemporaryFile`` inside *work_dir*.
    3. ``os.replace(tmp_path, dest)`` — atomic rename on POSIX (same-device).

    Returns the byte count written.
    """
    dest = dest.resolve()
    dest.parent.mkdir(parents=True, exist_ok=True)
    work_dir.mkdir(parents=True, exist_ok=True)

    fd, tmp_path_str = tempfile.mkstemp(
        suffix=dest.suffix or ".tmp",
        dir=work_dir,
    )
    tmp_path = Path(tmp_path_str)
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
            fh.flush()
            os.fsync(fh.fileno())
    except Exception:
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError:
            pass
        raise

    os.replace(tmp_path, dest)
    logger.debug("Wrote %d bytes atomically to %s", len(data), dest)
    return len(data)
