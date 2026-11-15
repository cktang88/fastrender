#!/usr/bin/env bash
# WP10 A/B on THIS Mac: the real game's main pass with three's lighting functions in 16-bit float
# (src/xlat_half.rs, docs/notes/half-precision.md). Rows: off (today), brdf (picture-safe terms), lit (the whole
# per-light evaluation; highlights on very smooth surfaces differ), all (every fragment function: a ceiling, the
# picture is wrong on purpose). Interleaved rounds, so thermal drift hits every row alike.
#   scripts/half-ab.sh [size=2940x1912] [frames=180] [rounds=3]
# Needs the game copy that host/electron/scripts/bench-3way.sh fetches (host/electron/.bench/3d-world-objects).
set -u
cd "$(dirname "$0")/.."
size="${1:-2940x1912}"; frames="${2:-180}"; rounds="${3:-3}"
game="host/electron/.bench/3d-world-objects"
[ -d "$game" ] || { echo "missing $game: run host/electron/scripts/bench-3way.sh --quick once first"; exit 1; }
cargo build --release -q || exit 1
(cd js && npm ci --silent) >/dev/null 2>&1 || true
node js/examples/world-objects.build.mjs "$game" js/dist/world-objects.js --target jsc --three three >/dev/null || exit 1
out="bench-results/$(date +%F)-half-ab-$(sysctl -n hw.model 2>/dev/null || echo mac)"; mkdir -p "$out"
for r in $(seq 1 "$rounds"); do
  for m in off brdf lit all; do
    FASTRENDER_ASSETS="$game/public" ./target/release/fastrender bench --three js/dist/world-objects.js \
      --scale 1 --upscale bilinear --size "$size" --frames "$frames" --warmup 60 --gpu-passes 1 --half "$m" \
      $( [ "$r" = 1 ] && echo "--shot $out/$m.png" ) > "$out/$m-r$r.log" 2>&1
    echo "round $r  half $m: $(grep '^gpu passes' "$out/$m-r$r.log" | sed -E 's/.*(main [0-9.]+).*(frame [0-9.]+).*/\1 ms, \2 ms/')  $(grep -E '^\| ' "$out/$m-r$r.log" | tail -1 | awk -F'|' '{print "fps" $3}')"
  done
done
echo "logs + one screenshot per mode: $out (commit and push this folder)"
