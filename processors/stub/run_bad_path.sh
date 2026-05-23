#!/usr/bin/env bash
# processors/stub/run_bad_path.sh
# Stub processor that intentionally writes its output INSIDE sources_dir.
# Used to test that the output validator catches invariant violations.
#
# The worker's validate_outputs() must reject the path returned here because
# it violates the core invariant: outputs MUST NOT reside inside sources_dir.
#
# Usage: <JSON on stdin> | run_bad_path.sh <input_path> <work_dir>
# Exit code: 0 (processor claims success — validator must catch the violation)

set -euo pipefail

# ── 1. Read full stdin ────────────────────────────────────────────────────────
INPUT_JSON="$(cat)"

# ── 2. Extract fields ─────────────────────────────────────────────────────────
if command -v jq >/dev/null 2>&1; then
    INPUT_PATH="$(  printf '%s' "$INPUT_JSON" | jq -r '.input_path')"
    SOURCES_DIR="$( printf '%s' "$INPUT_JSON" | jq -r '.sources_dir')"
    WORK_DIR="$(    printf '%s' "$INPUT_JSON" | jq -r '.work_dir')"
    CONTENT_HASH="$(printf '%s' "$INPUT_JSON" | jq -r '.content_hash')"
else
    _py() { python3 -c "import sys,json; d=json.load(sys.stdin); print(d['$1'])" <<< "$INPUT_JSON"; }
    INPUT_PATH="$(_py input_path)"
    SOURCES_DIR="$(_py sources_dir)"
    WORK_DIR="$(   _py work_dir)"
    CONTENT_HASH="$(_py content_hash)"
fi

echo "[stub-bad-path] intentionally writing output inside sources_dir=${SOURCES_DIR}"

# ── 3. Create work_dir if needed ─────────────────────────────────────────────
mkdir -p "${WORK_DIR}"
mkdir -p "${SOURCES_DIR}"

# ── 4. Derive a bad output path (inside sources_dir — INVARIANT VIOLATION) ───
STEM="$(basename "${INPUT_PATH%.*}")"
BAD_OUTPUT_PATH="${SOURCES_DIR}/bad_output_${STEM}.md"

# ── 5. Write the bad output atomically ───────────────────────────────────────
TEMP_FILE="$(mktemp "${WORK_DIR}/bad_output.XXXXXX")"
cat > "${TEMP_FILE}" <<EOF
# BAD OUTPUT: This file was written inside sources_dir on purpose.
# The output validator should catch and reject this path.
Source: ${INPUT_PATH}
Hash: ${CONTENT_HASH}
EOF
mv "${TEMP_FILE}" "${BAD_OUTPUT_PATH}"

echo "[stub-bad-path] wrote bad output to ${BAD_OUTPUT_PATH}"

# ── 6. Compute size ───────────────────────────────────────────────────────────
if stat -f %z "${BAD_OUTPUT_PATH}" >/dev/null 2>&1; then
    OUTPUT_BYTES="$(stat -f %z "${BAD_OUTPUT_PATH}")"
else
    OUTPUT_BYTES="$(stat -c %s "${BAD_OUTPUT_PATH}" 2>/dev/null || wc -c < "${BAD_OUTPUT_PATH}" | tr -d ' ')"
fi

# ── 7. Return 'ok' JSON pointing to the bad path (validator must reject it) ──
# IMPORTANT: This MUST be the last line of stdout.
printf '{"status":"ok","outputs":[{"path":"%s","kind":"markdown","bytes":%s}],"metadata":{"processor":"stub-bad-path","version":"0.1"}}\n' \
    "${BAD_OUTPUT_PATH}" "${OUTPUT_BYTES}"
