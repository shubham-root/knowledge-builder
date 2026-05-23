"""
kb_processor — Knowledge Builder reference processor.

Receives a source file (PDF, DOCX, XLSX, PPTX, image) via a JSON descriptor
on stdin, extracts content, synthesises markdown notes via an LLM, and writes
outputs into the Obsidian vault.

Entry point: ``python3 -m kb_processor <input_path> <work_dir>``
"""

__version__ = "0.1.0"
