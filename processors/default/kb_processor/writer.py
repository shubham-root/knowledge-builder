"""
Atomic output writer (stub).

Implements the write-to-temp-then-replace pattern required by the processor
contract (PLAN.md §8 "Rules"):

    "Outputs MUST be written via temp-file + os.replace for atomicity."

Usage::

    from kb_processor.writer import atomic_write_text, atomic_write_bytes

    path = vault_root / "Notes" / "Foo.md"
    atomic_write_text(path, markdown_content, work_dir)

The ``work_dir`` argument is used as the directory for the temporary file so
that the ``os.replace`` call is guaranteed to be on the same filesystem as the
final destination (avoiding EXDEV cross-device errors on macOS when the vault
and the caches volume differ).
"""

from __future__ import annotations

import logging
import os
import tempfile
from pathlib import Path

logger = logging.getLogger(__name__)


def atomic_write_text(
    dest: Path,
    content: str,
    work_dir: Path,
    encoding: str = "utf-8",
) -> int:
    """
    Write *content* to *dest* atomically via a temporary file in *work_dir*.

    Creates all parent directories of *dest* if they do not already exist.

    Returns the number of bytes written.
    Raises :class:`OSError` on any filesystem failure.
    """
    return _atomic_write(dest, content.encode(encoding), work_dir)


def atomic_write_bytes(dest: Path, data: bytes, work_dir: Path) -> int:
    """
    Write *data* to *dest* atomically via a temporary file in *work_dir*.

    Creates all parent directories of *dest* if they do not already exist.

    Returns the number of bytes written.
    Raises :class:`OSError` on any filesystem failure.
    """
    return _atomic_write(dest, data, work_dir)


def _atomic_write(dest: Path, data: bytes, work_dir: Path) -> int:
    """
    Core implementation: write *data* to *dest* atomically.

    Steps:
      1. ``dest.parent.mkdir(parents=True, exist_ok=True)`` — create destination
         directory hierarchy.
      2. Write *data* to a ``NamedTemporaryFile`` inside *work_dir*.
      3. ``os.replace(tmp_path, dest)`` — atomic rename on POSIX (same-device).

    Returns the byte count written.
    """
    dest = dest.resolve()
    dest.parent.mkdir(parents=True, exist_ok=True)
    work_dir.mkdir(parents=True, exist_ok=True)

    # Use delete=False so we can rename the file after writing.
    # The suffix preserves the destination extension for easier debugging.
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
        # Clean up the temp file if the write fails.
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError:
            pass
        raise

    # Atomic rename: on POSIX this is guaranteed to be atomic as long as src
    # and dst are on the same filesystem (which they are because work_dir is
    # a subdirectory of the vault's cache path).
    os.replace(tmp_path, dest)
    logger.debug("Wrote %d bytes atomically to %s", len(data), dest)
    return len(data)
