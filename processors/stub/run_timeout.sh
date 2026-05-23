#!/usr/bin/env bash
# processors/stub/run_timeout.sh
# Stub processor that sleeps forever, simulating a hung processor.
# Used to test the worker's timeout + SIGTERM/SIGKILL logic.
#
# Usage: <JSON on stdin> | run_timeout.sh
# Never exits on its own — must be killed by the caller.

set -euo pipefail

# Drain stdin so the caller's pipe write doesn't block
cat > /dev/null

# Log that we're intentionally hanging
echo "[stub-timeout] sleeping forever to simulate a hung processor..."

# Sleep forever (or until killed by the worker timeout mechanism)
sleep 86400
