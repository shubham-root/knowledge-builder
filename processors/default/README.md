# kb-processor — Knowledge Builder default processor

The reference processor implementation invoked by the `kb` daemon for each new
source file. Reads a JSON descriptor on stdin, runs an OCR + LLM pipeline, and
emits a JSON result describing the markdown notes and assets it produced.

See the top-level `PLAN.md` (§8 Processor Contract) for the wire protocol.

## Install (editable)

```bash
pip install -e .
# Optional extras:
pip install -e ".[vision]"     # OpenAI vision for image description
pip install -e ".[llm]"        # litellm (multi-provider)
pip install -e ".[anthropic]"  # direct Claude support
pip install -e ".[all]"        # everything
```

## Invocation

The daemon calls the entry-point script created by pip:

```bash
kb-processor <input_path> <work_dir> < descriptor.json
```

Or equivalently:

```bash
python3 -m kb_processor <input_path> <work_dir> < descriptor.json
```

The last line of stdout is a JSON `ProcessResult` object.
