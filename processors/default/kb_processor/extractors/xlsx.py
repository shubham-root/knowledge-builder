"""
XLSX extractor — uses ``docling`` to convert Excel workbooks to Markdown tables.

docling's ``DocumentConverter`` converts each worksheet into one or more
``TableItem`` objects inside a ``DoclingDocument``.  Calling
``doc.export_to_markdown()`` renders every table as a GFM pipe-table.  Merged
cells are handled transparently by docling's internal table-flattening logic.
Empty sheets produce no table output and are recorded in metadata only.

Structured output contract
--------------------------
The :class:`ExtractionResult` returned by :meth:`XlsxExtractor.extract` uses:

* ``content`` — full Markdown representation with all sheets rendered as tables.
* ``images``   — always empty (spreadsheets carry no raster images in typical use).
* ``metadata`` — dict with keys:

  .. code-block:: python

      {
          "table_count": int,           # number of tables found
          "sheets": [                   # one entry per table / sheet
              {
                  "name":    str,       # sheet name or "Sheet N" fallback
                  "content": str,       # Markdown table for this sheet
              },
          ],
      }
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from .base import BaseExtractor, ExtractionError, ExtractionResult

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Module-level helpers
# ---------------------------------------------------------------------------

def _import_docling() -> Any:
    """Return ``DocumentConverter`` class or raise :class:`ExtractionError`."""
    try:
        from docling.document_converter import DocumentConverter  # type: ignore[import]
        return DocumentConverter
    except ImportError as exc:
        raise ExtractionError(
            "docling is not installed — run: pip install 'docling>=2.0.0'",
            retryable=False,
        ) from exc


def _check_conversion_status(result: Any, path: Path) -> None:
    """Raise :class:`ExtractionError` if docling reports a hard failure."""
    try:
        status_str = str(result.status).lower()
        if "fail" in status_str:
            raise ExtractionError(
                f"docling conversion failed for '{path}': status={result.status}",
                retryable=False,
            )
    except AttributeError:
        pass


def _is_transient_error(message: str) -> bool:
    """Classify I/O errors as transient vs. format errors as permanent."""
    msg_lower = message.lower()
    permanent_hints = ("password", "encrypt", "protected", "corrupt",
                       "not a valid", "invalid", "unsupported format")
    return not any(hint in msg_lower for hint in permanent_hints)


def _sheet_name_from_table(table: Any, doc: Any, index: int) -> str:
    """
    Best-effort sheet name from a docling ``TableItem``.

    Tries caption_text(doc), then the .caption attribute, then falls
    back to ``"Sheet {index}"`` (1-based).
    """
    # Try callable caption_text(doc)
    try:
        text = table.caption_text(doc).strip()
        if text:
            return text
    except Exception:  # noqa: BLE001
        pass

    # Try plain .caption attribute
    try:
        text = str(table.caption).strip()
        if text and text.lower() not in ("none", ""):
            return text
    except Exception:  # noqa: BLE001
        pass

    return f"Sheet {index}"


def _extract_sheets(doc: Any) -> list[dict[str, str]]:
    """
    Build the ``sheets`` metadata list from docling table items.

    Returns a list of ``{"name": str, "content": str}`` dicts.  Empty
    tables (no rows) produce an empty *content* string but are still listed.
    """
    sheets: list[dict[str, str]] = []
    try:
        tables = list(doc.tables)
    except Exception as exc:  # noqa: BLE001
        logger.warning("Could not iterate document tables: %s", exc)
        return sheets

    for idx, table in enumerate(tables, start=1):
        name = _sheet_name_from_table(table, doc, idx)
        content = ""
        try:
            content = table.export_to_markdown().strip()
        except Exception as exc:  # noqa: BLE001
            logger.warning("Could not export table %d to Markdown: %s", idx, exc)
        sheets.append({"name": name, "content": content})
        logger.debug("Sheet %d ('%s'): %d chars.", idx, name, len(content))

    return sheets


# ---------------------------------------------------------------------------
# Public extractor class
# ---------------------------------------------------------------------------

class XlsxExtractor(BaseExtractor):
    """
    Extract cell data from an ``.xlsx`` file using ``docling``.
    """

    #: File extensions handled by this extractor.
    EXTENSIONS: frozenset[str] = frozenset({".xlsx", ".xls"})

    def can_handle(self, path: Path) -> bool:
        """Return ``True`` for ``.xlsx`` / ``.xls`` files."""
        return path.suffix.lower() in self.EXTENSIONS

    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Convert the XLSX workbook at *input_path* to Markdown tables.

        Parameters
        ----------
        input_path:
            Absolute path to the ``.xlsx`` source file.
        work_dir:
            Per-job working directory (not used for XLSX but required by the
            :class:`BaseExtractor` interface).

        Returns
        -------
        ExtractionResult
            ``content`` is the full Markdown; ``images`` is always empty;
            ``metadata`` has ``table_count`` and a ``sheets`` list.

        Raises
        ------
        ExtractionError
            * ``retryable=False`` — corrupt/encrypted workbook or docling not installed.
            * ``retryable=True``  — transient I/O error.
        """
        DocumentConverter = _import_docling()  # noqa: N806
        logger.info("XlsxExtractor: converting '%s'", input_path)

        try:
            converter = DocumentConverter()
            result = converter.convert(str(input_path))
        except ExtractionError:
            raise
        except Exception as exc:  # noqa: BLE001
            msg = str(exc)
            raise ExtractionError(
                f"docling failed to convert XLSX '{input_path}': {msg}",
                retryable=_is_transient_error(msg),
            ) from exc

        _check_conversion_status(result, input_path)

        try:
            doc = result.document
            markdown: str = doc.export_to_markdown()
        except Exception as exc:  # noqa: BLE001
            raise ExtractionError(
                f"Failed to export XLSX '{input_path}' to Markdown: {exc}",
                retryable=False,
            ) from exc

        if not markdown.strip():
            logger.info("XlsxExtractor: '%s' produced no content (all empty sheets).", input_path)
            return ExtractionResult(content="", images=[], metadata={"table_count": 0, "sheets": []})

        sheets = _extract_sheets(doc)
        logger.info("XlsxExtractor: done — %d chars, %d sheet(s).", len(markdown), len(sheets))
        return ExtractionResult(
            content=markdown,
            images=[],
            metadata={"table_count": len(sheets), "sheets": sheets},
        )
