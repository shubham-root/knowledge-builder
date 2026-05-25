# Knowledge Builder — Quickstart

Zero to your first integrated note in ~15 minutes.  This is the
shortest path; the [README](README.md) covers everything else.

## 0.  Prerequisites checklist

Run all four; install whatever's missing before continuing.

```bash
sw_vers -productVersion          # macOS 12+
rustc --version                  # 1.75+ (only if building from source)
python3 --version                # 3.12+
which obsidian || echo MISSING   # Obsidian CLI must be enabled
```

If `obsidian` is missing, open Obsidian → **Settings → General →
Command line interface** and follow the prompt.

You will also need an **OpenRouter** API key (sign up at
https://openrouter.ai/keys) or a key for any other provider supported
by `litellm`.

## 1.  Build and install the binaries (~5 min)

```bash
# Rust daemon (~60-90 s on a cold cache)
cargo build --release
sudo cp target/release/kb /usr/local/bin/kb
kb --version

# pi-coding-agent (the LLM agent loop)
npm install -g @earendil-works/pi-coding-agent
pi --version

# Python processor venv (~3 min, downloads docling + torch)
python3 -m venv ~/.local/share/kb/venv
~/.local/share/kb/venv/bin/pip install --upgrade pip
~/.local/share/kb/venv/bin/pip install -e "$(pwd)/processors/default[llm]"
ls -l ~/.local/share/kb/venv/bin/kb-processor    # should exist
```

## 2.  Prepare your vault (1 min)

If you don’t already have an Obsidian vault, create one:

```bash
mkdir -p ~/Documents/Obsidian/Sources
mkdir -p ~/Documents/Obsidian/KnowledgeBase
# Open Obsidian once, point it at ~/Documents/Obsidian, close it.
# This makes the .obsidian/ folder so the CLI knows about the vault.
```

The two directories you just created have specific roles:

* **`Sources/`** — you drop files here.  Read-only for the agent.
* **`KnowledgeBase/`** — the agent’s mutation sandbox.  Everything
  it creates or modifies stays inside this subtree.  Hand-written
  notes elsewhere in the vault are protected by the wrapper.

## 3.  Configure (2 min)

Knowledge Builder reads two files in `~/.config/knowledge-builder/`.
Create them:

```bash
mkdir -p ~/.config/knowledge-builder
```

### `config.toml`

```bash
cat > ~/.config/knowledge-builder/config.toml <<'EOF'
[paths]
vault_root  = "~/Documents/Obsidian"
sources_dir = "~/Documents/Obsidian/Sources"
agent_root  = "~/Documents/Obsidian/KnowledgeBase"

[processor]
command = "~/.local/share/kb/venv/bin/kb-processor"
EOF
```

Defaults handle everything else (worker concurrency, log paths,
backoff, watch globs).

### `secrets.env` (mode 600 — not optional)

```bash
cat > ~/.config/knowledge-builder/secrets.env <<'EOF'
KB_LLM_MODEL=openrouter/anthropic/claude-3.5-haiku
OPENROUTER_API_KEY=sk-or-v1-...replace-me...

# Default is `apply` (the agent writes to KnowledgeBase/ for real).
# Set to `shadow` only when you want to review a plan without
# executing it — useful when validating a new model or skill change.
# KB_AGENT_MODE=apply
EOF
chmod 600 ~/.config/knowledge-builder/secrets.env
```

Replace `sk-or-v1-...replace-me...` with your real OpenRouter key.

## 4.  Sanity check (10 s)

```bash
kb doctor
```

Every line should print `✓`.  If anything fails, the error tells you
exactly what to fix.  Common ones:

* `agent_root '…' overlaps sources_dir`     → you nested them; move one.
* `Secrets file '…' has permissive mode 644` → `chmod 600 …`.
* `processor.command '…': file not found`    → the venv install didn’t
  finish; rerun the `pip install -e` step.

## 5.  Start the daemon

For the first run, foreground mode lets you see what’s happening:

```bash
kb daemon --foreground
```

Leave that terminal open.  Open a second terminal for the next steps.

For permanent operation (once you’re confident), install as a
LaunchAgent:

```bash
kb install
```

`kb install` runs `kb doctor` first and refuses if anything fails.

## 6.  Drop your first file

In the second terminal:

```bash
# Pick any small PDF you have lying around.
cp ~/Downloads/some_paper.pdf ~/Documents/Obsidian/Sources/

# Watch live events:
kb tail
```

Within a few seconds the daemon notices the new file.  You’ll see
events stream by:

```
discovered  paper.pdf
stable      paper.pdf  (size+mtime stable for 2s)
hashed      paper.pdf  sha256:9af1...
queued      paper.pdf
processor_started  attempt 1
  ... extraction (docling, MPS) ...
  ... agent run (pi --mode rpc) ...
done        paper.pdf
```

Look at the result:

```bash
kb status                # one-line queue summary
kb list --status done    # find the row id
kb show 1                # full detail for that row
```

`kb show` prints the agent’s plan section: every operation it
proposed, the LLM’s reasoning, and the post-run audit verdict.

## 7.  Optionally: dry-run with shadow mode

By default the agent writes to your vault directly.  Three layered
guards keep the blast radius contained — see [README → Safety
model](README.md#safety-model) — and Obsidian’s File Recovery plugin
provides per-note version history for everything the agent touches.

If you ever want to inspect a plan before it executes (validating a
new model, debugging a skill regression, etc.), flip to shadow mode
for that one job:

```bash
# Add to ~/.config/knowledge-builder/secrets.env:
#   KB_AGENT_MODE=shadow
# Restart the daemon to re-read the env:
launchctl kickstart -k gui/$UID/com.user.knowledge-builder    # if installed
# or just Ctrl-C `kb daemon --foreground` and run it again.
```

In shadow mode the kb-obsidian wrapper records every intended write
to `<work_dir>/.kb-plan.jsonl` without executing it.  The work_dir is
preserved on success so `kb show <id>` displays the full plan.
Remove the override (or set it back to `apply`) when you’re done.

Obsidian’s File Recovery plugin keeps versioned history of every edit,
so `obsidian history:list file=<note>` and `history:restore` are your
undo path if the agent does something you don’t like.

## 8.  When something goes wrong

```bash
kb list --status failed         # rows that hit max_attempts
kb show <id>                    # see the error and the agent log
kb requeue <id>                 # try again with attempts reset
kb reset <id>                   # forget about it; next discovery treats as new
```

The full log lives at `~/Library/Logs/knowledge-builder/`.

Three common situations:

* **Agent ran but plan is empty.**  The model didn’t issue any
  `kb-obsidian` commands (often a model-capability issue).  Try a
  stronger model in `secrets.env`
  (e.g. `KB_LLM_MODEL=openrouter/anthropic/claude-sonnet-4-5`) and
  `kb requeue` the row.
* **`kb show` flags rogue writes.**  The agent bypassed the
  `kb-obsidian` wrapper via raw bash.  Check the listed paths and
  decide whether to keep, move, or delete.  See [README → Safety
  model](README.md#safety-model) for what the audit can and can’t
  catch.
* **Large PDF takes hours.**  Look at `kb show <id>` — the
  per-batch timings tell you which pages are scanned vs text-native.
  Increase `KB_PDF_BATCH_TIMEOUT_SECS` in `secrets.env` if individual
  scanned batches are timing out.

## 9.  What to read next

* [README → Architecture at a glance](README.md#architecture-at-a-glance)
  — how the three programs cooperate.
* [README → Pipeline internals](README.md#pipeline-internals) —
  every stage of one job, end to end.
* [README → Safety model](README.md#safety-model) — the three-layer
  sandbox and what it does and does not catch.
* [README → CLI reference](README.md#cli-reference) — all
  subcommands, flags, and behaviour.
* [README → Configuration](README.md#configuration) — every key in
  `config.toml` and `secrets.env`, with defaults and rationale.

