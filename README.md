# Knowledge Builder

> A macOS background daemon that watches your Obsidian vault for new source files and automatically processes them into structured markdown notes.

---

## Table of Contents

1. [Overview](#overview)
2. [Quick Start](#quick-start)
3. [Installation](#installation)
4. [Configuration](#configuration)
5. [Usage — CLI Reference](#usage--cli-reference)
6. [Architecture](#architecture)
7. [Processor Contract](#processor-contract)
8. [Troubleshooting](#troubleshooting)
9. [Development](#development)

---

## Overview

**Knowledge Builder** (`kb`) is a macOS background service written in Rust. It monitors a designated folder inside your [Obsidian](https://obsidian.md) vault — the *sources directory* — for new files. When it detects one (PDF, DOCX, XLSX, PPT/PPTX, JPG, PNG), it automatically:

1. Waits for the file to finish writing (stability check)
2. Computes a SHA-256 content hash for deduplication
3. Enqueues the file in a local SQLite database
4. Invokes a pluggable *processor* (a Python subprocess by default) that performs OCR, image understanding, and LLM-based synthesis
5. Writes well-structured markdown notes and supporting assets back into your vault

The daemon runs as a `launchd` LaunchAgent so it starts automatically on login and restarts on crash. Every decision is recorded in a structured audit log you can query with the `kb` CLI.

**Who is it for?** Researchers, writers, and knowledge workers who accumulate PDFs, scanned documents, and images and want them automatically converted into searchable, linkable Obsidian notes without manual effort.

### Core guarantee

Processor outputs **must** live inside `vault_root` and **must not** land inside `sources_dir`. This structural invariant prevents the daemon from treating its own outputs as new inputs and entering an infinite reprocessing loop. It is enforced at startup (configuration validation) and at runtime (output path validation after every processor invocation).

---

## Quick Start

Five steps from zero to a running daemon:

### Step 1 — Install Rust and clone the repo

```bash
# Install Rust (skip if already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Clone
git clone https://github.com/your-org/knowledge-builder.git
cd knowledge-builder
```

### Step 2 — Build the release binary

```bash
cargo build --release
# Binary lands at: ./target/release/kb

# Optional: put it on your PATH
cp target/release/kb /usr/local/bin/kb
```

> **Note:** The first build compiles a bundled SQLite (via `rusqlite`). Expect 60–90 seconds on a cold cache.

### Step 3 — Create your config file

```bash
mkdir -p ~/.config/knowledge-builder
cat > ~/.config/knowledge-builder/config.toml << 'EOF'
[paths]
vault_root  = "~/Vault"
sources_dir = "~/Vault/Sources"

[processor]
command = "/path/to/knowledge-builder/processors/default/run.sh"
EOF
```

Also create the sources directory if it doesn't exist:

```bash
mkdir -p ~/Vault/Sources
```

### Step 4 — Validate your setup

```bash
kb doctor
```

All checks should show `✓`. Fix any reported errors before proceeding (the output is actionable — each failure includes an `Action:` line).

Example passing output:

```
kb doctor — running pre-flight checks

  ✓ Config file parsed successfully.
  ✓ All 8 configuration checks passed.
  ✓ SQLite integrity check passed (ok).
  ✓ Log directory is writable.

All checks passed. Ready to install.
```

### Step 5 — Install and start the daemon

```bash
kb install
```

This registers Knowledge Builder as a `launchd` LaunchAgent. The daemon starts immediately and will restart automatically on login.

Verify it's running:

```bash
kb status
```

Drop a PDF into `~/Vault/Sources/` and watch it flow through:

```bash
kb tail
```

---

## Installation

### From source (recommended)

```bash
git clone https://github.com/your-org/knowledge-builder.git
cd knowledge-builder
cargo build --release
```

**Requirements:**
- Rust 1.75+ (stable)
- macOS 12 Monterey or later (uses FSEvents and `launchd`)
- Python 3.10+ (for the default processor)
- An Obsidian vault on local disk

**Python processor dependencies** (for the default processor):

```bash
cd processors/default
pip install -e .
# or: pip install pymupdf python-docx openpyxl python-pptx openai
```

Set your LLM API key (the default processor uses OpenAI):

```bash
export OPENAI_API_KEY="sk-..."
# Add to ~/.zshrc or ~/.bash_profile to persist across sessions
```

### Binary release

Download the latest release archive from the [Releases page](https://github.com/your-org/knowledge-builder/releases), extract, and move the `kb` binary to a directory on your `$PATH`:

```bash
tar xzf knowledge-builder-macos-aarch64.tar.gz
mv kb /usr/local/bin/
```

---

## Configuration

The config file lives at `~/.config/knowledge-builder/config.toml`. All path values support `~` expansion. Missing keys fall back to built-in defaults.

**Environment variable overrides** use the prefix `KB__` with `__` as the nesting separator:

```bash
KB__PATHS__VAULT_ROOT=/my/vault  # overrides [paths].vault_root
KB__WORKER__CONCURRENCY=4        # overrides [worker].concurrency
```

### Full annotated `config.toml`

```toml
# ── Filesystem paths ──────────────────────────────────────────────────────────
[paths]

# Root of your Obsidian vault.
# Every processor output MUST live inside this directory.
# Default: ~/Vault
vault_root = "~/Vault"

# Subdirectory of vault_root where you drop source files.
# Must be a strict subdirectory (not the vault root itself).
# Default: ~/Vault/Sources
sources_dir = "~/Vault/Sources"

# SQLite database file used for all state, queue, and audit data.
# Default: ~/Library/Application Support/knowledge-builder/state.db
db_path = "~/Library/Application Support/knowledge-builder/state.db"

# Directory for rotating daily log files (kb.log.YYYY-MM-DD).
# Default: ~/Library/Logs/knowledge-builder
log_dir = "~/Library/Logs/knowledge-builder"


# ── File watching ─────────────────────────────────────────────────────────────
[watch]

# File extensions to admit for processing (lowercase, no leading dot).
# Files with any other extension are silently ignored.
# Default: pdf, docx, xlsx, ppt, pptx, jpg, jpeg, png
extensions = ["pdf", "docx", "xlsx", "ppt", "pptx", "jpg", "jpeg", "png"]

# Glob patterns that suppress an otherwise-admitted file.
# Matched against the full absolute path.
# Default patterns suppress dotfiles, Office lock-files, and iCloud placeholders.
ignore_globs = [
    "**/.*",          # dotfiles and hidden directories
    "**/~$*",         # Office lock files (~$document.docx)
    "**/.obsidian/**", # Obsidian internal files
    "**/*.icloud",    # iCloud placeholder stubs
]

# Milliseconds that a file's size + mtime must remain unchanged before it is
# considered stable and safe to hash/enqueue. Prevents reading partial writes.
# Default: 2000 (2 seconds)
stability_ms = 2000

# How often (seconds) the daemon performs a full directory scan as a backstop
# for files missed by the FSEvents watcher (e.g. during sleep or cloud sync).
# Default: 300 (5 minutes)
poll_interval_secs = 300

# Read-chunk size for streaming SHA-256 hashing (bytes).
# Larger values use more memory but reduce syscall overhead for big files.
# Default: 1048576 (1 MiB)
hash_chunk_bytes = 1048576


# ── Worker pool ───────────────────────────────────────────────────────────────
[worker]

# Maximum number of processor subprocesses running simultaneously.
# Reduce to 1 if you experience high memory usage.
# Default: 2
concurrency = 2

# Maximum number of processing attempts before a file is permanently marked
# as 'failed'. Must be >= 1.
# Default: 3
max_attempts = 3

# Per-retry backoff delays in seconds.
# Must have at least (max_attempts - 1) entries.
# Entry [0] is used before attempt 2, entry [1] before attempt 3, etc.
# Default: [30, 300, 1800] (30s, 5m, 30m)
backoff_secs = [30, 300, 1800]


# ── Processor subprocess ──────────────────────────────────────────────────────
[processor]

# Path to the processor script or executable.
# Can be absolute, relative to the current working directory, or a bare
# name resolvable via $PATH.
# Default: processors/default/run.sh
command = "processors/default/run.sh"

# Hard wall-clock timeout per processor invocation in seconds.
# The daemon sends SIGTERM then SIGKILL if the processor exceeds this limit.
# Default: 1800 (30 minutes)
timeout_secs = 1800

# Root directory under which per-job working directories are created.
# Each job gets its own subdirectory: <work_dir_root>/<hash12>-<job_id>/
# Cleaned up automatically on successful processing.
# Default: ~/Library/Caches/knowledge-builder/jobs
work_dir_root = "~/Library/Caches/knowledge-builder/jobs"


# ── HTTP ops server ───────────────────────────────────────────────────────────
[ops]

# TCP bind address for the local HTTP API.
# MUST be loopback only (127.0.0.1) — no authentication is implemented.
# Default: 127.0.0.1:7878
http_bind = "127.0.0.1:7878"

# Minimum log level. One of: trace, debug, info, warn, error.
# Override per-module with RUST_LOG (e.g. RUST_LOG=kb_worker=debug,info).
# Default: info
log_level = "info"

# Log format for the rotating file log. "json" is recommended for parsing
# with jq or log aggregators. "pretty" is easier to read in a terminal.
# Default: json
log_format = "json"
```

### Validation rules

`kb doctor` (and `kb install`) run eight validation checks at startup:

| # | Check |
|---|-------|
| 1 | `vault_root` exists, is a directory, and is readable + writable |
| 2 | `sources_dir` exists, is a directory, and is readable |
| 3 | `sources_dir` is a subdirectory of `vault_root` (after canonicalization) |
| 4 | `sources_dir` is not the same path as `vault_root` |
| 5 | `processor.command` exists and has execute permission |
| 6 | `db_path` parent directory is writable; SQLite can be opened |
| 7 | `log_dir` can be created/written |
| 8 | `backoff_secs` has at least `max_attempts − 1` entries |

---

## Usage — CLI Reference

All subcommands support `--help` for inline documentation.

```
kb [COMMAND] [OPTIONS]
```

### `kb daemon`

Start the daemon in the foreground (useful for debugging; `kb install` manages it via `launchd` for normal use).

```
kb daemon [--foreground]
```

| Flag | Description |
|------|-------------|
| `--foreground` | Print logs to stderr in addition to the log file. Implied when a TTY is attached. |

**What it does on startup:**

1. Acquires a singleton lock (`state.db.lock`) — exits if another instance is already running
2. Runs crash recovery: any files stuck in `processing` are reset to `queued`
3. Performs an initial full-directory scan to catch files added during downtime
4. Starts the FSEvents file watcher
5. Starts the worker pool
6. Starts the HTTP ops server on `127.0.0.1:7878`

**Example:**

```bash
# Debug a problem — see all log output in your terminal
RUST_LOG=debug kb daemon --foreground
```

**When to use:** During development or troubleshooting. For production use, let `launchd` manage the daemon via `kb install`.

---

### `kb install`

Register the daemon as a macOS `launchd` LaunchAgent.

```
kb install [--force]
```

| Flag | Description |
|------|-------------|
| `--force` | Overwrite an existing plist and re-register the service. Use after upgrading the binary or changing the config path. |

**What it does:**

1. Runs `kb doctor` — aborts if any check fails
2. Writes a rendered plist to `~/Library/LaunchAgents/com.user.knowledge-builder.plist`
3. Runs `launchctl bootstrap gui/<UID> <plist>`
4. Runs `launchctl enable` and `launchctl kickstart` to start the daemon immediately

**Example:**

```bash
kb install
# Running pre-flight checks (kb doctor)…
#   ✓ Config file parsed successfully.
#   ✓ All 8 configuration checks passed.
#   ✓ SQLite integrity check passed (ok).
#   ✓ Log directory is writable.
#
# ✓ Installed: ~/Library/LaunchAgents/com.user.knowledge-builder.plist
# ✓ Service started: com.user.knowledge-builder
#
# Next steps:
#   kb status    — check daemon is running
#   kb tail      — stream live events
```

**When to use:** Once after initial setup, and again with `--force` after upgrading `kb`.

---

### `kb uninstall`

Stop and remove the `launchd` LaunchAgent.

```
kb uninstall
```

**What it does:**

1. Runs `launchctl bootout gui/<UID> <plist>` to stop and deregister the daemon
2. Deletes `~/Library/LaunchAgents/com.user.knowledge-builder.plist`

The SQLite database, log files, and vault contents are **not** removed. To fully clean up:

```bash
kb uninstall
rm -rf "~/Library/Application Support/knowledge-builder"
rm -rf ~/Library/Logs/knowledge-builder
rm -rf ~/Library/Caches/knowledge-builder
```

**When to use:** When you want to stop Knowledge Builder from running at login or need to reconfigure from scratch.

---

### `kb doctor`

Validate configuration and environment prerequisites.

```
kb doctor
```

No flags. Exits 0 when all checks pass; exits 1 with a diagnostic listing otherwise.

**Example output (all passing):**

```
kb doctor — running pre-flight checks

  ✓ Config file parsed successfully.
  ✓ All 8 configuration checks passed.
  ✓ SQLite integrity check passed (ok).
  ✓ Log directory is writable.

All checks passed. Ready to install.
```

**Example output (failures):**

```
kb doctor — running pre-flight checks

  ✓ Config file parsed successfully.
  ✗ sources_dir '/Users/alice/Vault/Sources': directory does not exist.
    Action: create the directory and ensure the current user has read access.
  ✗ processor.command 'processors/default/run.sh': file not found.
    Action: verify the script path is correct and that it has execute permission.

  → 2 configuration error(s). Run `kb config validate` for full detail.
```

**When to use:** After editing `config.toml`, before `kb install`, or when diagnosing why the daemon won't start.

---

### `kb status`

Show a summary of the current processing queue.

```
kb status
```

No flags. Works without the daemon running (reads DB directly).

**Example output:**

```
Knowledge Builder — Queue Status
─────────────────────────────────────────
  seen        0
  queued      3
  processing  1  ●
  done       47
  failed      2  ◀
  skipped     5
─────────────────────────────────────────
  Total      58

Queue depth:        4
Oldest pending:     2m 15s
Last error:         Processor timeout after 1800s (report.docx)
```

**When to use:** Quick health check. The `◀` marker on `failed` and `●` on `processing` flag rows that need attention.

---

### `kb list`

List tracked files with filtering and pagination.

```
kb list [--status <STATUS>] [--limit <N>]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-s, --status` | *(all)* | Filter by status: `seen`, `queued`, `processing`, `done`, `failed`, `skipped` |
| `-n, --limit` | `20` | Maximum number of rows to return |

**Example:**

```bash
# Show all failed files
kb list --status failed

# ID  Status    Path                                   Hash           Updated
# ─────────────────────────────────────────────────────────────────────────────
# 14  failed ◀  Sources/report.docx                   sha256:a3f9…  2024-01-15 14:33
# 27  failed ◀  Sources/scan-2024-01.pdf               sha256:7bc2…  2024-01-15 09:12
#
# Showing 2 rows.

# Show the 5 most recent done files
kb list --status done --limit 5
```

**When to use:** Finding files to requeue after processor fixes, or auditing what's been processed.

---

### `kb show`

Show detailed information about a single file.

```
kb show <ID | PATH>
```

Accepts either a numeric row ID (from `kb list`) or a file path (absolute, relative, or `~`-prefixed).

**Example:**

```bash
kb show 42
# ┌─ paper.pdf [id: 42] ──────────────────────────────────────────────────────┐
#
# File details:
#   Path         /Users/alice/Vault/Sources/paper.pdf
#   Status       done
#   Content hash sha256:3a7f9e2b1c4d…
#   Size         2.3 MB
#   Attempts     1 / 3
#   First seen   2024-01-15 14:30:01
#   Updated      2024-01-15 14:32:45
#   Processed    2024-01-15 14:32:45
#
# Outputs (2):
#   1. /Users/alice/Vault/Notes/paper.md          markdown   14.2 KB
#   2. /Users/alice/Vault/Assets/paper-fig1.png   asset      82.1 KB
#
# Recent events:
#   2024-01-15 14:30:01  INFO  discovered     paper.pdf - File detected by watcher
#   2024-01-15 14:30:03  INFO  queued         paper.pdf - Enqueued for processing
#   2024-01-15 14:30:03  INFO  processor_started  paper.pdf - Attempt 1
#   2024-01-15 14:32:45  INFO  done           paper.pdf - Processing complete (2 outputs)

kb show ~/Vault/Sources/paper.pdf   # same result, by path
```

**When to use:** Diagnosing a specific file — see its full history, outputs, and any error messages.

---

### `kb requeue`

Reset a file back to `queued` so it will be processed again.

```
kb requeue <ID | PATH>
```

Resets `status → queued`, clears `attempts` and `last_error`. Works offline (no daemon required). The daemon will pick up the requeued file on its next claim loop iteration (within 100 ms if running).

**Example:**

```bash
kb requeue 14
# Requeued: /Users/alice/Vault/Sources/report.docx (was: failed)

kb requeue ~/Vault/Sources/report.docx
# Requeued: /Users/alice/Vault/Sources/report.docx (was: failed)

# If already queued:
# Note: /Users/alice/Vault/Sources/report.docx is already queued.
# Requeued: /Users/alice/Vault/Sources/report.docx (was: queued)
```

**When to use:** After fixing a processor bug, after increasing `timeout_secs`, or after a transient API failure. Use `kb list --status failed` to find all files that need requeuing.

---

### `kb reset`

Delete a file's database record entirely so it will be re-discovered and re-processed from scratch.

```
kb reset <ID | PATH>
```

**What it removes:**
- The `files` row (cascade-deletes all `outputs` rows)
- Historical audit events retain `file_id = NULL` (not deleted)

**What it does NOT remove:**
- Physical output files on disk
- The source file itself

After a reset, the next watcher event or periodic scan will re-discover the file and re-enqueue it.

**Example:**

```bash
kb reset 42
# Reset: /Users/alice/Vault/Sources/paper.pdf - row and 2 outputs removed.
# File will be re-discovered on next scan.
```

**When to use:** When `kb requeue` isn't enough — for example, if the content hash is stale, if you want to force re-detection of a renamed file, or after manual vault restructuring.

---

### `kb tail`

Stream live audit events to the terminal.

```
kb tail [--level <LEVEL>] [--kind <KIND>]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--level` | `info` | Minimum severity: `info`, `warn`, `error` |
| `--kind` | *(all)* | Filter by event kind (e.g. `queued`, `done`, `failed`) |

Polls the `events` table every 500 ms. Shows the last 20 events as backfill on startup, then streams new events as they arrive. Press Ctrl-C to stop.

**Example output:**

```
[2024-01-15 14:30:01] INFO  discovered       paper.pdf - File detected by watcher
[2024-01-15 14:30:01] INFO  queued           paper.pdf - Enqueued for processing
[2024-01-15 14:30:03] INFO  processor_started  paper.pdf - Attempt 1, hash sha256:3a7f…
[2024-01-15 14:32:45] INFO  done             paper.pdf - Processing complete (2 outputs)
[2024-01-15 14:33:01] WARN  failed           report.docx - Processor timeout after 1800s
```

Color coding: `INFO` → green, `WARN` → yellow, `ERROR` → red.

**Example:**

```bash
# Watch only failures
kb tail --level warn

# Watch only completion events
kb tail --kind done
```

**When to use:** Real-time monitoring while dropping files into the vault, or watching a processor fix propagate through the queue.

---

### `kb scan`

Trigger an immediate full-directory scan without waiting for the next scheduled poll.

```
kb scan
```

No flags. Useful after the daemon was offline and files accumulated, or after manually adding many files at once.

**When to use:** After bringing the daemon back online following maintenance, or if you suspect the watcher missed some events during a cloud sync.

---

### `kb prune`

Delete old or terminal file records from the database to reclaim storage.

```
kb prune [--before <DATE>] [--status <STATUS>] [--dry-run]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--before` | *(none — required)* | ISO date (`2024-01-01`) or relative (`30d`, `7d`) |
| `--status` | `done` | Status to prune: `done`, `failed`, `skipped` |
| `--dry-run` | false | Preview what would be deleted without making changes |

Does **not** delete physical files from disk — only database records.

**Example:**

```bash
# Preview what would be pruned
kb prune --before 30d --status done --dry-run
# Would delete 23 rows (done, before 2023-12-15)

# Actually prune
kb prune --before 30d --status done
# Deleted 23 rows.

# Clean up all failed records older than 7 days
kb prune --before 7d --status failed
```

**When to use:** Routine maintenance to keep the database lean. Schedule monthly via `cron` or run manually.

---

### `kb storage`

Show disk usage grouped by file type.

```
kb storage
```

No flags.

**Example output:**

```
Sources:
  pdf:      45 files,  234.0 MB
  docx:     12 files,   56.0 MB
  png:       8 files,   12.3 MB

Outputs:
  markdown: 65 files,    3.1 MB
  asset:   120 files,  450.0 MB

Total: 65 sources, 185 outputs, 755.4 MB tracked
```

**When to use:** Before pruning, to understand where disk space is being used.

---

### `kb config show`

Print the fully-resolved configuration (after `~` expansion, before validation).

```
kb config show
```

**Example:**

```bash
kb config show
# [paths]
# vault_root = "/Users/alice/Vault"
# sources_dir = "/Users/alice/Vault/Sources"
# db_path = "/Users/alice/Library/Application Support/knowledge-builder/state.db"
# log_dir = "/Users/alice/Library/Logs/knowledge-builder"
# ...
```

**When to use:** Confirming that environment variable overrides or `~` expansion resolved as expected.

---

### `kb config validate`

Run the 8-point validation and report results without starting the daemon.

```
kb config validate
```

Equivalent to `kb doctor` but exits 0 even on failure (for scripting). Prints all validation errors.

---

### `kb config path`

Print the path to the active config file.

```
kb config path
# /Users/alice/.config/knowledge-builder/config.toml
```

**When to use:** Scripting — pipe to `$EDITOR $(kb config path)`.

---

### `kb backup`

Back up the SQLite database to a file.

```
kb backup [--output <PATH>]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--output` | `./kb-backup-<timestamp>.db` | Destination path for the backup file |

Uses SQLite's `VACUUM INTO` for a consistent, hot backup (safe to run while the daemon is processing).

**Example:**

```bash
kb backup --output ~/Desktop/kb-backup-2024-01-15.db
# Backed up to: /Users/alice/Desktop/kb-backup-2024-01-15.db (1.2 MB)
```

**When to use:** Before major changes to the processor or vault restructuring. Schedule weekly via `cron`.

---

### `kb restore`

Restore the SQLite database from a backup file.

```
kb restore <BACKUP_PATH>
```

**Important:** The daemon must not be running when restoring. Run `kb uninstall` first or stop it with `launchctl stop com.user.knowledge-builder`.

**Example:**

```bash
launchctl stop com.user.knowledge-builder
kb restore ~/Desktop/kb-backup-2024-01-15.db
launchctl start com.user.knowledge-builder
```

---

## Architecture

### High-level diagram

```
┌─────────────────────────────────────────────────────────────┐
│                        kb daemon                            │
│                                                             │
│  ┌─────────────┐    ┌──────────────┐    ┌───────────────┐  │
│  │  FSEvents   │    │  Stability   │    │  SHA-256      │  │
│  │  Watcher    │───▶│  Tracker     │───▶│  Hasher       │  │
│  │ (notify)    │    │ (500ms poll) │    │ (1MB chunks)  │  │
│  └─────────────┘    └──────────────┘    └──────┬────────┘  │
│                                                │            │
│  ┌─────────────┐                              │            │
│  │  Periodic   │                              ▼            │
│  │  Scanner    │───────────────────▶ ┌─────────────────┐  │
│  │ (5 min)     │                     │   Dedup Logic   │  │
│  └─────────────┘                     │  (5 rules, SQL) │  │
│                                      └────────┬────────┘  │
│                                               │            │
│                                               ▼            │
│                                      ┌─────────────────┐  │
│                                      │  SQLite Actor   │  │
│                                      │  (single writer)│  │
│                                      └────────┬────────┘  │
│                                               │            │
│  ┌──────────────────────────────────────────┐│            │
│  │             Worker Pool                  ││            │
│  │  ┌─────────┐ ┌─────────┐                ││            │
│  │  │ Worker  │ │ Worker  │ … (concurrency)◀┘│            │
│  │  │  slot   │ │  slot   │                  │            │
│  │  └────┬────┘ └────┬────┘                  │            │
│  └───────┼───────────┼───────────────────────┘            │
│          │           │                                     │
│          ▼           ▼                                     │
│  ┌───────────────────────────────────────────┐            │
│  │           Processor Subprocess            │            │
│  │  JSON stdin ──▶ Python ──▶ JSON stdout    │            │
│  │  (timeout: 1800s, process group kill)     │            │
│  └───────────────────┬───────────────────────┘            │
│                      │                                     │
│                      ▼                                     │
│  ┌───────────────────────────────────────────┐            │
│  │         Output Path Validation            │            │
│  │  canonicalize ⊂ vault_root ∧ ⊄ sources  │            │
│  └───────────────────┬───────────────────────┘            │
│                      │                                     │
│                      ▼                                     │
│  ┌───────────────────────────────────────────┐            │
│  │  Record outputs + mark done + audit event │            │
│  └───────────────────────────────────────────┘            │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │  HTTP API (axum, 127.0.0.1:7878)                    │  │
│  │  GET /healthz  GET /stats  GET /files  GET /events  │  │
│  │  POST /files/:id/requeue  POST /scan  GET /tail SSE │  │
│  └─────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### Crate descriptions

| Crate | Role |
|-------|------|
| **`kb-core`** | Foundation: `Config`, `StateStore` (SQLite actor), `FileRow`/`Status` types, path invariant utilities, migrations, tracing setup, singleton lock |
| **`kb-watcher`** | Detection: FSEvents watcher, stability tracker, SHA-256 hasher, periodic scanner, detection pipeline (wires all stages together) |
| **`kb-worker`** | Execution: worker pool (tokio semaphore + claim loop), subprocess invocation, JSON result parser, output validator |
| **`kb-ops`** | Observability: axum HTTP server, all REST endpoints, SSE tail stream |
| **`kb-cli`** | Interface: `clap`-based `kb` binary, all subcommand implementations |

### Data flow detail

1. **Detection:** A file drop triggers an FSEvents notification → stability window (2 s of unchanged size + mtime) → SHA-256 hash → 5-rule dedup decision → SQLite `files` row with `status = 'queued'`

2. **Execution:** Worker pool claims `queued` row atomically (single `UPDATE … RETURNING`) → constructs `ProcessorInput` JSON → spawns processor subprocess → 30-minute timeout with SIGTERM→SIGKILL fallback → parses last stdout line as `ProcessResult` JSON → validates all output paths → records `outputs` rows → marks `status = 'done'`

3. **Resilience:** On daemon restart, `status = 'processing'` rows are immediately reset to `'queued'` (crash recovery). The periodic scanner re-discovers files missed during sleep or iCloud sync. Failed jobs are retried with exponential backoff (30s, 5m, 30m by default).

### SQLite schema (3 tables)

```sql
-- Source file tracking
files(
    id, path, content_hash, size, mtime_ns, inode,
    status CHECK(status IN ('seen','queued','processing','done','failed','skipped')),
    attempts, next_attempt_at, last_error,
    first_seen_at, updated_at, processed_at, processor_meta
)

-- Produced artifacts
outputs(
    id, source_id REFERENCES files(id) ON DELETE CASCADE,
    path, kind, bytes, created_at
)

-- Audit log
events(
    id, ts, level, kind,
    file_id REFERENCES files(id) ON DELETE SET NULL,
    message, detail
)
```

---

## Processor Contract

The default processor (`processors/default/run.sh` → Python) handles PDF/DOCX/XLSX/PPTX/image files. You can replace it with any executable that honours this contract.

### Input (JSON on stdin)

```json
{
    "input_path":   "/Users/alice/Vault/Sources/paper.pdf",
    "content_hash": "sha256:3a7f9e2b1c4d5e6f...",
    "vault_root":   "/Users/alice/Vault",
    "sources_dir":  "/Users/alice/Vault/Sources",
    "work_dir":     "/Users/alice/Library/Caches/knowledge-builder/jobs/3a7f9e-42/",
    "job_id":       42,
    "attempt":      1
}
```

| Field | Type | Description |
|-------|------|-------------|
| `input_path` | string (absolute path) | The file to process |
| `content_hash` | string (`sha256:<hex>`) | SHA-256 of the input file |
| `vault_root` | string (absolute path) | Vault root — all outputs must be inside this |
| `sources_dir` | string (absolute path) | Sources directory — outputs must NOT be inside this |
| `work_dir` | string (absolute path) | Scratch space for this job; created before invocation |
| `job_id` | integer | Unique database row ID |
| `attempt` | integer (1-based) | Which attempt this is |

### Output (last line of stdout must be JSON)

**On success:**

```json
{
    "status": "ok",
    "outputs": [
        {
            "path":  "/Users/alice/Vault/Notes/paper.md",
            "kind":  "markdown",
            "bytes": 14567
        },
        {
            "path":  "/Users/alice/Vault/Assets/paper-fig1.png",
            "kind":  "asset",
            "bytes": 84012
        }
    ],
    "metadata": {
        "title": "Attention Is All You Need",
        "pages": 15,
        "model": "gpt-4o"
    }
}
```

**On error:**

```json
{
    "status":    "error",
    "error":     "OpenAI API rate limit exceeded",
    "retryable": true
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `status` | `"ok"` or `"error"` | ✅ | Outcome |
| `outputs` | array | if `status="ok"` | List of produced files |
| `outputs[].path` | string (absolute path) | ✅ | Must be inside `vault_root`, outside `sources_dir` |
| `outputs[].kind` | string | ✅ | Semantic label: `"markdown"`, `"asset"`, `"csv"`, etc. |
| `outputs[].bytes` | integer | ✅ | File size in bytes |
| `error` | string | if `status="error"` | Human-readable error description |
| `retryable` | boolean | if `status="error"` | Whether the daemon should retry (defaults to `true`) |
| `metadata` | object | ❌ | Arbitrary key-value data stored in `files.processor_meta` |

**Exit code rules:**

- Exit `0` + valid `ok` JSON → success
- Exit `0` + valid `error` JSON → treated as a controlled failure
- Non-zero exit + valid `error` JSON → failure, `retryable` flag honoured
- Non-zero exit + no valid JSON → retryable failure (contract violation)
- Stdout empty or no JSON on last line → retryable failure

**Output file rules:**

1. All output paths must be absolute and inside `vault_root`
2. No output path may be inside `sources_dir` (prevents reprocessing loops)
3. Write outputs atomically: write to a temp file then use `os.replace()` — never write directly to the final path (protects against partial writes)
4. The daemon validates paths after your processor exits; violations are non-retryable failures

### Writing a custom processor

**Step 1: Create a script**

Your processor can be any executable. It receives the JSON payload on stdin and must print a result JSON object as the last line of stdout.

**Step 2: Make it executable**

```bash
chmod +x /path/to/my-processor.sh
```

**Step 3: Point the config at it**

```toml
[processor]
command = "/path/to/my-processor.sh"
```

**Step 4: Run `kb doctor`** to verify the command is found and executable.

---

### Example: Minimal Python processor

```python
#!/usr/bin/env python3
"""
Minimal Knowledge Builder processor.
Reads a file, creates a markdown note with basic metadata.
"""
import json
import sys
import os
from pathlib import Path
from datetime import datetime

def main():
    # Read the job input from stdin
    payload = json.load(sys.stdin)

    input_path  = Path(payload["input_path"])
    vault_root  = Path(payload["vault_root"])
    sources_dir = Path(payload["sources_dir"])
    work_dir    = Path(payload["work_dir"])

    # Determine output path — inside vault, outside sources
    notes_dir = vault_root / "Notes" / "Processed"
    notes_dir.mkdir(parents=True, exist_ok=True)
    out_path = notes_dir / (input_path.stem + ".md")

    # Build the note content
    content = f"""---
source: {input_path}
processed: {datetime.now().isoformat()}
size_bytes: {input_path.stat().st_size}
---

# {input_path.stem}

*Processed from `{input_path.name}`*

> Add your extraction logic here.
"""

    # Atomic write: temp file → os.replace
    tmp = work_dir / "output.tmp"
    tmp.write_text(content, encoding="utf-8")
    os.replace(tmp, out_path)

    # Print result JSON as the LAST line of stdout
    result = {
        "status": "ok",
        "outputs": [
            {
                "path":  str(out_path),
                "kind":  "markdown",
                "bytes": out_path.stat().st_size,
            }
        ],
        "metadata": {"source": str(input_path)},
    }
    print(json.dumps(result))
    sys.exit(0)

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        error = {"status": "error", "error": str(e), "retryable": True}
        print(json.dumps(error))
        sys.exit(1)
```

---

### Example: Minimal shell processor

```bash
#!/bin/bash
# Minimal shell processor for Knowledge Builder
# Usage: processor.sh <input_path> <work_dir>
# JSON payload is on stdin

set -euo pipefail

# Read the JSON payload from stdin (requires jq)
PAYLOAD=$(cat)
INPUT_PATH=$(echo "$PAYLOAD" | jq -r '.input_path')
VAULT_ROOT=$(echo "$PAYLOAD" | jq -r '.vault_root')
WORK_DIR=$(echo  "$PAYLOAD" | jq -r '.work_dir')

FILENAME=$(basename "$INPUT_PATH")
STEM="${FILENAME%.*}"
NOTES_DIR="$VAULT_ROOT/Notes/Processed"
OUT_PATH="$NOTES_DIR/$STEM.md"

mkdir -p "$NOTES_DIR"

# Write to temp file first, then atomic replace
TMP="$WORK_DIR/output.tmp"
cat > "$TMP" << EOF
# $STEM

Source: $INPUT_PATH
Processed: $(date -u +"%Y-%m-%dT%H:%M:%SZ")

> Shell processor placeholder. Replace with real extraction logic.
EOF

mv "$TMP" "$OUT_PATH"

# Emit the result JSON as the LAST line of stdout
BYTES=$(wc -c < "$OUT_PATH" | tr -d ' ')
printf '{"status":"ok","outputs":[{"path":"%s","kind":"markdown","bytes":%s}]}\n' \
    "$OUT_PATH" "$BYTES"
```

---

## Troubleshooting

### Files not being processed

**Symptoms:** You drop a file into the sources directory but `kb status` shows no new `queued` entries after 30 seconds.

**Diagnosis steps:**

```bash
# 1. Check the daemon is running
kb status
# → If it shows counts, the daemon is alive
# → If it errors, the daemon isn't running

# 2. Verify the file extension is in the allowlist
kb config show | grep extensions
# Default: pdf, docx, xlsx, ppt, pptx, jpg, jpeg, png

# 3. Run doctor to check all paths resolve correctly
kb doctor

# 4. Check for recent events
kb tail
# → Should show 'discovered' and 'queued' events on file drop

# 5. Look at recent log output
tail -50 ~/Library/Logs/knowledge-builder/kb.log
```

**Common causes:**

| Cause | Fix |
|-------|-----|
| File extension not in `extensions` list | Add extension to `config.toml [watch] extensions` |
| File matches an `ignore_globs` pattern | Check `ignore_globs` in config; remove conflicting pattern |
| Sources directory path mismatch | Run `kb doctor`; verify `sources_dir` matches where you're dropping files |
| Daemon not running | `kb install` or `kb daemon --foreground` |
| File is an iCloud placeholder (`.icloud` suffix) | Wait for iCloud to materialise the file; it will be picked up on the next scan |

---

### Daemon not starting

**Symptoms:** `kb status` errors out, or `launchctl list com.user.knowledge-builder` shows no PID.

**Diagnosis steps:**

```bash
# 1. Check launchctl status
launchctl list com.user.knowledge-builder
# Look for PID in first column; 'LastExitStatus' non-zero = crash

# 2. Check stderr log
cat ~/Library/Logs/knowledge-builder/stderr.log
# This is where startup errors appear

# 3. Try running in foreground to see errors directly
kb daemon --foreground

# 4. Re-run doctor
kb doctor

# 5. Check if another instance is running
lsof ~/Library/Application\ Support/knowledge-builder/state.db.lock
```

**Common causes:**

| Cause | Fix |
|-------|-----|
| Config validation failed | Fix errors reported by `kb doctor` |
| Binary path changed after install | `kb install --force` to re-render the plist |
| Singleton lock stale (previous crash) | `rm ~/Library/Application\ Support/knowledge-builder/state.db.lock` |
| Port 7878 already in use | Change `ops.http_bind` in config.toml |
| Processor command not found | Verify `processor.command` path; run `kb doctor` |

---

### Processing failures

**Symptoms:** `kb status` shows `failed` count > 0; files aren't producing notes.

**Diagnosis steps:**

```bash
# 1. Find all failed files
kb list --status failed

# 2. Inspect a specific failure
kb show 14
# → Check "Last error" field and "Recent events" section

# 3. Test the processor manually with a sample input
echo '{
  "input_path":   "/path/to/test.pdf",
  "content_hash": "sha256:test",
  "vault_root":   "/Users/alice/Vault",
  "sources_dir":  "/Users/alice/Vault/Sources",
  "work_dir":     "/tmp/kb-test",
  "job_id":       0,
  "attempt":      1
}' | processors/default/run.sh /path/to/test.pdf /tmp/kb-test

# 4. Check processor logs in the work directory
ls ~/Library/Caches/knowledge-builder/jobs/
# Failed jobs retain their work_dir for inspection

# 5. After fixing the processor, requeue the failed file
kb requeue 14
# Or requeue all failed files:
kb list --status failed --limit 100 | awk 'NR>1 {print $1}' | xargs -I{} kb requeue {}
```

**Common causes:**

| Cause | Fix |
|-------|-----|
| Missing API key | Set `OPENAI_API_KEY` in environment; add to `launchd` environment via `launchctl setenv` |
| Processor timeout | Increase `processor.timeout_secs`; reduce `worker.concurrency` |
| Output path outside vault | Fix processor to write inside `vault_root` and outside `sources_dir` |
| Python dependency missing | `pip install -r processors/default/requirements.txt` |
| Corrupt/unreadable source file | Check file integrity; `kb reset` to remove and re-drop |

---

### High memory usage

**Symptoms:** The `kb` daemon is consuming more RAM than expected (typically > 500 MB).

**Diagnosis:**

```bash
# Check how many workers are running simultaneously
kb status
# Look at 'processing' count

# Check if large files are being processed
kb list --status processing
```

**Fix: reduce concurrency**

```toml
# ~/.config/knowledge-builder/config.toml
[worker]
concurrency = 1   # process one file at a time
```

Apply by restarting the daemon:

```bash
kb install --force
```

**Other tuning options:**

- Reduce `hash_chunk_bytes` to lower per-file memory during hashing
- Increase `timeout_secs` and reduce `concurrency` if processing very large files

---

### Database corruption

**Symptoms:** `kb doctor` reports SQLite integrity check failed; unusual errors in `kb status` or `kb list`.

```bash
# 1. Confirm the issue
kb doctor
# Look for: "SQLite integrity check: FAILED"

# 2. Stop the daemon
launchctl stop com.user.knowledge-builder

# 3. Restore from backup
kb restore ~/path/to/kb-backup-YYYY-MM-DD.db

# 4. If no backup available, rebuild state from scratch
rm ~/Library/Application\ Support/knowledge-builder/state.db
# Restart daemon — it will re-discover all files via the periodic scan

# 5. Restart
launchctl start com.user.knowledge-builder
```

**Prevention:** Schedule regular backups:

```bash
# Add to crontab (runs every Sunday at 3am)
0 3 * * 0 /usr/local/bin/kb backup --output ~/Backups/kb-$(date +\%Y-\%m-\%d).db
```

> **Note:** The database is stored in `~/Library/Application Support/` which iCloud Drive may try to sync. If you experience repeated corruption, move `db_path` outside the iCloud-synced tree (e.g., `/usr/local/var/knowledge-builder/state.db`).

---

### Viewing logs

```bash
# Stream the current structured log (requires jq)
tail -f ~/Library/Logs/knowledge-builder/kb.log | jq .

# Filter for errors only
tail -f ~/Library/Logs/knowledge-builder/kb.log | jq 'select(.level == "ERROR")'

# launchd stdout/stderr logs
tail -f ~/Library/Logs/knowledge-builder/stdout.log
tail -f ~/Library/Logs/knowledge-builder/stderr.log

# Increase log verbosity without restarting
# Add to config.toml and run `kb install --force`:
# [ops]
# log_level = "debug"

# Or use RUST_LOG for per-module verbosity:
RUST_LOG=kb_worker=debug,kb_watcher=debug,info kb daemon --foreground
```

---

## Development

### Building

```bash
# Debug build (faster compile, slower runtime)
cargo build

# Release build (optimized)
cargo build --release

# Check all crates compile without building
cargo check --workspace

# Run all tests
cargo test --workspace

# Run tests for a specific crate
cargo test -p kb-core
cargo test -p kb-worker
```

### Testing

The test suite has 200+ unit tests across all crates. Integration tests live in `tests/integration/`.

```bash
# Run all tests with output
cargo test --workspace -- --nocapture

# Run a specific test
cargo test -p kb-core state::tests::psf_rule1_already_done_same_hash

# Run integration tests
cargo test --test integration

# Property tests (proptest)
cargo test -p kb-core proptest
```

**Note on timing-sensitive tests:** `kb-worker` includes subprocess tests with real timeouts (up to 6 seconds). `cargo test -p kb-worker` is expected to take at least 6 seconds.

### Project structure

```
knowledge_builder/
├── Cargo.toml                     # workspace root
├── crates/
│   ├── kb-core/                   # types, config, state, paths, migrations, lock
│   ├── kb-watcher/                # watcher, stability, hasher, scanner, pipeline
│   ├── kb-worker/                 # pool, processor subprocess, parser, validator
│   ├── kb-ops/                    # axum HTTP server, SSE
│   └── kb-cli/                    # kb binary + all subcommands
├── processors/
│   └── default/                   # Python processor (OCR + LLM pipeline)
│       ├── pyproject.toml
│       ├── run.sh                 # entry point called by the daemon
│       └── kb_processor/
│           ├── extractors/        # pdf, docx, xlsx, pptx, image
│           ├── llm.py             # LLM synthesis pipeline
│           └── writer.py          # atomic output writer
├── installer/
│   └── com.user.knowledge-builder.plist  # launchd plist template
└── tests/
    └── integration/               # cross-crate integration tests
```

### Crate dependency graph

```
kb-cli ──▶ kb-ops ──▶ kb-worker ──▶ kb-core
                 │              │
                 │              └──▶ kb-watcher ──▶ kb-core
                 │
                 └──▶ kb-core
```

### Adding a new migration

Migrations are append-only. Never edit existing migrations.

```rust
// In crates/kb-core/src/migrations.rs

const MIGRATION_002: &str = r#"
    ALTER TABLE files ADD COLUMN my_new_column TEXT;
"#;

// Append to the MIGRATIONS array:
static MIGRATIONS: &[(u32, &str)] = &[
    (1, MIGRATION_001),
    (2, MIGRATION_002),  // ← add here
];
```

`run_migrations()` is idempotent — it checks `schema_version` and only applies pending migrations.

### Running the default Python processor locally

```bash
cd processors/default
pip install -e .

# Test with a real file
mkdir -p /tmp/kb-test-work
echo '{
  "input_path": "/path/to/test.pdf",
  "content_hash": "sha256:abc123",
  "vault_root": "/Users/alice/Vault",
  "sources_dir": "/Users/alice/Vault/Sources",
  "work_dir": "/tmp/kb-test-work",
  "job_id": 1,
  "attempt": 1
}' | bash run.sh /path/to/test.pdf /tmp/kb-test-work
```

### Contributing

1. Fork the repository and create a feature branch
2. Ensure `cargo check --workspace` passes with zero warnings
3. Ensure `cargo test --workspace` passes
4. Add tests for new functionality
5. Update `task_update.md` with your changes
6. Open a pull request

**Code style:** `cargo fmt --all` before committing. `cargo clippy --workspace` should be warning-free.

**Before adding a new dependency:** check if it's already in `[workspace.dependencies]` in the root `Cargo.toml`. Add new crates there first, then reference them with `{ workspace = true }` in individual crate `Cargo.toml` files.

---

## HTTP API Reference

The daemon exposes a local REST API on `127.0.0.1:7878` (loopback only). No authentication.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/healthz` | Liveness check + uptime |
| `GET` | `/stats` | Queue counts, depth, last error |
| `GET` | `/files` | Paginated file list (`?status=&limit=&offset=`) |
| `GET` | `/files/by-path` | Look up by path (`?path=<url-encoded-path>`) |
| `GET` | `/files/:id` | File details + outputs + recent events |
| `POST` | `/files/:id/requeue` | Reset to queued |
| `POST` | `/files/:id/reset` | Delete row and outputs |
| `POST` | `/scan` | Trigger immediate full scan |
| `GET` | `/events` | Recent audit events (`?since=&level=&kind=&limit=`) |
| `GET` | `/tail` | SSE stream of live events |

**Examples:**

```bash
# Health check
curl -s http://127.0.0.1:7878/healthz | jq .
# {"status":"ok","uptime_secs":3600}

# Queue statistics
curl -s http://127.0.0.1:7878/stats | jq .
# {"queued":3,"processing":1,"done":47,"failed":2,"skipped":5,"queue_depth":4}

# List failed files
curl -s "http://127.0.0.1:7878/files?status=failed" | jq .

# Requeue a file
curl -s -X POST http://127.0.0.1:7878/files/14/requeue | jq .
# {"ok":true,"message":"Requeued file 14"}

# Stream live events (Server-Sent Events)
curl -N http://127.0.0.1:7878/tail
```

---

*Knowledge Builder is built with Rust, tokio, axum, rusqlite, clap, and notify.*
