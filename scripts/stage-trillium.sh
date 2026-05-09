#!/usr/bin/env bash
# Copy the local trillium workspace into each variant's _trillium/ so
# `docker build`'s build context (frameworks/<variant>/) can include it
# for [patch.crates-io] path overrides.
#
# Override TRILLIUM_SRC to point at a different clone.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TRILLIUM_SRC="${TRILLIUM_SRC:-$ROOT_DIR/../trillium}"

if [ ! -f "$TRILLIUM_SRC/Cargo.toml" ]; then
    echo "stage-trillium: $TRILLIUM_SRC does not look like a trillium workspace" >&2
    exit 1
fi

for variant in trillium trillium-tuned; do
    dest="$ROOT_DIR/frameworks/$variant/_trillium"
    echo "[stage-trillium] $TRILLIUM_SRC -> $dest"
    mkdir -p "$dest"
    rsync -a --delete --delete-excluded \
        --exclude='target/' \
        --exclude='.git/' \
        --exclude='Cargo.lock' \
        --exclude='_trillium/' \
        --exclude='/bench/' \
        --exclude='/fuzz/' \
        --exclude='/controllers/' \
        "$TRILLIUM_SRC/" "$dest/"
done

echo "[stage-trillium] done"
