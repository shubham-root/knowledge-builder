#!/bin/bash
# Entry point for the knowledge-builder processor
# Usage: run.sh <input_path> <work_dir>
# Reads JSON from stdin, writes JSON result to stdout (last line)
DIR="$(cd "$(dirname "$0")" && pwd)"
exec python3 -m kb_processor "$@" < /dev/stdin
