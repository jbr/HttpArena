#!/usr/bin/env bash
# Side-by-side comparison of N frameworks from a single results tree.
# Usage: scripts/compare-frameworks.sh <results-dir> <fw1> <fw2> [fw3 ...]
set -euo pipefail

RESULTS_DIR="${1:?results dir required}"; shift
FRAMEWORKS=("$@")
if [ ${#FRAMEWORKS[@]} -lt 1 ]; then
    echo "Usage: $0 <results-dir> <fw1> [fw2 ...]" >&2; exit 1
fi

python3 - "$RESULTS_DIR" "${FRAMEWORKS[@]}" <<'PY'
import json, os, sys, glob

results_dir = sys.argv[1]
frameworks = sys.argv[2:]

# Walk results/<profile>/<conns>/<fw>.json
data = {}  # (profile, conns) -> {fw: dict}
for fw in frameworks:
    for fp in glob.glob(f"{results_dir}/*/*/{fw}.json"):
        rel = os.path.relpath(fp, results_dir)
        profile, conns, _ = rel.split("/")
        try:
            d = json.load(open(fp))
        except Exception:
            continue
        data.setdefault((profile, conns), {})[fw] = d

def fmt_rps(r):
    if r is None or r == 0: return "—"
    if r >= 1000: return f"{r:,}"
    return f"{r}"

def fmt(s):
    return s if s else "—"

# Header
hdr = "| profile/conns |"
sep = "|---|"
for fw in frameworks:
    hdr += f" {fw} RPS |"
    sep += "--:|"
print(hdr)
print(sep)

for k in sorted(data):
    profile, conns = k
    row = f"| {profile} {conns}c |"
    for fw in frameworks:
        d = data[k].get(fw, {})
        row += f" {fmt_rps(d.get('rps'))} |"
    print(row)

print()
print("### CPU / Memory")
print()
hdr = "| profile/conns |"
sep = "|---|"
for fw in frameworks:
    hdr += f" {fw} CPU/mem |"
    sep += "---|"
print(hdr); print(sep)
for k in sorted(data):
    profile, conns = k
    row = f"| {profile} {conns}c |"
    for fw in frameworks:
        d = data[k].get(fw, {})
        cpu = d.get('cpu','—')
        mem = d.get('memory','—')
        row += f" {cpu} / {mem} |"
    print(row)
PY
