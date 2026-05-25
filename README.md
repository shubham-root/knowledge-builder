# Knowledge Builder

> A macOS background daemon that watches an Obsidian vault, extracts content from incoming source files (PDF, DOCX, XLSX, PPTX, images), and uses an LLM-driven agent to integrate that content into a curated `KnowledgeBase/` subtree of the vault.

## TL;DR

Drop a PDF into `~/Documents/Obsidian/Sources/`.  A few minutes later you
have a structured Markdown note under `~/Documents/Obsidian/KnowledgeBase/`
that’s been auto-titled, tagged, frontmatter-attributed, and wikilinked
into your existing vault.  No filesystem operations of yours are touched
in the process — the agent is confined to a single subtree of the vault
by a three-layer sandbox.

**Just want to try it?**  Follow the [Quickstart](Quickstart.md) —
~15 minutes from clone to first integrated note.  This README is the
reference.

## Table of contents

1. [What it does (concrete example)](#what-it-does-concrete-example)
2. [Architecture at a glance](#architecture-at-a-glance)
3. [Installation](#installation)
4. [Configuration](#configuration)
5. [Operation modes — shadow vs apply](#operation-modes-shadow-vs-apply)
6. [Daily usage](#daily-usage)
7. [Pipeline internals](#pipeline-internals)
8. [Safety model](#safety-model)
9. [CLI reference](#cli-reference)
10. [Processor contract](#processor-contract)
11. [Troubleshooting](#troubleshooting)
12. [Development](#development)

## What it does (concrete example)

You drop the file `attention-is-all-you-need.pdf` (a research paper)
into the watched `Sources/` folder of your Obsidian vault.

Within a few seconds the daemon notices it.  A few minutes later,
your vault contains:

```
KnowledgeBase/
  Topics/
    Transformer_Architecture.md       # paper note with frontmatter:
                                      #   year: 2017
                                      #   venue: NeurIPS
                                      #   tags: [deep-learning, attention, sequence-models]
                                      #   ...
    Self_Attention.md                 # mechanism note, links to Transformer
  Authors/
    Vaswani_et_al_2017.md             # author note with bibliography links
  Concepts/
    Multi_Head_Attention.md           # concept note with [[Transformer_Architecture]]
```

Nothing was created outside `KnowledgeBase/`.  Your hand-written notes
elsewhere in the vault are untouched.  An audit log records exactly
what the agent did, what it considered, and what tokens it spent.

A shadow run of the same input produces a `.kb-plan.jsonl` plan file
without touching the vault — you inspect it via `kb show <id>` and
promote to apply mode once you trust the agent’s decisions.

## Architecture at a glance

```
  ┌──────────────────────────────────────────────────────────────────┐
  │  macOS user vault   ~/Documents/Obsidian/                        │
  │    Sources/             ◀─ you drop files here                   │
  │    KnowledgeBase/       ◀─ agent's writeable sandbox             │
  │    Notes/, Topics/, ... ◀─ your hand-written content (read-only) │
  │    .obsidian/                                                    │
  └────────────────┬─────────────────────────────────────────────────┘
                   │ FSEvents + periodic full scan
                   ▼
  ┌──────────────────────────────────────────────────────────────────┐
  │  Daemon (Rust, launchd-managed)                                  │
  │    kb-watcher  stability + SHA-256 hash + dedup                  │
  │    kb-core     state.db (SQLite) + state machine                 │
  │    kb-worker   bounded pool, atomic claim, retry / backoff       │
  │    kb-ops      HTTP / SSE on 127.0.0.1                           │
  └────────────────┬─────────────────────────────────────────────────┘
                   │ spawns one Python subprocess per job
                   ▼
  ┌──────────────────────────────────────────────────────────────────┐
  │  kb-processor (Python, dedicated venv)                           │
  │    EXTRACT     docling + MPS, 5-page batches, native/OCR mix     │
  │    STAGE       extracted.md  ▶  work_dir/                        │
  │    INTEGRATE   spawn pi --mode rpc, drive integration agent      │
  └────────────────┬─────────────────────────────────────────────────┘
                   │ spawns pi binary (RPC mode, restricted PATH)
                   ▼
  ┌──────────────────────────────────────────────────────────────────┐
  │  pi --mode rpc  (LLM agent loop)                                 │
  │    one tool: bash                                                │
  │    skill: knowledge-builder-integrator (loaded from package)     │
  │    PATH = curated wrapper dir only                               │
  │    issues kb-obsidian commands (the only mutation primitive)     │
  └────────────────┬─────────────────────────────────────────────────┘
                   │ obsidian CLI (read+write, scoped to KnowledgeBase/)
                   ▼
              [back to vault]
```

Three distinct programs cooperate:

* **The Rust daemon** (`kb`) is the long-running supervisor.  It watches
  the filesystem, manages the queue, applies retry / backoff, runs the
  HTTP ops endpoints, and spawns processor subprocesses.  All persistent
  state lives in a single SQLite database (`state.db`).
* **The Python processor** (`kb-processor`) is a per-job subprocess
  that performs extraction (via `docling`) and orchestrates the agent
  run.  It returns a JSON `ProcessResult` to the daemon.
* **`pi --mode rpc`** is the LLM agent loop.  The processor spawns it,
  registers its skills, and feeds it the integration prompt.  The
  agent’s only tool is `bash`, but its `PATH` is restricted to a
  curated wrapper directory so the only mutation primitive available
  is `kb-obsidian` (a Python policy wrapper around Obsidian’s native
  CLI).

A detailed walk-through of each stage is in [Pipeline
internals](#pipeline-internals).

## Installation

### Prerequisites

| Requirement | Why | How to verify |
|---|---|---|
| **macOS 12+** | FSEvents, launchd, Apple-Silicon MPS for docling. | `sw_vers -productVersion` |
| **Rust 1.75+** (only when building from source) | Compiles `kb`. | `rustc --version` |
| **Python 3.12+** | Hosts the docling pipeline and the agent driver. | `python3 --version` |
| **Obsidian 1.7+** with **Command line interface enabled** | The agent talks to your vault through the `obsidian` CLI. | Obsidian → Settings → General → *Command line interface*; then `which obsidian` |
| **`pi-coding-agent`** | The LLM agent loop. | `which pi && pi --version` |
| An **OpenRouter API key** (or any provider supported by `litellm`) | LLM calls during synthesis and integration. | https://openrouter.ai/keys |
| Local disk space for an ML venv (~2 GB) | docling pulls in `torch`, `transformers`, etc. | n/a |

The daemon is single-user and macOS-only.  Linux/Windows ports are
structurally feasible but currently out of scope.

### Build and install the daemon binary

From a clean clone of this repository:

```bash
cargo build --release          # ~60-90 s on a cold cache (rusqlite is bundled)
cp target/release/kb /usr/local/bin/kb
kb --version
```

### Install pi-coding-agent

The Knowledge Builder agent runs inside `pi --mode rpc`.  Install it
globally so the daemon can find it on PATH:

```bash
npm install -g @earendil-works/pi-coding-agent
which pi          # should resolve to a node shim
pi --version
```

### Enable the Obsidian CLI

Open Obsidian and turn on:

```
Settings → General → Command line interface
```

Follow Obsidian’s prompt to register the binary.  After that:

```bash
which obsidian              # /usr/local/bin/obsidian
obsidian help               # should print a long list of subcommands
obsidian files total        # should print a number
```

The agent will only ever talk to the running Obsidian app via this
CLI — it never edits markdown files directly.

### Set up the Python processor venv

The processor lives in `processors/default/` and is installed into a
dedicated virtualenv to keep its heavy ML dependencies (torch, docling,
transformers) isolated from the system Python.

```bash
python3 -m venv ~/.local/share/kb/venv
~/.local/share/kb/venv/bin/pip install --upgrade pip
~/.local/share/kb/venv/bin/pip install -e "$(pwd)/processors/default[llm]"

# Verify the entry-point script was created.
ls -l ~/.local/share/kb/venv/bin/kb-processor
```

The `[llm]` extra installs `litellm`, which routes every supported
provider (OpenRouter, OpenAI, Anthropic, Bedrock, local Ollama, …)
through one interface.

#### Why a pinned `transformers`

`pyproject.toml` pins `transformers>=5.8.1,<5.9.0`.  Newer transformers
crashes on Apple-Silicon MPS for every PDF page processed by docling
(`TypeError: Cannot convert a MPS Tensor to float64 dtype`).  The pin
can be removed once docling ships against a fixed transformers —
tracked at https://github.com/docling-project/docling/issues/3483.

#### Why a `setdefault PYTORCH_ENABLE_MPS_FALLBACK=1`

The processor sets this env var at import time so any straggler MPS×
float64 ops fall back to CPU instead of crashing.  This is a belt to
the transformers pin’s suspenders.

## Configuration

Knowledge Builder reads from two files in `~/.config/knowledge-builder/`:

| File | Purpose | Mode |
|---|---|---|
| `config.toml` | Paths, watch parameters, worker concurrency, processor command, ops bind, log level. | 644 OK |
| `secrets.env` | Credentials (`OPENROUTER_API_KEY`, `KB_LLM_MODEL`, etc.) and any per-process env the operator wants to forward to the processor subprocess. | **chmod 600** |

These paths are XDG-style on every platform.  The daemon refuses to
start if `secrets.env` exists with permissive (group/world-readable)
perms.

### `config.toml`

A full annotated example:

```toml
[paths]
# Root of your Obsidian vault.  All read/write operations are scoped
# to this directory tree.
vault_root  = "~/Documents/Obsidian"

# Where you drop new source files.  Must be a strict subdirectory of
# vault_root and disjoint from agent_root.  Files placed here are
# treated as inputs to the pipeline.
sources_dir = "~/Documents/Obsidian/Sources"

# The agent's mutation sandbox.  All agent-driven creates / edits /
# deletes are confined to this tree.  Defaults to vault_root/KnowledgeBase.
# Auto-created on daemon startup.
agent_root  = "~/Documents/Obsidian/KnowledgeBase"

# SQLite state file.  Created on first run.
db_path     = "~/Library/Application Support/knowledge-builder/state.db"

# Rotating log directory (one file per day).
log_dir     = "~/Library/Logs/knowledge-builder"

[watch]
# Extensions admitted by the watcher.  Anything else (including .md) is
# silently skipped.
extensions         = ["pdf", "docx", "xlsx", "ppt", "pptx",
                       "jpg", "jpeg", "png"]

# Globs that suppress otherwise-admitted files.  Patterns match the
# full absolute path.
ignore_globs       = [
  "**/.*",            # dotfiles
  "**/~$*",           # Office lock files
  "**/.obsidian/**",  # Obsidian internal state
  "**/*.icloud",      # iCloud placeholders
]

# Milliseconds (size+mtime) must be unchanged before a file is hashed
# and enqueued.  Prevents reading partial writes.
stability_ms       = 2000

# Periodic full-scan interval (seconds).  Catches files missed during
# sleep / iCloud sync stragglers.
poll_interval_secs = 300

# Read chunk for streaming SHA-256.
hash_chunk_bytes   = 1048576

[worker]
# Maximum simultaneous processor subprocesses.  Each one runs docling
# with MPS, so 1–2 is appropriate for an M-series laptop.
concurrency  = 2

# Total attempts per source.  Counted across daemon restarts.
max_attempts = 3

# Per-attempt delay before a retryable failure is re-queued.
backoff_secs = [30, 300, 1800]    # 30 s, 5 min, 30 min

[processor]
# How the daemon spawns the processor.  Default points at the venv we
# set up above.  Can be a bare name on $PATH, an absolute path, or a
# space-separated argv.
command       = "~/.local/share/kb/venv/bin/kb-processor"

# Hard wall-clock timeout per invocation.  On expiry the daemon sends
# SIGTERM to the child's process group, waits 5 s, then SIGKILL.
timeout_secs  = 1800

# Per-job working directories live under here.  Cleaned automatically
# on success; retained on failure for inspection.
work_dir_root = "~/Library/Caches/knowledge-builder/jobs"

[ops]
# HTTP endpoint for `kb status`, `kb tail`, etc.  Loopback only.
http_bind  = "127.0.0.1:7878"

# Daemon log level.
log_level  = "info"            # trace|debug|info|warn|error
log_format = "json"            # json|pretty
```

Every key has a sensible default; `config.toml` only needs to specify
the values you actually want to override.  `kb config show` prints the
fully-resolved configuration.

### `secrets.env`

This is a `KEY=VALUE` file (`.env` syntax: comments start with `#`,
blank lines ignored, optional `export ` prefixes accepted).  The daemon
loads it at startup and forwards every entry into the processor
subprocess via `Command::envs(...)`.

It is the **single source of truth for credentials and per-job
configuration**.  Both `kb daemon --foreground` and the launchd-managed
daemon read from the same file, so behaviour is identical regardless of
how you start the service.

Minimal example:

```dotenv
# litellm-format model id.  Prefix with `openrouter/` to route via
# OpenRouter; the model id after that prefix follows OpenRouter's
# catalogue (https://openrouter.ai/models).
KB_LLM_MODEL=openrouter/anthropic/claude-3.5-haiku

# OpenRouter API key (sk-or-v1-…).  Get one at https://openrouter.ai/keys
OPENROUTER_API_KEY=sk-or-v1-...
```

Additional knobs the processor recognises:

```dotenv
# Toggle the agent.  Default is `apply` — set to `shadow` only when you
# want to inspect a plan before it executes.  See "Operation modes".
KB_AGENT_MODE=apply               # apply | shadow

# Per-job wall-clock budget for the agent (default 600 s).
KB_AGENT_TIMEOUT_SECS=600

# Cap how much extracted text is sent to the LLM (default 50 000).
KB_LLM_MAX_CONTENT_CHARS=80000

# Docling per-batch knobs (rarely touched).
KB_PDF_BATCH_SIZE=5              # pages per batch
KB_PDF_BATCH_TIMEOUT_SECS=300    # per-batch hard timeout
DOCLING_DEVICE=mps               # mps | cuda | cpu | auto
DOCLING_NUM_THREADS=8
```

Create it with restrictive permissions:

```bash
mkdir -p ~/.config/knowledge-builder
touch    ~/.config/knowledge-builder/secrets.env
chmod 600 ~/.config/knowledge-builder/secrets.env
# now edit it
```

`kb doctor` reports the file's status:

* missing  → warning + actionable mkdir/chmod hint;
* mode ≠ 0600 → hard fail (won't start the daemon);
* loaded successfully → prints the **keys only** (never values).

### LaunchAgent (running on login)

For day-to-day operation the daemon runs under launchd:

```bash
kb install
```

This runs `kb doctor`, renders
`installer/com.user.knowledge-builder.plist` into
`~/Library/LaunchAgents/`, and starts the agent immediately via
`launchctl bootstrap` + `launchctl kickstart`.

The plist contains only `PATH` in `EnvironmentVariables` — **secrets
are deliberately not stored there** because plists are mode-644
plaintext and may be picked up by Time Machine, dotfile sync, etc.
All credentials flow through `secrets.env` as described above.

Uninstall:

```bash
kb uninstall                   # bootout + remove plist
```

The SQLite database, log files, and vault contents are preserved.
For a full wipe:

```bash
kb uninstall
rm -rf "~/Library/Application Support/knowledge-builder"
rm -rf  ~/Library/Logs/knowledge-builder
rm -rf  ~/Library/Caches/knowledge-builder
```

### First-run sanity check

```bash
kb doctor
```

All items should print `✓`.  Typical output:

```
  [1] ✓  Config file found at /Users/you/.config/knowledge-builder/config.toml
  [2] ✓  Config parsed successfully.
  [3] ✓  All 8 configuration checks passed.
  [4] ✓  SQLite integrity check passed.
  [5] ✓  Log directory is writable: /Users/you/Library/Logs/knowledge-builder
  [6] ✓  Last backup is 0 day(s) old — within the 7-day window.
  [7] ✓  Secrets file readable with 2 key(s): ["KB_LLM_MODEL", "OPENROUTER_API_KEY"]
```

## Operation modes — shadow vs apply

The agent can run in one of two modes, selected by `KB_AGENT_MODE` in
`secrets.env`.  Both modes use the *same* code path, prompts, skills
and sandbox; the only difference is whether the kb-obsidian wrapper
actually executes mutations or merely records them.

### Apply mode (default)

```
KB_AGENT_MODE=apply
```

* Reads pass through to the real `obsidian` binary.
* Writes also pass through, with each entry annotated `applied: true`
  in the plan file plus the obsidian exit code.
* Obsidian’s own File Recovery plugin (enabled by default) keeps
  per-edit history of every change — use
  `obsidian history:list file=<note>` to see versions and
  `obsidian history:restore` to roll back.
* The daemon’s post-run audit still flags any file that changed but
  isn’t in the plan, even when the wrapper applied everything you
  expected.

This is the default because the safety case for it holds:

* Mutations are confined to `agent_root` (the wrapper rejects writes
  outside `KnowledgeBase/`).
* The agent has no shell tools that can bypass the wrapper (restricted
  PATH).
* Anything the LLM does inside `KnowledgeBase/` is versioned by
  Obsidian and recoverable.
* Your hand-written notes outside `KnowledgeBase/` are physically
  unreachable to the agent.

### Shadow mode

```
KB_AGENT_MODE=shadow
```

* Read commands still pass through.
* Write commands are intercepted: the wrapper appends a JSON record to
  `<work_dir>/.kb-plan.jsonl` describing what *would* have happened,
  then returns mock-success JSON so the agent’s control flow
  continues.
* The vault is **never mutated** in shadow mode.
* `kb show <id>` displays the full plan, including the agent’s final
  reasoning text.

Use this when you want to:

* validate a new prompt template;
* sanity-check an unfamiliar model on a sample of your inputs;
* debug why a particular run produced an unexpected result —
  re-run the same input in shadow mode to see the plan without
  re-mutating.

Switching modes is a one-line edit; the daemon does not need
restarting because the value is read per-job.

### How the modes interact with safety

Independent of the mode, **all three sandbox layers** apply:

1. The processor subprocess runs with `PATH` set to a curated wrapper
   directory.  Bare-name calls to `mkdir`, `cp`, `mv`, `rm`, `tee`,
   `git`, `curl`, etc. fail with `command not found`.
2. The kb-obsidian wrapper rejects writes whose path resolves outside
   `agent_root`, inside `sources_dir`, or outside `vault_root`.  It
   also rejects POSIX-style `--flag` arguments and a list of known
   destructive obsidian subcommands (`eval`, `plugin:*`,
   `history:restore`, …).
3. After the agent finishes, the daemon snapshots the vault before
   and after the run and reports any change that is not in the plan
   as a *rogue write*.  In shadow mode this should always be empty;
   if not, your skill regressed or the model is finding a clever
   bypass.  In apply mode, it tells you exactly which paths the agent
   touched.

## Daily usage

Once installed and running, the daemon is invisible.  The interactive
workflow looks like:

```bash
# 1. Drop a file into Sources/.
cp ~/Downloads/some_paper.pdf ~/Documents/Obsidian/Sources/

# 2. Watch the live event stream (optional).
kb tail

# 3. Once the event log reads `done`, inspect the result.
kb status                       # one-line summary of the queue
kb list --status done --limit 5 # the recent successes
kb show 42                      # full detail for a specific job
```

If something fails:

```bash
kb list --status failed         # find the row id
kb show 42                      # see the error and the agent log
kb requeue 42                   # try again
kb reset 42                     # forget about it entirely
```

See [CLI reference](#cli-reference) for the full surface.

## Pipeline internals

This section walks one source file through the system end-to-end.
If you only ever drop files in and read notes out, you can skip it —
but understanding the pipeline is essential for diagnosing failures
and for building custom processors.

### Stage 0 — detection (Rust, `kb-watcher`)

The watcher subscribes to FSEvents on `sources_dir` (recursive).  It
also runs a periodic full-directory scan every `poll_interval_secs`
seconds.  The scan is the **correctness backstop** — FSEvents miss
events during sleep, and iCloud / Dropbox can materialise files
late; the scan catches both.

For every candidate path:

1. **Filter by extension and ignore-glob.**
2. **Stability check** — poll `(size, mtime)` every 500 ms; require both
   to be unchanged for `stability_ms` (default 2 s) before proceeding.
   This avoids reading partially-written files.
3. **Hash** — stream-SHA-256 in `hash_chunk_bytes`-byte chunks.
4. **Dedup** against the `files` table:
   * already `done` with the same hash → silently skipped;
   * already `done` with a different hash → treated as a new revision;
   * different row, same hash, status `done` → marked `skipped`
     (deduplicated content);
   * in `queued` / `processing` → noop.
5. **Enqueue** — insert a row with status `queued`.

### Stage 1 — claim (Rust, `kb-worker`)

A bounded pool (`worker.concurrency` permits) repeatedly:

1. Acquires a permit.
2. Atomically claims the next `queued` row via
   `UPDATE files SET status='processing' … RETURNING …`.
3. Spawns a Tokio task that runs the processor subprocess.

Claiming is the only place where multiple workers race; the SQLite
UPDATE-RETURNING ensures exactly-once delivery.  Daemon restarts
between claim and completion are handled by the startup recovery
sweep, which resets every `processing` row back to `queued`.

### Stage 2 — spawn (Rust)

For each claimed job the worker:

* creates a per-job `work_dir` under `processor.work_dir_root`,
  named `<hash12>-<job_id>/`;
* serialises a `ProcessorInput` JSON to the child’s stdin;
* puts the child in its own process group (`setsid`), so it can be
  signalled with `killpg(SIGTERM)` then `killpg(SIGKILL)` on timeout
  or daemon shutdown;
* injects `extra_env` (loaded from `secrets.env`) plus
  `KB_AGENT_ROOT`, `KB_VAULT_ROOT`, `KB_SOURCES_DIR`, `KB_PLAN_FILE`,
  `KB_AGENT_MODE`;
* enforces `processor.timeout_secs` with the same SIGTERM→grace→SIGKILL
  escalation, and reacts to a daemon-wide `CancellationToken` so
  Ctrl-C never leaves orphan Python processes.

### Stage 3 — extract (Python, `kb_processor.extractors`)

Dispatch is by file extension.  Five extractors share a common
`docling.DocumentConverter` instance configured with MPS acceleration
on Apple Silicon:

* `pdf.py`  — PDF, **batched** (see below).
* `docx.py` — Word documents.
* `xlsx.py` — spreadsheets, sheet-by-sheet.
* `pptx.py` — presentations.
* `image.py` — PNG / JPEG, runs OCR.

#### PDF batching

Large PDFs (>50 pages) are processed in 5-page batches.  For each
batch:

1. **Sample text** with `pypdfium2` from every page in the batch.
2. Decide a **per-batch policy** by counting pages with substantial
   selectable text:
   * **`text_native`** (all sampled pages have ≥ 50 chars) →
     `do_ocr=False` — docling streams the embedded text directly,
     ~10 s/batch on M-series.
   * **`scanned`** (no pages have text) → `do_ocr=True`, full RT-DETR
     layout + RapidOCR pipeline, 30–100 s/batch.
   * **`mixed`** (some pages text, some not) → `do_ocr=True` (safer
     to over-extract than miss content).
3. Call `converter.convert(input_path, page_range=(start, end))` with a
   per-batch `document_timeout` (default 300 s).
4. Concatenate the batch markdown; save figures with globally unique
   filenames (`figure_0001.png`, …).
5. Stream a progress line to stdout (`[kb-processor] PDF batch N/M
   pages=A-B policy=text_native ok elapsed=Ts`); the daemon captures
   these into the audit log so `kb tail` shows real-time progress.

If a batch fails (timeout, decode error, hardware blip), the failure
is logged as a placeholder block in the output markdown and the rest
of the document continues.  Only when *every* batch fails does the
whole job fail.

Adaptive policy alone produced ~3× speed-up on a 208-page text-native
book (`The Visual Display of Quantitative Information`): 25 minutes
total, vs. an estimated >1 hour with OCR-on-everything.

#### MPS × transformers float64 caveat

The RT-DETR layout model (used by docling for region detection) is
defined in HuggingFace `transformers`.  Versions from 5.9.0 onwards
allocate a `torch.float64` tensor in their position-embedding code,
and Apple’s MPS backend does not support float64 — every page raises
`TypeError: Cannot convert a MPS Tensor to float64 dtype`.  Setting
`DOCLING_DEVICE=cpu` does **not** fix it (the layout model still
lands on MPS through docling internals).

The project pins `transformers>=5.8.1,<5.9.0` in
`processors/default/pyproject.toml` to sidestep this.  Tracking issue:
https://github.com/docling-project/docling/issues/3483

### Stage 4 — stage (Python)

The extracted markdown is written to `<work_dir>/extracted.md` with a
short frontmatter header (source basename, file type, extractor name,
job id, page count, figure count).  Figures are kept inside the work
dir; the agent decides whether to import any of them into the vault.

At this point the extractor is done.  The processor moves to the
integration step.

### Stage 5 — integrate (Python → pi RPC → LLM)

The processor calls `kb_processor.agent.rpc_driver.run_agent(...)`.
This function:

1. **Snapshots the vault** (one stat per file under `vault_root` minus
   `sources_dir` and `.obsidian/`).  Stored in memory as `{path:
   (mtime_ns, size)}`.
2. **Stages a per-job binary directory** at `<work_dir>/.agent-bin/`
   containing only:
   * `kb-obsidian` — a Python policy wrapper around `obsidian` (more
     below).
   * Curated read utilities: `cat`, `head`, `tail`, `sed`, `grep`,
     `awk`, `wc`, `printf`, `sort`, `uniq`, `tr`, `echo`, `sh`, `bash`,
     `env`, `true`, `false`, `basename`, `dirname`, `date`, `jq`.
   * Infrastructure binaries pi itself needs: `node`, `npm`, `npx`,
     `python3`.
   * No `mkdir`, `cp`, `mv`, `rm`, `tee`, `touch`, `chmod`, `git`,
     `curl`, `wget`, `ssh`, `pip`, etc.
3. **Builds the subprocess env**:
   * inherits the daemon’s env (which already merged `secrets.env`);
   * sets `PATH` to **only** the wrapper directory (no
     `/usr/bin:/bin:…` inheritance);
   * exports `KB_PLAN_FILE`, `KB_AGENT_MODE`, `KB_VAULT_ROOT`,
     `KB_SOURCES_DIR`, `KB_AGENT_ROOT`, `KB_EXTRACTED`;
   * strips other-provider keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
     etc.) so litellm cannot accidentally route to a different
     provider via its credential cascade.
4. **Spawns pi**:
   ```bash
   pi --mode rpc --no-session \
      --no-context-files --no-extensions --no-prompt-templates \
      --tools bash \
      --skill <package-skills-dir>/ \
      --provider openrouter --model anthropic/claude-3.5-haiku \
      --api-key $OPENROUTER_API_KEY
   ```
   The agent’s only built-in tool is `bash`.  Skills are described in
   the next section.
5. **Sends one prompt** — a `/skill:knowledge-builder-integrator`
   slash-command followed by the per-job context block.  Pi expands
   the slash-command into the SKILL.md content before forwarding to
   the LLM.
6. **Streams events** from pi’s stdout JSONL until `agent_end` or the
   `KB_AGENT_TIMEOUT_SECS` budget is exhausted.  Every event is
   captured to `<work_dir>/.agent-events.jsonl` for postmortem.
7. **Reaps pi** cleanly (`stdin.close()`, `wait(10s)`, escalating to
   `killpg(SIGTERM)` then `killpg(SIGKILL)`).
8. **Reads the plan** from `<work_dir>/.kb-plan.jsonl` (written by the
   wrapper, see below).
9. **Re-snapshots the vault** and computes the diff vs. the plan.
   Files that changed but are not in the plan are flagged as **rogue
   writes**.  In shadow mode this should always be empty; if it is
   not, the agent bypassed the wrapper via raw bash and the operator
   sees a loud warning.

### kb-obsidian — the policy boundary

The wrapper at
`processors/default/kb_processor/agent/wrappers/kb-obsidian` is a
Python script that mediates every vault operation.  Its rules:

* **Read commands** (`read`, `search`, `search:context`, `outline`,
  `tags`, `aliases`, `properties`, `backlinks`, `links`, `unresolved`,
  `orphans`, `deadends`, `files`, `folders`, …) pass straight through
  to `obsidian`.  These can target any path in the vault.
* **Write commands** (`create`, `append`, `prepend`, `move`, `rename`,
  `delete`, `property:set`, `property:remove`, …) are validated:
  * any token starting with `-` is rejected (POSIX-style flags are
    not Obsidian CLI syntax);
  * for any `path=`, `to=`, `file=` (with a `/`), or `name=` (with a
    `/`), the value is canonicalised against `vault_root` and must
    satisfy:
    ```
    p ⊆ vault_root  AND  p ⊄ sources_dir  AND  p ⊆ agent_root
    ```
    Bare wikilink `file=` values (no `/`) are passed through —
    Obsidian resolves them; we cannot pre-validate without running
    obsidian itself.
* **Blocked commands** are rejected unconditionally: `eval` (arbitrary
  JS in the running Obsidian app), `history:restore`, `plugin:*`,
  `publish:*`, `sync`, `reload`, `restart`, `devtools`,
  `dev:screenshot`.
* In **shadow mode** every accepted write is logged to
  `$KB_PLAN_FILE` as a JSON line and the wrapper returns mock-success
  to the agent without invoking obsidian.
* In **apply mode** every accepted write is logged AND passed through
  to obsidian; the entry’s `applied` field is set to the obsidian
  exit code.

### Skills

The agent gets a single instructional skill at
`processors/default/kb_processor/agent/skills/SKILL.md`.  Pi loads it
at startup; the user prompt activates it via
`/skill:knowledge-builder-integrator` so its content is expanded into
the LLM’s system context.

The skill teaches:

* the affirmative workflow — *read extracted, survey vault, search,
  decide, place, link, optionally set properties, summarise, stop*;
* the kb-obsidian command syntax — always `key=value`, never
  `--flag`, always `path=KnowledgeBase/…` for writes;
* the decision heuristics — CREATE / APPEND / RESTRUCTURE / MERGE /
  LINK_ONLY / SKIP, in order of precedence;
* the hard rules — use `kb-obsidian` for *all* mutations, paths must
  start with `KnowledgeBase/`, property name is `name=` (not `key=`),
  no `mkdir`/`cp`/`mv`/`rm`/`tee`, no shell redirects to vault paths,
  do not loop, stop after the summary.

The skill ends with five worked examples covering the common
decisions.

### Stage 6 — return (Python)

The processor returns a JSON `ProcessResult` to the daemon’s stdout:

```json
{
  "status":  "ok",
  "outputs": [],                          // empty in shadow mode
  "metadata": {
    "extractor":          "PdfExtractor",
    "model":              "openrouter/anthropic/claude-3.5-haiku",
    "agent_mode":         "shadow",
    "agent_turns":        10,
    "agent_elapsed_secs": 87.89,
    "agent_aborted":      false,
    "plan_file":          "/Users/.../jobs/<job>/.kb-plan.jsonl",
    "agent_log":          "/Users/.../jobs/<job>/.agent-events.jsonl",
    "plan_summary":       "plan(7 entries; applied=0): create=4, property_set=3",
    "plan_entry_count":   7,
    "rogue_writes_count": 0,
    "rogue_writes":       []
  }
}
```

In apply mode `outputs` is populated from successfully-applied plan
entries, and the daemon validates each path one more time against the
three-way invariant before recording it in the `outputs` table.

### Stage 7 — record (Rust)

The daemon parses the result, marks the row `done` (or `failed` with
the processor’s `retryable` flag), records every output, and writes a
final `done` / `failed` audit event.  The full plan summary is in
`processor_meta`, accessible via `kb show <id>` or the `/files/:id`
HTTP endpoint.

## Safety model

The failure mode this project is designed against is **the LLM
producing instructions that destructively mutate user-authored
content**.  A determined LLM acting in apparent good faith can still
be wrong; the policy must hold even when the model misbehaves.

### Three-layer sandbox (defence in depth)

```
Layer 1 — PATH restriction
  Subprocess sees only:
    kb-obsidian, cat, head, tail, sed, grep, awk, wc, printf,
    sort, uniq, tr, echo, sh, bash, env, true, false, basename,
    dirname, date, jq, node, npm, npx, python3
  Bare-name calls to mkdir, cp, mv, rm, tee, touch, chmod, git,
  curl, wget, ssh, pip ... fail with `command not found`.

Layer 2 — kb-obsidian wrapper
  For every WRITE command:
    • reject any token starting with `-` (POSIX-style flag).
    • reject path ⊄ vault_root.
    • reject path ⊆ sources_dir.
    • reject path ⊄ agent_root.
    • reject blocklisted subcommands (eval, plugin:*, history:restore, ...).
  Read commands pass through unchanged.

Layer 3 — post-run vault diff audit
  snapshot vault ▶ run agent ▶ snapshot vault
  rogue_writes = (after - before) - planned_paths
  rogue_writes != ∅  ⇒  loud error log,
                       processor_meta.rogue_writes_count,
                       displayed in `kb show`.
```

### What this catches

* The LLM calling `kb-obsidian create path=Sources/Foo.md` — wrapper
  rejects (path inside sources_dir).
* The LLM calling `kb-obsidian create path=Notes/Foo.md` — wrapper
  rejects (path outside agent_root).
* The LLM calling `mkdir /Users/me/.../KnowledgeBase/x && cp …` —
  PATH rejects `mkdir` and `cp`.
* The LLM calling `cat > /Users/me/.../foo.md` — PATH allows `cat` but
  the resulting unsanctioned file shows up in the post-run diff and
  is flagged.

### What this does NOT fully catch

* `python3 -c "open('/abs/path','w').write('...')"` — Python is on PATH
  because the wrapper itself is a Python script.  The audit catches
  the resulting filesystem change, but only after the fact.
* `node -e "require('fs').writeFileSync(...)"` — same story.
* True process-level isolation requires `sandbox-exec` (macOS) which
  is not yet wired in.  Tracking issue noted in the development
  notes.

### `agent_root` invariant

`paths.agent_root` (default `<vault_root>/KnowledgeBase`) is the
agent’s mutation sandbox.  Reads anywhere in the vault are fine and
actively encouraged — the agent surveys the whole vault when deciding
how to integrate new content.  Writes outside `agent_root` are
rejected at the wrapper layer; if a determined LLM bypasses the
wrapper, the audit catches it.

Obsidian’s File Recovery plugin keeps versioned history of every
edit by default.  When operating in apply mode this is your
second-to-last line of defence —
`obsidian history:restore file=<note> version=N` rolls a single note
back.  The actual last line is your existing backup story (Time
Machine, etc.); Knowledge Builder does not handle backups itself.

## CLI reference

All commands support `--help`.

```
kb <command> [options]
```

### `kb daemon`

Start the daemon.

```
kb daemon [--foreground]
```

With `--foreground`, logs are printed to stderr in addition to the
rotating JSON log file under `paths.log_dir`.  Without it the daemon
backgrounds and logs only to file (the default for `launchd` use).

Startup sequence:

1. Load and validate `config.toml`.
2. Initialise `tracing` (rotating JSON file + optional stderr).
3. Acquire singleton lock on `state.db.lock`.
4. Open SQLite, run any pending migrations.
5. Crash recovery: rows in `processing` are reset to `queued`.
6. Build the detection pipeline (FSEvents → stability → hasher → state).
7. Build the periodic scanner (initial pass runs immediately).
8. Load `secrets.env`, log the keys (never the values).
9. Start the worker pool.
10. Start the HTTP ops server on `ops.http_bind`.

The daemon parks on SIGINT / SIGTERM / SIGHUP.  SIGTERM and SIGINT
trigger the shutdown sequence; SIGHUP currently logs “config reload
not yet supported” and continues running.

### `kb install` / `kb uninstall`

Manage the launchd LaunchAgent at
`~/Library/LaunchAgents/com.user.knowledge-builder.plist`.

```
kb install [--force]
kb uninstall
```

`kb install` runs `kb doctor` first and refuses to install if any
check fails.  `--force` lets you reinstall over an existing plist
(used after upgrading the binary or changing the config path).

`kb uninstall` runs `launchctl bootout` and removes the plist.  It
does not touch the database, logs, caches, or the vault.

### `kb doctor`

Validate configuration and environment.  Exit 0 = all good, exit 1 =
one or more issues.  Reports:

* config file presence and parseability;
* the eight startup validation checks (`vault_root`, `sources_dir`,
  `agent_root`, processor command, db_path, log_dir, backoff,
  containment);
* SQLite integrity check;
* log directory writability;
* backup file age (warns if > 7 days old);
* secrets file mode (0600 enforced) and key list (values never printed).

### `kb config`

```
kb config show       # pretty-print resolved configuration as JSON
kb config path       # print the config.toml path; report whether it exists
kb config validate   # run all 8 validation checks; exit 1 on failure
```

### `kb status`

One-line summary of the queue:

```
kb status
```

Prints counts per status (`seen`, `queued`, `processing`, `done`,
`failed`, `skipped`), queue depth, oldest pending age, last error.

### `kb list`

List tracked files.

```
kb list [--status <status>] [--limit N] [--offset N]
```

When the daemon is running, talks to the HTTP ops endpoint; otherwise
falls back to direct DB read.

### `kb show`

Full detail for one row.

```
kb show <id|path>
```

Displays:

* the file metadata (path, hash, size, mtimes, status, attempts,
  next_attempt, last error);
* an **Agent plan** section when `processor_meta.plan_file` is
  populated, including:
  * mode badge (shadow / apply);
  * turn count + elapsed time;
  * plan summary;
  * plan file path;
  * **rogue-write warning** if the post-run audit flagged anything;
  * the agent’s final assistant message (truncated to 20 lines);
  * up to 20 plan entries with shadow / applied markers and
   abbreviated `key=value` args;
* the `outputs` table for that source;
* the last 10 audit events.

### `kb tail`

Follow the audit-event stream live (Server-Sent Events when the
daemon is running, polling fallback otherwise).

```
kb tail [--level <level>] [--kind <kind>]
```

### `kb scan`

Force an immediate full-directory scan of `sources_dir`.  Useful
after restoring files from backup or fixing a watcher hiccup.

```
kb scan
```

### `kb requeue`

Reset a row to `queued` (attempts = 0) so the worker pool picks it up
on the next claim.

```
kb requeue <id|path>
```

Use this after fixing a processor bug, after increasing
`processor.timeout_secs`, or after a transient API failure.

### `kb reset`

Delete a `files` row and its outputs entirely.  The next time the
file is discovered (or `kb scan` is run) it is treated as new.

```
kb reset <id|path>
```

Use this when you want the processor to start fresh (e.g. after
upgrading the agent skill).

### `kb prune`

Delete `done` rows older than a date.

```
kb prune [--before <YYYY-MM-DD>] [--status done] [--dry-run]
```

The `outputs` rows for those files are cascade-deleted; vault
content is **not** removed.  Use `--dry-run` first.

### `kb storage`

Report disk usage attributable to Knowledge Builder — outputs by
kind, sources by extension, work_dir cache size, plan file count.

```
kb storage
```

### `kb backup`

Create a `VACUUM INTO`-style snapshot of `state.db` under
`<state.db parent>/backups/state-YYYY-MM-DD.db`.  `kb doctor` warns
if the most recent backup is older than 7 days.

```
kb backup
```

Note: this only backs up the daemon’s state, not your vault.

## Processor contract

Knowledge Builder ships with one processor (`kb-processor`) but the
interface is language-agnostic and stable.  You can replace it with
any executable that honours this contract.

### Invocation

```
<processor.command> <input_path> <work_dir>
```

The daemon spawns the processor with stdin piped, stdout captured
line-by-line, and stderr captured to the audit log.  The child runs
in its own process group so the daemon can reliably kill it on
timeout or shutdown.

Environment includes everything from the daemon’s env, plus the
entries from `secrets.env`, plus:

| Variable | Meaning |
|---|---|
| `KB_PLAN_FILE` | Path the agent should append plan entries to (the kb-obsidian wrapper writes here). |
| `KB_AGENT_MODE` | `shadow` or `apply`. |
| `KB_VAULT_ROOT` | Absolute path. |
| `KB_SOURCES_DIR` | Absolute path. |
| `KB_AGENT_ROOT` | Absolute path — the agent’s mutation sandbox. |
| `KB_EXTRACTED` | Absolute path the agent should `cat` to read extracted content. |

### Input (stdin)

A single JSON object terminated by newline + EOF:

```json
{
  "input_path":   "/Users/me/Vault/Sources/foo.pdf",
  "content_hash": "sha256:9af1...",
  "vault_root":   "/Users/me/Vault",
  "sources_dir":  "/Users/me/Vault/Sources",
  "agent_root":   "/Users/me/Vault/KnowledgeBase",
  "work_dir":     "/Users/me/Library/Caches/knowledge-builder/jobs/9af1.../",
  "job_id":       12345,
  "attempt":      1
}
```

### Output (stdout)

The processor may print arbitrary log lines (the daemon stores them
in the audit log).  The **last non-empty line** must be a single JSON
object:

```json
{
  "status": "ok",
  "outputs": [
    {"path": "/Users/me/Vault/KnowledgeBase/Notes/Foo.md", "kind": "markdown", "bytes": 8421}
  ],
  "metadata": {
    "model": "gpt-4o-mini",
    "tokens_in": 12345,
    "tokens_out": 678
  }
}
```

Failure variant:

```json
{
  "status": "error",
  "error": "docling failed to convert PDF: ...",
  "retryable": true,
  "metadata": {"step": "extract"}
}
```

### Output path validation

For every entry in `outputs[]` the daemon verifies
`canonicalize(path)` is:

* under `vault_root`, AND
* not under `sources_dir`, AND
* under `agent_root`.

Violations are non-retryable failures: the processor produced an
output in the wrong place.  This protects against a buggy / rogue
processor regardless of the agent’s in-process sandboxing.

### Exit codes

* `0` — status was `ok`.
* non-zero — the processor crashed before emitting JSON, or emitted
  `status: error`.  When a parseable JSON line is present the daemon
  uses its `retryable` flag; otherwise it treats the failure as
  retryable by default.

## Troubleshooting

### `kb doctor` reports an error

Every error message names the failed key and provides an `Action:`
line.  Fix in order; the most common are:

* **`vault_root '…': does not exist`** — create the directory or fix
  the path in `config.toml`.
* **`agent_root '…' overlaps sources_dir`** — these two MUST be
  disjoint subtrees.  Move one of them.
* **`processor.command '…': file not found at '…'`** — the venv
  install never ran or was deleted.  Re-run the install steps.
* **`Secrets file '…' has permissive mode 644`** — `chmod 600` it.

### Daemon won’t start under launchd

Check the launchd log:

```bash
tail -50 ~/Library/Logs/knowledge-builder/stderr.log
launchctl list | grep knowledge-builder
```

If the agent is loaded but not running, run `kb daemon --foreground`
from your terminal and watch for the actual error.  Usually it’s a
config validation failure that `kb doctor` would have caught.

### A job is stuck in `processing`

A processor subprocess crashed without writing JSON, or the daemon
was force-killed.  Three options:

```bash
kb requeue 42      # try again with attempts reset
kb reset 42        # forget about it, next discovery treats it as new
```

The daemon’s startup recovery sweep auto-resets stuck rows on the
next start.

### Agent runs but plan is empty

This usually means the LLM hallucinated a workflow without issuing
any `kb-obsidian` commands.  Inspect the agent’s reasoning:

```bash
kb show 42                                       # plan summary in `kb show`
cat <work_dir>/.agent-events.jsonl | head -30    # full streaming log
```

Common causes: the model is too small for the task; the skill prompt
is being interpreted too defensively; you’re running an experimental
model with weak instruction-following.  Switch to a stronger model in
`secrets.env` (`KB_LLM_MODEL=openrouter/anthropic/claude-sonnet-4-5…`)
and `kb requeue` the row.

### Rogue writes flagged in `kb show`

The post-run audit found one or more files that changed but are not
in the plan.  This means the agent bypassed the kb-obsidian wrapper
— typically by using shell redirects or absolute paths to system
binaries.  Read the listed paths in `kb show`, decide whether to
keep / move / delete, and:

* tighten the skill prompt with stronger “do NOT use” examples;
* consider switching to a more compliant model;
* if the bypass is via `python3 -c …` or `node -e …`, this is the
  expected limitation noted in [Safety model](#safety-model);
  enabling `sandbox-exec` will close it.

### Large PDF takes hours

Check `kb show <id>`: `successful_batches` vs. `batch_count`.  If
batches are running >100 s each, your PDF is mostly scanned.  Either
wait it out or:

* increase `KB_AGENT_TIMEOUT_SECS` if it’s the agent step that’s
  timing out;
* increase `KB_PDF_BATCH_TIMEOUT_SECS` for slow OCR pages;
* drop the document and use a different source if OCR quality is too
  poor for the LLM to use anyway.

### MPS errors during extraction

```
TypeError: Cannot convert a MPS Tensor to float64 dtype
```

Your `transformers` is ≥ 5.9.0.  `pyproject.toml` pins
`<5.9.0`; reinstall the venv:

```bash
~/.local/share/kb/venv/bin/pip install -e "$(pwd)/processors/default[llm]"
```

### “Missing API key” errors

The daemon’s env did not contain `OPENROUTER_API_KEY` (or the
provider key your model needs).  Check:

```bash
kb doctor                       # secrets section
cat ~/.config/knowledge-builder/secrets.env
```

The most common cause is running `kb daemon --foreground` from a
shell that doesn’t source `secrets.env` and forgetting that the
daemon does.  Restarting with `kb install` (or just
`launchctl kickstart -k gui/$UID/com.user.knowledge-builder`) makes
the daemon re-read.

## Development

### Repository layout

```
knowledge_builder/
  Cargo.toml                 # workspace root
  crates/
    kb-core/                 # shared types, config, state store, paths,
                             #   secrets loader, migrations, lock
    kb-watcher/              # FSEvents watcher, stability, hasher,
                             #   periodic scanner, detection pipeline
    kb-worker/               # bounded worker pool, claim loop,
                             #   processor invocation, output validation
    kb-ops/                  # axum HTTP server, SSE event stream
    kb-cli/                  # `kb` binary; daemon entry point + ops cmds
  processors/
    default/                 # the reference Python processor
      pyproject.toml
      kb_processor/
        __main__.py          # JSON-on-stdin/stdout entry point
        models.py            # ProcessorInput / ProcessorResult pydantic
        pipeline.py          # extract → stage → integrate orchestrator
        extractors/          # pdf, docx, xlsx, pptx, image (docling)
        agent/
          rpc_driver.py      # spawns pi --mode rpc, drives the agent
          plan.py            # JSONL plan reader / writer
          indexer.py         # SQLite FTS5 vault index (legacy / unused)
          skills/SKILL.md    # the integration skill
          wrappers/kb-obsidian   # the policy wrapper
      tests/
        agent/               # plan, wrapper, audit, indexer tests
  installer/
    com.user.knowledge-builder.plist
  tests/integration/         # cross-crate integration tests
```

### Building, testing, running

```bash
# Rust
cargo build --release
cargo test  --release                # ~310 tests

# Python
cd processors/default
~/.local/share/kb/venv/bin/python3 tests/agent/test_plan_and_wrapper.py
~/.local/share/kb/venv/bin/python3 tests/agent/test_audit_and_path.py
~/.local/share/kb/venv/bin/python3 tests/agent/test_rpc_driver_e2e.py
```

The live RPC test is skipped automatically unless `pi` is on PATH and
both `OPENROUTER_API_KEY` and `KB_LLM_MODEL` are set in the
environment.  Costs ~$0.01 per run; uses a stub `obsidian` binary
so it does not contact your real vault.

### Debugging tips

* Run the daemon foreground with `RUST_LOG=debug` for verbose tracing.
* Each job’s working directory is preserved on failure.  Inspect
  `<work_dir>/extracted.md`, `<work_dir>/.kb-plan.jsonl`, and
  `<work_dir>/.agent-events.jsonl` directly.
* `kb show <id>` is the fastest way to see the full agent trace.
* `kb tail` streams audit events in real time.

### Adding a custom processor

Any executable that satisfies the [Processor contract](#processor-contract)
works.  Set `processor.command` in `config.toml` to its path (or
`bash -c ‘…’` style argv) and run `kb requeue <id>` to test.

### Adding a new extractor

1. Add a class under `processors/default/kb_processor/extractors/` that
   subclasses `BaseExtractor` and implements `can_handle(path)` and
   `extract(input_path, work_dir)`.
2. Register it in the `_EXTRACTORS` list in `pipeline.py`.
3. Add the new file extension to `watch.extensions` in `config.toml`.
4. Add unit tests under `processors/default/tests/`.

### Modifying the agent skill

The skill file at
`processors/default/kb_processor/agent/skills/SKILL.md` is the
primary lever for changing agent behaviour.  Edit it, then
`kb requeue <id>` an existing failed / shadow job to re-run with the
new prompt.  No daemon restart required — the skill is loaded into
pi at every job’s spawn.

### Adding to the kb-obsidian allowlist

If you find the agent legitimately needs an Obsidian subcommand that
is not in `READ_COMMANDS` or `WRITE_COMMANDS` of
`processors/default/kb_processor/agent/wrappers/kb-obsidian`, add it
to the appropriate set.  Be conservative: any command that mutates
files under control paths must respect the wrapper’s path-invariant
check (it already runs against any `path=`, `to=`, `file=`, or
`name=` argument with a `/` in it).

### Pi-extension alternative (not currently shipped)

The agent uses pi’s built-in `bash` tool with restricted PATH.  An
alternative architecture would be a custom pi extension that
registers dedicated tools (`vault_search`, `vault_create`, …) and
communicates with a long-lived Python helper over a UNIX socket.
That is a more complex protocol but gives strict tool-level isolation
that survives even rogue bash invocations.  Tracked as a future
improvement; not currently implemented.

## Acknowledgments

* [Obsidian](https://obsidian.md) and its CLI made the
  policy-mediated mutation surface possible.
* [docling](https://github.com/docling-project/docling) does the heavy
  lifting on PDF / DOCX / XLSX / PPTX extraction.
* [`pi-coding-agent`](https://www.npmjs.com/package/@earendil-works/pi-coding-agent)
  provides the agent loop and the JSON-RPC protocol the processor
  drives.
* [`litellm`](https://github.com/BerriAI/litellm) gives the agent
  one consistent interface across every provider you might use.

## License

MIT.  See `LICENSE`.

