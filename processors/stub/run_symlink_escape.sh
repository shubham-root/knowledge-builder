#!/usr/bin/env bash
# processors/stub/run_symlink_escape.sh
# Stub processor that creates a symlink inside vault_root pointing to /tmp,
# then returns an output path through that symlink.
#
# After canonicalization the output path resolves to /tmp/... (or /private/tmp/...)
# which is outside vault_root.  The output validator must reject it.
#
# Used to test scenario 7: symlink-escaping output path → rejected.
#
# Exit code: 0  (processor claims success — validator must catch the violation)

set -euo pipefail

# ── 1. Read full stdin ────────────────────────────────────────────────────────
INPUT_JSON="$(cat)"

# ── 2. Extract fields ─────────────────────────────────────────────────────────
if command -v jq >/dev/null 2>&1; then
    VAULT_ROOT="$(printf '%s' "$INPUT_JSON" | jq -r '.vault_root')"
    WORK_DIR="$(  printf '%s' "$INPUT_JSON" | jq -r '.work_dir')"
    JOB_ID="$(    printf '%s' "$INPUT_JSON" | jq -r '.job_id')"
else
    VAULT_ROOT="$(python3 -c "import sys,json; d=json.load(sys.stdin); print(d['vault_root'])" <<< "$INPUT_JSON")"
    WORK_DIR="$(  python3 -c "import sys,json; d=json.load(sys.stdin); print(d['work_dir'])"   <<< "$INPUT_JSON")"
    JOB_ID="$(    python3 -c "import sys,json; d=json.load(sys.stdin); print(d['job_id'])"     <<< "$INPUT_JSON")"
fi

mkdir -p "${WORK_DIR}"

# ── 3. Create symlink inside vault that points to /tmp ────────────────────────
# The link name is unique to avoid collisions between concurrent tests.
LINK_NAME="kb_escape_link_${JOB_ID}_$$"
ESCAPE_LINK="${VAULT_ROOT}/${LINK_NAME}"

# Remove any stale link from a previous run.
rm -f "${ESCAPE_LINK}" 2>/dev/null || true
# /tmp on macOS is a symlink to /private/tmp; after canonicalize this resolves
# to /private/tmp which is outside any vault in a tempdir.
ln -sf "/tmp" "${ESCAPE_LINK}"

echo "[stub-symlink-escape] created ${ESCAPE_LINK} -> /tmp"

# ── 4. Write a file via the symlink (actually writes to /tmp) ─────────────────
OUTPUT_FILENAME="kb_symlink_escape_test_${JOB_ID}_$$.md"
OUTPUT_PATH="${ESCAPE_LINK}/${OUTPUT_FILENAME}"

printf '# Symlink escape test\nJob: %s\n' "${JOB_ID}" > "${OUTPUT_PATH}"
OUTPUT_BYTES="$(wc -c < "${OUTPUT_PATH}" | tr -d ' ')"

echo "[stub-symlink-escape] wrote via symlink to ${OUTPUT_PATH}"
echo "[stub-symlink-escape] canonicalized this resolves outside vault_root"

# ── 5. Emit 'ok' JSON — the path looks vault-internal but resolves outside ───
# IMPORTANT: This MUST be the last line of stdout.
printf '{"status":"ok","outputs":[{"path":"%s","kind":"markdown","bytes":%s}],"metadata":{"processor":"stub-symlink-escape"}}\n' \
    "${OUTPUT_PATH}" "${OUTPUT_BYTES}"
