#!/usr/bin/env bash
# Soak runner: builds/starts this lane's stack with server bug reports enabled, runs the
# headless-client scenarios with N parallel bot accounts for a given duration, and writes a
# dashboard. Run from inside a lane worktree; MySQL stays up. See rust/tools/bots_soak.py --help.
#
#   Scripts/bots-soak.sh --minutes 30 --bots 8
#   Scripts/bots-soak.sh --minutes 5 --bots 3 --seed 7 --scenarios full,combat,questitem
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
PY=${PYTHON:-python}
if ! command -v "$PY" >/dev/null 2>&1; then
  PY=py
fi
exec "$PY" "$REPO/rust/tools/bots_soak.py" "$@"
