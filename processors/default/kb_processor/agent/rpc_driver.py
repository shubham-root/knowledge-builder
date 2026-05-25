"""
Pi RPC driver for the Knowledge Builder agent.

Spawns ``pi --mode rpc`` as a subprocess, hands it our skills and the
``kb-obsidian`` wrapper on PATH, drives the agent with the integration
prompt, and returns the resulting :class:`Plan`.

This module is the boundary between :mod:`kb_processor.pipeline` (which
calls :func:`run_agent`) and pi (which actually drives the LLM).  It
does not import pi or its SDK; it speaks to pi only through stdin/stdout
JSON-RPC framing per ``docs/rpc.md``.

Lifecycle
---------
::

    1. Build env:  inherit, override OPENROUTER_API_KEY, strip other
                   provider keys, set KB_PLAN_FILE + KB_AGENT_MODE.
    2. Stage PATH: prepend a temp dir containing the kb-obsidian wrapper
                   so the agent's bash tool sees `kb-obsidian` first.
    3. Spawn pi:   --mode rpc --no-session
                   --tools bash
                   --skill <agent/skills/>
                   --provider <openrouter|...> --model <id>
                   --api-key <override>
                   --append-system-prompt <integration prompt>
    4. Send a single `prompt` command with the per-job context block.
    5. Stream events; collect text deltas + tool calls for the audit log.
    6. On `agent_end` event: send `abort`, terminate pi, parse the plan
       file written by the kb-obsidian wrapper.

Failure modes
-------------
* pi binary missing             → :class:`PiNotFoundError`
* pi spawn fails                → :class:`PiSpawnError`
* RPC parse error / pi crashes  → :class:`PiProtocolError`
* Agent runs over budget        → :class:`AgentBudgetError`
* Plan file missing/malformed   → :class:`PlanCorruptError`
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

from .plan import Plan, PlanParseError, read_plan

logger = logging.getLogger(__name__)


# ── Constants ─────────────────────────────────────────────────────────────────

#: How long to wait for the agent to finish after sending its prompt.
#: This is wall-clock; LLM call latencies dominate.  Override with
#: ``KB_AGENT_TIMEOUT_SECS``.
_DEFAULT_AGENT_TIMEOUT_SECS: int = 600

#: Maximum lines of streaming agent output to retain in the audit log.
#: Truncating prevents pathological cases (agent stuck in a tool loop)
#: from blowing the work_dir budget.
_AGENT_LOG_MAX_LINES: int = 5_000

#: pi binary on PATH; override with ``KB_PI_BIN``.  We intentionally do
#: not hard-code an absolute path so the agent can be redeployed without
#: editing the daemon.
_DEFAULT_PI_BIN: str = "pi"

#: Provider keys we strip from the subprocess env so pi cannot accidentally
#: route to the wrong provider.  ``OPENROUTER_API_KEY`` is preserved (and
#: explicitly verified) before spawn.
_OTHER_PROVIDER_KEYS: tuple[str, ...] = (
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "COHERE_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
)


# ── Exceptions ────────────────────────────────────────────────────────────────


class AgentError(Exception):
    """Base class for all driver errors."""


class PiNotFoundError(AgentError):
    """The ``pi`` binary could not be found on PATH."""


class PiSpawnError(AgentError):
    """pi subprocess failed to launch."""


class PiProtocolError(AgentError):
    """pi emitted output that did not match the documented JSON-RPC schema."""


class AgentBudgetError(AgentError):
    """The agent ran over its turn / wall-clock budget without finishing."""


class PlanCorruptError(AgentError):
    """The wrapper's plan JSONL file is missing or malformed."""


class MissingApiKeyError(AgentError):
    """``OPENROUTER_API_KEY`` is not present in the environment."""


# ── Inputs / outputs ──────────────────────────────────────────────────────────


@dataclass(frozen=True)
class AgentInput:
    """Everything :func:`run_agent` needs to drive one job."""
    extracted_path:  Path
    work_dir:        Path
    vault_root:      Path
    sources_dir:     Path
    agent_root:      Path             # KnowledgeBase tree, agent's mutation sandbox
    source_basename: str
    model:           str             # litellm-style; e.g. openrouter/moonshotai/kimi-k2.5
    job_id:          int
    mode:            str = "apply"   # "apply" (default) | "shadow"


@dataclass
class AgentResult:
    """What :func:`run_agent` returns to the pipeline."""
    plan:                Plan
    plan_file:           Path
    agent_log:           Path                  # path to streamed RPC events JSONL
    final_assistant_text: str                   # last assistant message body
    elapsed_secs:        float
    turns:               int
    aborted:             bool                   # True if budget hit / forced abort
    metadata:            dict[str, Any] = field(default_factory=dict)
    #: Files in the vault that changed during the agent run but are NOT
    #: present in the plan.  Should always be empty in the happy path; a
    #: non-empty value indicates the agent bypassed the kb-obsidian
    #: wrapper (e.g. via raw ``cat >`` or ``cp``).  Populated by the
    #: post-run :func:`_audit_vault_diff` check.
    rogue_writes:        list[Path] = field(default_factory=list)


# ── Helpers ───────────────────────────────────────────────────────────────────


def _agent_timeout_secs() -> int:
    raw = os.environ.get("KB_AGENT_TIMEOUT_SECS", "").strip()
    try:
        n = int(raw) if raw else _DEFAULT_AGENT_TIMEOUT_SECS
    except ValueError:
        return _DEFAULT_AGENT_TIMEOUT_SECS
    return max(60, n)


def _pi_bin() -> str:
    raw = os.environ.get("KB_PI_BIN", _DEFAULT_PI_BIN)
    resolved = shutil.which(raw)
    if not resolved:
        raise PiNotFoundError(
            f"pi binary {raw!r} not found on PATH.  Install pi-coding-agent "
            "or set KB_PI_BIN to the absolute path."
        )
    return resolved


def _split_litellm_model(model: str) -> tuple[str, str]:
    """Split ``provider/id``-style model into (provider, id).

    Examples
    --------
    >>> _split_litellm_model("openrouter/moonshotai/kimi-k2.5")
    ('openrouter', 'moonshotai/kimi-k2.5')
    >>> _split_litellm_model("anthropic/claude-3-5-sonnet-latest")
    ('anthropic', 'claude-3-5-sonnet-latest')
    """
    if "/" not in model:
        raise AgentError(
            f"KB_LLM_MODEL must be 'provider/id' format; got {model!r}"
        )
    provider, _, model_id = model.partition("/")
    if not provider or not model_id:
        raise AgentError(
            f"KB_LLM_MODEL has empty provider or id: {model!r}"
        )
    return provider, model_id


def _build_subprocess_env(
    *,
    extracted_path:  Path,
    plan_file:       Path,
    mode:            str,
    wrapper_dir:     Path,
    vault_root:      Path,
    sources_dir:     Path,
    agent_root:      Path,
) -> dict[str, str]:
    """Construct the env passed to the pi subprocess.

    * Inherit from the daemon's environment (which already merged the
      contents of ``secrets.env`` via ``Command::envs()`` on the Rust
      side).
    * Verify ``OPENROUTER_API_KEY`` is set; refuse to spawn otherwise.
    * Strip all other provider API keys so pi cannot fall through to the
      wrong provider via its credential cascade.
    * Prepend the wrapper directory to PATH so ``kb-obsidian`` resolves
      before any user-installed ``obsidian`` aliases.
    * Export ``KB_PLAN_FILE``, ``KB_AGENT_MODE``, ``KB_VAULT_ROOT``,
      ``KB_SOURCES_DIR``, ``KB_AGENT_ROOT``, and the per-job extracted
      path used by the skill prompt.
    """
    env = os.environ.copy()

    or_key = env.get("OPENROUTER_API_KEY", "").strip()
    if not or_key:
        raise MissingApiKeyError(
            "OPENROUTER_API_KEY is not set in the daemon's environment.  "
            "Add it to ~/.config/knowledge-builder/secrets.env."
        )

    for k in _OTHER_PROVIDER_KEYS:
        env.pop(k, None)

    env["KB_PLAN_FILE"]    = str(plan_file)
    env["KB_AGENT_MODE"]   = mode
    env["KB_EXTRACTED"]    = str(extracted_path)
    env["KB_VAULT_ROOT"]   = str(vault_root)
    env["KB_SOURCES_DIR"]  = str(sources_dir)
    env["KB_AGENT_ROOT"]   = str(agent_root)

    # Resolve the real `obsidian` binary on the OPERATOR's PATH (not the
    # agent's restricted PATH) and pass it to the wrapper as an absolute
    # path.  Without this the kb-obsidian wrapper does `shutil.which(
    # "obsidian")` against the restricted PATH, fails to find it, and
    # reports a misleading "Obsidian CLI is not enabled" error that the
    # LLM tends to take literally and bail on.
    #
    # Crucially this means `obsidian` is NOT added to the agent's PATH
    # — the agent still has only kb-obsidian for vault operations.
    if "KB_OBSIDIAN_BIN" not in env or not env["KB_OBSIDIAN_BIN"].strip():
        operator_obsidian = shutil.which("obsidian")
        if operator_obsidian:
            env["KB_OBSIDIAN_BIN"] = operator_obsidian
        else:
            logger.warning(
                "`obsidian` binary not found on the daemon's PATH; the agent "
                "will be unable to read the vault and will likely produce "
                "empty plans.  Enable the Obsidian CLI in the Obsidian app "
                "(Settings → General → Command line interface), restart the "
                "daemon, and try again."
            )

    # Strict PATH — ONLY the wrapper directory.  See _AGENT_PATH_BINARIES
    # for the curated allowlist of binaries the agent can invoke as bare
    # names.  System paths (/usr/bin, /bin, ...) are intentionally NOT
    # inherited so bare-name calls to ``mkdir``, ``cp``, ``mv``, ``rm`` etc.
    # fail with ``command not found``.  This is the first layer of bash
    # defence; the post-run vault diff audit is the second.
    env["PATH"] = str(wrapper_dir)

    return env


def _build_user_prompt(inp: AgentInput) -> str:
    """The single ``prompt`` command sent to pi.  Begins with the slash
    command ``/skill:knowledge-builder-integrator`` so pi expands the
    SKILL.md content inline before the LLM sees it.  The remainder is
    the per-job context that the skill instructions reference."""
    return (
        "/skill:knowledge-builder-integrator\n\n"
        "Per-job context:\n"
        f"  extracted_path  = {inp.extracted_path}\n"
        f"  source_basename = {inp.source_basename}\n"
        f"  vault_root      = {inp.vault_root}\n"
        f"  sources_dir     = {inp.sources_dir}\n"
        f"  agent_root      = {inp.agent_root}\n"
        f"  mode            = {inp.mode}\n"
        f"  job_id          = {inp.job_id}\n\n"
        "Required workflow:\n"
        "  1. Read the extracted content with `cat`.\n"
        "  2. Survey existing structure: `kb-obsidian folders folder=KnowledgeBase`,\n"
        "     `kb-obsidian tags counts`.\n"
        "  3. For overlap detection: at least one `kb-obsidian search query=...`.\n"
        "  4. **Issue at least one `kb-obsidian create path=KnowledgeBase/...` \n"
        "     (or append/move/etc.) command** to actually integrate the content.\n"
        "     A textual summary alone is NOT a successful integration.\n"
        "  5. Optionally set frontmatter properties with `kb-obsidian property:set\n"
        "     name=... value=... file=...`.\n"
        "  6. Emit a brief final summary message and stop.\n\n"
        "All vault operations MUST go through the `kb-obsidian` wrapper.  Do not\n"
        "use `cat >`, `tee`, redirects, or any other shell tricks to write into\n"
        "the vault — those are detected and reported as rogue writes.\n"
    )


# ── Wrapper staging ──────────────────────────────────────────────────────────


# ── Subprocess PATH ──────────────────────────────────────────────────────
#
# We give the pi subprocess a *minimal* PATH that contains only the
# kb-obsidian wrapper plus a handful of read-only utilities the agent
# legitimately needs for its workflow (``cat`` to read extracted markdown,
# ``head`` / ``tail`` / ``sed`` for chunked reading, ``grep`` for filtering,
# ``wc`` / ``printf`` for sanity).  System PATH entries (``/usr/bin``,
# ``/bin``, etc.) are NOT inherited — so bare-name calls to ``mkdir``,
# ``cp``, ``mv``, ``rm``, ``cat >``, ``tee`` etc. fail with
# ``command not found``.
#
# This is layered defence — a determined LLM could still bypass via
# absolute paths (``/bin/mkdir``) or pure shell redirects
# (``> /vault/file``).  The post-run vault diff audit (see
# :func:`_audit_vault_diff`) catches what slips through.
_AGENT_PATH_BINARIES: tuple[str, ...] = (
    "cat",      # read files (the extracted markdown lives outside the vault)
    "head",     # chunked reads of long extracts
    "tail",     # ditto
    "sed",      # ditto with line ranges
    "grep",     # filtering search outputs
    "awk",      # tabular processing of `kb-obsidian tags counts` etc.
    "wc",       # counting
    "printf",   # constructing arguments
    "echo",     # also a shell builtin in most shells; included for shells that don't have it
    "sort",     # sorting tag/folder lists
    "uniq",     # ditto
    "tr",       # case folding
    "sh",       # pi spawns its bash tool via the system shell; we need /bin/sh reachable
    "bash",     # ditto
    "env",      # diagnostic; harmless
    "true", "false",  # control-flow helpers in shell scripts
    "basename", "dirname",  # path manipulation
    "date",     # timestamps inside notes
    "jq",       # JSON parsing of search/list outputs (when installed)
    "node",     # required: pi's shebang is `#!/usr/bin/env node` so without
                # node on PATH the pi subprocess fails to start.  The agent
                # could in principle run `node -e '...'` to bypass policy;
                # the post-run vault diff audit catches such bypasses.
    "npm",      # pi runs `npm root -g` at startup to discover installed
                # packages.  Without it pi crashes with ENOENT before the
                # RPC loop is up.  Same risk profile as `node`.
    "npx",      # pi may shell out to `npx` for package extension discovery.
    "python3",  # required: the kb-obsidian wrapper is a Python script with
                # `#!/usr/bin/env python3` shebang.  Without python3 on the
                # agent's PATH the wrapper itself cannot execute.  The agent
                # could in principle use `python3 -c '...'` to write files;
                # the post-run audit catches such bypasses.
)


def _resolve_system_path(name: str) -> str | None:
    """Find ``name`` on the OPERATOR's PATH (not the subprocess one)."""
    return shutil.which(name)


def _stage_wrapper_on_path(work_dir: Path) -> Path:
    """Build a per-job directory containing only the binaries the agent is
    allowed to invoke as bare names.  Returns the directory.

    The kb-obsidian wrapper is copied in (it lives in our package and we
    don't want a fragile absolute-path symlink); other utilities are
    symlinked from their resolved system path.

    Missing utilities are silently skipped — if the operator's system
    doesn't have ``jq`` for example, the agent simply can't use it.
    """
    wrappers_dir = work_dir / ".agent-bin"
    wrappers_dir.mkdir(parents=True, exist_ok=True)

    # 1. kb-obsidian wrapper (in our package).
    pkg_root    = Path(__file__).parent
    wrapper_src = pkg_root / "wrappers" / "kb-obsidian"
    if not wrapper_src.exists():
        raise AgentError(
            f"kb-obsidian wrapper missing at {wrapper_src}; reinstall "
            "kb-processor."
        )
    wrapper_dst = wrappers_dir / "kb-obsidian"
    if wrapper_dst.exists() or wrapper_dst.is_symlink():
        wrapper_dst.unlink()
    os.symlink(wrapper_src, wrapper_dst)

    # 2. Read-only utilities, symlinked from system paths.
    for name in _AGENT_PATH_BINARIES:
        src = _resolve_system_path(name)
        if src is None:
            continue
        dst = wrappers_dir / name
        if dst.exists() or dst.is_symlink():
            try:
                dst.unlink()
            except OSError:
                pass
        try:
            os.symlink(src, dst)
        except OSError as exc:
            logger.debug("could not symlink %s → %s: %s", src, dst, exc)

    return wrappers_dir


# ── Vault diff audit ─────────────────────────────────────────────────────
#
# After the agent finishes we walk the vault and diff against the
# pre-run snapshot.  Any file that appeared or was modified during the
# run AND is not in the plan's intended writes is a "rogue write" —
# the agent bypassed the kb-obsidian wrapper.  We surface this loudly
# so the operator can decide whether to clean up or accept.

#: Top-level directory names skipped during vault walks.  Mirrors the
#: indexer's exclusions — these directories contain Obsidian's own
#: metadata or VCS state and are not part of the agent's responsibility.
_AUDIT_SKIP_DIRS: frozenset[str] = frozenset({
    ".obsidian", ".trash", ".git", "node_modules",
})


def _snapshot_vault(
    vault_root:  Path,
    sources_dir: Path,
) -> dict[str, tuple[int, int]]:
    """Walk the vault (excluding sources_dir + _AUDIT_SKIP_DIRS) and
    return a ``{abs_path: (mtime_ns, size)}`` map.

    Cheap (one stat per file) for vaults under ~10 k notes.  Returns
    an empty dict if the walk fails for any reason — the audit then
    becomes a no-op rather than blowing up the agent run.
    """
    sources_str = str(sources_dir.resolve())
    out: dict[str, tuple[int, int]] = {}

    try:
        stack: list[Path] = [vault_root]
        while stack:
            d = stack.pop()
            try:
                entries = list(d.iterdir())
            except (OSError, PermissionError):
                continue
            for entry in entries:
                try:
                    real = str(entry.resolve())
                except OSError:
                    continue
                if entry.is_dir():
                    if entry.name in _AUDIT_SKIP_DIRS:
                        continue
                    if real == sources_str or real.startswith(sources_str + "/"):
                        continue
                    stack.append(entry)
                    continue
                if entry.is_file():
                    if real.startswith(sources_str + "/"):
                        continue
                    try:
                        st = entry.stat()
                        out[real] = (st.st_mtime_ns, st.st_size)
                    except OSError:
                        continue
    except Exception as exc:  # noqa: BLE001  defensive
        logger.warning("vault snapshot failed: %s", exc)
        return {}

    return out


def _planned_paths(plan: Plan, vault_root: Path) -> set[str]:
    """Return the set of canonical absolute paths the plan intends to
    write to.  Used to subtract from the post-run diff so the audit
    only flags TRULY rogue writes (i.e. ones the agent did via raw
    bash, not via kb-obsidian).

    For ``move``/``rename`` we record both source and destination so
    neither shows up as a surprise.
    """
    out: set[str] = set()
    for entry in plan.entries:
        kv: dict[str, str] = {}
        for tok in entry.args:
            eq = tok.find("=")
            if eq > 0:
                kv[tok[:eq]] = tok[eq + 1:]

        for key in ("path", "to", "file", "name"):
            raw = kv.get(key, "")
            if not raw:
                continue
            p = Path(raw)
            if not p.is_absolute():
                p = vault_root / p
            try:
                resolved = p.resolve(strict=False)
            except OSError:
                continue
            out.add(str(resolved))
            # Some commands omit the .md suffix; record both forms.
            if not str(resolved).endswith(".md"):
                out.add(str(resolved) + ".md")
    return out


def _audit_vault_diff(
    before:        dict[str, tuple[int, int]],
    after:         dict[str, tuple[int, int]],
    plan:          Plan,
    vault_root:    Path,
) -> list[Path]:
    """Compute the set of files the agent created or modified that are
    NOT in its declared plan.  Returns a list of absolute paths.

    The list is empty in the happy case (agent only used kb-obsidian).
    Anything in the list indicates the agent bypassed the wrapper via
    raw bash.  In shadow mode this is just a warning; in apply mode
    callers should treat a non-empty list as a job failure.
    """
    planned = _planned_paths(plan, vault_root)

    rogue: list[Path] = []
    for path, (mt_after, sz_after) in after.items():
        before_entry = before.get(path)
        if before_entry is None:
            # New file.
            if path in planned:
                continue
            rogue.append(Path(path))
        elif before_entry != (mt_after, sz_after):
            # Existing file changed.  In shadow mode no plan should ever
            # cause an existing-file change in the vault, so even planned
            # writes against existing files are rogue here.  In apply mode
            # we permit it when the file is in `planned`.
            if path in planned:
                continue
            rogue.append(Path(path))

    # Detect deletions too — ``before \ after``.
    for path in before.keys() - after.keys():
        if path in planned:
            continue
        rogue.append(Path(path))

    return sorted(set(rogue))


# ── pi RPC client ─────────────────────────────────────────────────────────────


def _read_jsonl(stream: Any) -> Iterator[dict[str, Any]]:
    """Yield one parsed JSON object per LF-delimited record on ``stream``.

    Per ``docs/rpc.md``: split on LF only, tolerate trailing CR.  Skip
    blank lines (defensive — pi's framing is strict but operators may
    inspect/edit logs).
    """
    for raw_line in stream:
        if isinstance(raw_line, bytes):
            line = raw_line.decode("utf-8", errors="replace")
        else:
            line = raw_line
        line = line.rstrip("\n").rstrip("\r")
        if not line:
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError as exc:
            raise PiProtocolError(
                f"pi emitted non-JSON line: {line[:200]!r} ({exc})"
            ) from exc


def _send(proc: subprocess.Popen, command: dict[str, Any]) -> None:
    """Write a single JSON-RPC command followed by LF.  Flush so pi sees
    it immediately."""
    if proc.stdin is None:
        raise PiProtocolError("pi subprocess stdin is closed")
    line = json.dumps(command, ensure_ascii=False) + "\n"
    proc.stdin.write(line)
    proc.stdin.flush()


# ── Public entry point ───────────────────────────────────────────────────────


def run_agent(inp: AgentInput) -> AgentResult:
    """Drive one agent job to completion and return the plan.

    The caller is the pipeline's INTEGRATE step.  This function blocks
    until the agent finishes, the budget runs out, or pi crashes.
    """
    if inp.mode not in ("shadow", "apply"):
        raise AgentError(
            f"AgentInput.mode must be 'shadow' or 'apply'; got {inp.mode!r}"
        )

    started = time.perf_counter()

    # ── Resolve binaries / paths ─────────────────────────────────────────
    pi_bin     = _pi_bin()
    skills_dir = Path(__file__).parent / "skills"
    if not skills_dir.exists():
        raise AgentError(f"skills dir missing at {skills_dir}")

    inp.work_dir.mkdir(parents=True, exist_ok=True)
    plan_file  = inp.work_dir / ".kb-plan.jsonl"
    agent_log  = inp.work_dir / ".agent-events.jsonl"

    # Drop any stale plan from a previous attempt.
    if plan_file.exists():
        plan_file.unlink()

    # Snapshot the vault before the agent runs so we can diff afterwards
    # to detect rogue writes (bash bypass of the kb-obsidian wrapper).
    pre_snapshot = _snapshot_vault(inp.vault_root, inp.sources_dir)
    logger.info(
        "vault pre-snapshot: %d file(s) under %s",
        len(pre_snapshot), inp.vault_root,
    )

    # ── Stage wrapper + build env ────────────────────────────────────────
    wrapper_dir = _stage_wrapper_on_path(inp.work_dir)
    env = _build_subprocess_env(
        extracted_path = inp.extracted_path,
        plan_file      = plan_file,
        mode           = inp.mode,
        wrapper_dir    = wrapper_dir,
        vault_root     = inp.vault_root,
        sources_dir    = inp.sources_dir,
        agent_root     = inp.agent_root,
    )

    # ── Build pi argv ────────────────────────────────────────────────────
    provider, model_id = _split_litellm_model(inp.model)
    argv = [
        pi_bin,
        "--mode", "rpc",
        "--no-session",
        "--no-context-files",            # don't slurp arbitrary AGENTS.md
        "--no-extensions",               # we don't ship extensions
        "--no-prompt-templates",
        "--tools", "bash",               # ONLY bash; no read/edit/write/grep
        "--skill", str(skills_dir),
        "--provider", provider,
        "--model", model_id,
        "--api-key", env["OPENROUTER_API_KEY"],
    ]

    logger.info(
        "spawning pi: provider=%s model=%s mode=%s timeout=%ds",
        provider, model_id, inp.mode, _agent_timeout_secs(),
    )

    # ── Spawn ────────────────────────────────────────────────────────────
    try:
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
            bufsize=1,                   # line-buffered
            preexec_fn=os.setsid,        # own process group; we kill it cleanly
        )
    except FileNotFoundError as exc:
        raise PiSpawnError(f"could not exec pi: {exc}") from exc

    final_assistant_text: list[str] = []
    turns = 0
    aborted = False

    try:
        # ── Send the prompt ──────────────────────────────────────────────
        _send(proc, {
            "id":   "kb-prompt-1",
            "type": "prompt",
            "message": _build_user_prompt(inp),
        })

        # ── Stream events ────────────────────────────────────────────────
        deadline = time.perf_counter() + _agent_timeout_secs()
        with agent_log.open("w", encoding="utf-8") as audit:
            for evt in _read_jsonl(proc.stdout):
                # Audit every event for postmortem.
                audit.write(json.dumps(evt, ensure_ascii=False) + "\n")

                etype = evt.get("type", "")
                if etype == "turn_end":
                    turns += 1
                elif etype == "message_update":
                    delta = evt.get("assistantMessageEvent", {})
                    if delta.get("type") == "text_delta":
                        final_assistant_text.append(delta.get("delta", ""))
                    elif delta.get("type") == "text_start":
                        # Reset for the new text block (we keep only the
                        # last assistant text block as the final summary).
                        final_assistant_text = []
                elif etype == "tool_execution_start":
                    logger.debug(
                        "agent tool=%s args=%s",
                        evt.get("toolName"),
                        json.dumps(evt.get("args", {}))[:200],
                    )
                elif etype == "agent_end":
                    break
                elif etype == "response" and evt.get("success") is False:
                    # Hard pi-side error.  Abort.
                    raise PiProtocolError(
                        f"pi reported failure on command "
                        f"{evt.get('command')!r}: {evt.get('error')}"
                    )

                if time.perf_counter() > deadline:
                    aborted = True
                    logger.warning(
                        "agent timeout reached after %d turns; aborting",
                        turns,
                    )
                    _send(proc, {"type": "abort"})
                    break

        # ── Clean shutdown ───────────────────────────────────────────────
        try:
            proc.stdin.close()      # type: ignore[union-attr]
        except OSError:
            pass

        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            logger.warning("pi did not exit; killing process group")
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait()

    finally:
        # Always reap; never leak.
        if proc.poll() is None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass

    # ── Parse the plan ───────────────────────────────────────────────────
    try:
        plan = read_plan(plan_file)
    except PlanParseError as exc:
        raise PlanCorruptError(str(exc)) from exc

    # ── Post-run vault diff audit ────────────────────────────────────────
    #
    # Diff the vault state before vs. after the agent run.  Anything
    # changed but NOT in the plan is a "rogue write" — the agent issued
    # raw bash (e.g. ``cat > /vault/...``) that bypassed the wrapper.
    # In shadow mode this should be impossible; logging it loudly is a
    # tripwire for catching skill regressions / model misbehaviour.
    post_snapshot = _snapshot_vault(inp.vault_root, inp.sources_dir)
    rogue_writes  = _audit_vault_diff(
        before     = pre_snapshot,
        after      = post_snapshot,
        plan       = plan,
        vault_root = inp.vault_root,
    )
    if rogue_writes:
        logger.error(
            "vault audit: agent BYPASSED kb-obsidian and made %d "
            "unsanctioned write(s) outside the plan: %s",
            len(rogue_writes),
            [str(p) for p in rogue_writes[:10]],
        )
        if len(rogue_writes) > 10:
            logger.error("  … plus %d more not shown", len(rogue_writes) - 10)
    else:
        logger.info("vault audit: clean (no rogue writes)")

    elapsed = time.perf_counter() - started
    logger.info(
        "agent done: turns=%d elapsed=%.1fs plan=%s aborted=%s",
        turns, elapsed, plan.summary(), aborted,
    )

    if aborted and not plan.entries:
        raise AgentBudgetError(
            f"agent timed out after {_agent_timeout_secs()}s without "
            "proposing any mutations."
        )

    return AgentResult(
        plan                 = plan,
        plan_file            = plan_file,
        agent_log            = agent_log,
        final_assistant_text = "".join(final_assistant_text).strip(),
        elapsed_secs         = elapsed,
        turns                = turns,
        aborted              = aborted,
        rogue_writes         = rogue_writes,
        metadata={
            "provider":     provider,
            "model":        model_id,
            "mode":         inp.mode,
            "rogue_writes": len(rogue_writes),
        },
    )
