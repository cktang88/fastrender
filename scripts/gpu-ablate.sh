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
# alllights = the frame before trimLights (dark padding lights in every lit fragment); none = what ships now.
# name@N: the row `name` with FASTRENDER_V3_DEBUG=N (src/mac_three.rs: 512 discarding opaque draws in state order
# instead of last and nearest first, 1024 no discarding opaque draws, 2048 instanced draws whole (not only the runs of instances in view), 4 no instanced
# draws, 64 no textures bound)
rows="alllights none none@2048 none@512 none@1024 none@4 none@64 lights0 nopoint nolights notransparent noenv nomaps lambert basic notransparent,noenv"
for a in $rows; do
  echo "== $a"
  FASTRENDER_V3_DEBUG="$( [ "${a#*@}" != "$a" ] && echo "${a#*@}" || echo 0 )" FASTRENDER_ABLATE="${a%@*}" FASTRENDER_ASSETS="$game/public" ./target/release/fastrender bench --three js/dist/world-objects.js \
    --scale 1 --upscale bilinear --size "$size" --frames "$frames" --warmup 60 --gpu-passes 1 > "$out/$a.log" 2>&1
  grep -E "gpu passes|\[ablate\]|fps|draws" "$out/$a.log" | tail -6
  # what the frame asks of the GPU (js/gpu-census.js): lights in three's light state, draws and fragment size per program
  case "$a" in alllights|none) grep -F "[census]" "$out/$a.log";; esac
done
echo "logs: $out (commit and push this folder)"
