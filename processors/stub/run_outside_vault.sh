#!/usr/bin/env bash
# processors/stub/run_outside_vault.sh
# Stub processor that returns an output path OUTSIDE vault_root.
# Used to test scenario 6: output validator rejects paths outside the vault.
#
# The output file is written to /tmp so it actually exists on disk
# (canonicalize requires the path to exist).
#
# Exit code: 0  (processor claims success — validator must catch the violation)

set -euo pipefail

# ── 1. Read full stdin ────────────────────────────────────────────────────────
INPUT_JSON="$(cat)"

# ── 2. Extract fields ─────────────────────────────────────────────────────────
if command -v jq >/dev/null 2>&1; then
    WORK_DIR="$(printf '%s' "$INPUT_JSON" | jq -r '.work_dir')"
    JOB_ID="$(  printf '%s' "$INPUT_JSON" | jq -r '.job_id')"
else
    WORK_DIR="$(python3 -c "import sys,json; d=json.load(sys.stdin); print(d['work_dir'])"   <<< "$INPUT_JSON")"
    JOB_ID="$(  python3 -c "import sys,json; d=json.load(sys.stdin); print(d['job_id'])"     <<< "$INPUT_JSON")"
fi

mkdir -p "${WORK_DIR}"

# ── 3. Write output to a path outside vault_root ──────────────────────────────
# /tmp is always outside any vault in a temp directory.
OUTPUT_PATH="/tmp/kb_outside_vault_test_${JOB_ID}_$$.md"
printf '# Outside-vault test output\nJob: %s\n' "${JOB_ID}" > "${OUTPUT_PATH}"

OUTPUT_BYTES="$(wc -c < "${OUTPUT_PATH}" | tr -d ' ')"

echo "[stub-outside-vault] wrote bad output to ${OUTPUT_PATH}"
echo "[stub-outside-vault] this path is outside vault_root — validator must reject it"

# ── 4. Emit 'ok' JSON pointing to the bad path ───────────────────────────────
# IMPORTANT: This MUST be the last line of stdout.
printf '{"status":"ok","outputs":[{"path":"%s","kind":"markdown","bytes":%s}],"metadata":{"processor":"stub-outside-vault"}}\n' \
    "${OUTPUT_PATH}" "${OUTPUT_BYTES}"
