"""
Base extractor interface.

All concrete extractors must subclass :class:`BaseExtractor` and implement
:meth:`BaseExtractor.can_handle` and :meth:`BaseExtractor.extract`.
The return type :class:`ExtractionResult` is a plain dataclass that carries
the extracted markdown content, paths to any saved image assets, and optional
structured metadata for downstream use by the LLM pipeline.
"""

from __future__ import annotations

import abc
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class ExtractionResult:
    """
    Raw extraction output produced by a :class:`BaseExtractor`.

    Attributes
    ----------
    content:
        Markdown-formatted representation of the document content.  May include
        structural hints (headings, list markers, table syntax) if the extractor
        can infer them, but must NOT contain LLM-generated prose.
    images:
        List of absolute :class:`~pathlib.Path` objects pointing to image assets
        that were saved to the per-job ``work_dir`` during extraction.  The LLM
        pipeline stage is responsible for moving these into the vault.
    metadata:
        Optional free-form dict carrying extractor-specific information such as
        page count, author, sheet names, slide count, image dimensions, etc.
    """

    content: str = ""
    images: list[Path] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)


class BaseExtractor(abc.ABC):
    """
    Abstract base class for all Knowledge Builder extractors.

    Subclasses must implement :meth:`can_handle` and :meth:`extract`.

    The two-method interface mirrors a classic strategy pattern:

    - :meth:`can_handle` is used by the pipeline to select the right extractor
      for a given file path (checked by extension and/or magic bytes).
    - :meth:`extract` performs the actual extraction and returns an
      :class:`ExtractionResult`.
    """

    @abc.abstractmethod
    def can_handle(self, path: Path) -> bool:
        """
        Return ``True`` if this extractor can process *path*.

        Implementations typically check the file extension (case-insensitively)
        and may also perform a quick magic-bytes probe for robustness.

        Parameters
        ----------
        path:
            Absolute path to the candidate source file.

        Returns
        -------
        bool
            ``True`` if this extractor should be used; ``False`` otherwise.
        """

    @abc.abstractmethod
    def extract(self, input_path: Path, work_dir: Path) -> ExtractionResult:
        """
        Extract content from *input_path*, writing any image assets to *work_dir*.

        Parameters
        ----------
        input_path:
            Absolute path to the source file to extract from.
        work_dir:
            Per-job working directory.  Extractors SHOULD write transient
            artefacts (e.g. rendered page images) here; the daemon cleans this
            directory after the job completes successfully.

        Returns
        -------
        ExtractionResult
            The raw extraction result.  Must never return ``None``.

        Raises
        ------
        ExtractionError
            If the file cannot be opened or parsed.  The pipeline will catch
            this and produce a retryable ProcessorResultError.
        """

    def __repr__(self) -> str:
        return f"{type(self).__name__}()"


class ExtractionError(Exception):
    """
    Raised by :meth:`BaseExtractor.extract` when extraction fails.

    Parameters
    ----------
    message:
        Human-readable description of the failure.
    retryable:
        Whether the failure is likely transient (e.g. I/O error) and the job
        should be retried by the daemon.  Defaults to ``True``.
    """

    def __init__(self, message: str, *, retryable: bool = True) -> None:
        super().__init__(message)
        self.retryable = retryable
