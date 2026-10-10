#!/usr/bin/env bash
# CLI benchmark entry point; check every measured repetition.
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 bench/measure.py "$@"
