#!/usr/bin/env bash
# processors/stub/run_error.sh
# Stub processor that always returns an error JSON response.
# Used to test the worker's error handling and retry logic.
#
# Usage: <JSON on stdin> | run_error.sh
# Exit code: 1

set -euo pipefail

# Drain stdin (required by contract — processor must read its input)
cat > /dev/null

# Emit error JSON on stdout (last line)
printf '{"status":"error","error":"stub error for testing","retryable":true,"metadata":{}}\n'

exit 1
