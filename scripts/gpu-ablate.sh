#!/usr/bin/env bash
# GPU-cost attribution for the real game on THIS Mac: the main pass with one feature switched off at a time.
# Each row CHANGES the picture on purpose (FASTRENDER_ABLATE, js/examples/world-objects.three.mjs); only ms matter.
#   scripts/gpu-ablate.sh [size=1280x800] [frames=180]
# Needs the game copy that host/electron/scripts/bench-3way.sh fetches (host/electron/.bench/3d-world-objects).
set -u
cd "$(dirname "$0")/.."
size="${1:-1280x800}"; frames="${2:-180}"
game="host/electron/.bench/3d-world-objects"
[ -d "$game" ] || { echo "missing $game: run host/electron/scripts/bench-3way.sh --quick once first"; exit 1; }
cargo build --release -q || exit 1
(cd js && npm ci --silent) >/dev/null 2>&1 || true
node js/examples/world-objects.build.mjs "$game" js/dist/world-objects.js --target jsc --three three >/dev/null || exit 1
out="bench-results/$(date +%F)-gpu-ablate-$(sysctl -n hw.model 2>/dev/null || echo mac)"; mkdir -p "$out"
rows="none lights0 nopoint nolights notransparent noenv nomaps lambert basic lights0,notransparent"
for a in $rows; do
  echo "== $a"
  FASTRENDER_ABLATE="$a" FASTRENDER_ASSETS="$game/public" ./target/release/fastrender bench --three js/dist/world-objects.js \
    --scale 1 --upscale bilinear --size "$size" --frames "$frames" --warmup 60 --gpu-passes 1 > "$out/$a.log" 2>&1
  grep -E "gpu passes|\[ablate\]|fps|draws" "$out/$a.log" | tail -6
done
echo "logs: $out (commit and push this folder)"
