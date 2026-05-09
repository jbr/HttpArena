#!/usr/bin/env bash
# Compare two HttpArena results trees for the same framework. Produces a
# markdown delta table (RPS, p99, CPU, memory).
#
# Usage:
#   scripts/compare-runs.sh <framework> <baseline-results-dir> [<after-results-dir>]
#
# Defaults:
#   after-results-dir = $ROOT_DIR/results
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

FRAMEWORK="${1:?framework required}"
BASELINE="${2:?baseline results dir required}"
AFTER="${3:-$ROOT_DIR/results}"

python3 - "$FRAMEWORK" "$BASELINE" "$AFTER" <<'PY'
import json, os, sys, glob

framework, baseline_dir, after_dir = sys.argv[1], sys.argv[2], sys.argv[3]

def load(root):
    out = {}
    for fp in glob.glob(f"{root}/*/*/{framework}.json"):
        rel = os.path.relpath(fp, root)
        profile, conns, _ = rel.split("/")
        try:
            out[(profile, conns)] = json.load(open(fp))
        except Exception as e:
            print(f"warn: failed to read {fp}: {e}", file=sys.stderr)
    return out

b = load(baseline_dir)
a = load(after_dir)
keys = sorted(set(b) | set(a))

def fmt(n):
    if n is None: return "—"
    if isinstance(n, str): return n
    return f"{n:,}" if abs(n) >= 1000 else f"{n:.1f}"

def delta_pct(old, new):
    if not old: return "—"
    return f"{(new-old)/old*100:+.1f}%"

print(f"# {framework}: baseline ({os.path.basename(os.path.dirname(baseline_dir+'/.'))}) vs after ({os.path.basename(os.path.dirname(after_dir+'/.'))})")
print()
print("| profile/conns | baseline RPS | after RPS | Δ RPS | baseline p99 | after p99 | baseline mem | after mem |")
print("|---|--:|--:|--:|--:|--:|--:|--:|")
for k in keys:
    bb = b.get(k, {})
    aa = a.get(k, {})
    profile, conns = k
    print(f"| {profile} {conns}c | "
          f"{fmt(bb.get('rps'))} | {fmt(aa.get('rps'))} | "
          f"{delta_pct(bb.get('rps') or 0, aa.get('rps') or 0)} | "
          f"{bb.get('p99_latency','—')} | {aa.get('p99_latency','—')} | "
          f"{bb.get('memory','—')} | {aa.get('memory','—')} |")
PY
