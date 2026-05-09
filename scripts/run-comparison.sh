#!/usr/bin/env bash
# Runs a focused profile set for a single framework, saving results.
# Used by the trillium baseline-vs-after comparison.
#
# Usage: ./scripts/run-comparison.sh <framework> [profile1 profile2 ...]
#   default profiles: json json-comp async-db static static-h2
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Adapt sidecar pinning to a small (8-core) box. Harness defaults to "0,64"
# (the c7i SMT-pair convention) which docker rejects when no core 64 exists.
export REDIS_CPUSET="${REDIS_CPUSET:-0}"
export PG_CPUSET="${PG_CPUSET:-1}"
# 8-core layout for gateway / production-stack composes:
#   proxy on 0-1, server on 2-5, leaving 6-7 for the load gen.
# (compose files now use ${PROXY_CPUSET}/${SERVER_CPUSET} substitution; the
#  hardcoded 64-core defaults remain when these aren't set.)
export PROXY_CPUSET="${PROXY_CPUSET:-0-1}"
export SERVER_CPUSET="${SERVER_CPUSET:-2-5}"

# We installed all load-gen images via docker — no native binaries on PATH.
export LOADGEN_DOCKER="${LOADGEN_DOCKER:-true}"

FRAMEWORK="${1:?framework required}"
shift
PROFILES=("$@")
if [ ${#PROFILES[@]} -eq 0 ]; then
    PROFILES=(json json-comp async-db static static-h2)
fi

for p in "${PROFILES[@]}"; do
    echo "============================================================"
    echo "  $FRAMEWORK / $p"
    echo "============================================================"
    "$SCRIPT_DIR/benchmark.sh" "$FRAMEWORK" "$p" --save 2>&1 | tail -40 || {
        echo "[run-comparison] $FRAMEWORK $p exited non-zero, continuing"
    }
done

echo "[run-comparison] done: $FRAMEWORK ($(IFS=,; echo "${PROFILES[*]}"))"
