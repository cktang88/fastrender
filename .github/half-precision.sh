#!/bin/bash
# .github/half-precision.sh: WP10, three's lighting functions in 16-bit float (src/xlat_half.rs, --half brdf|lit|all;
# docs/notes/half-precision.md). Per mode: every fragment program still compiles with Apple's Metal compiler, the
# goldens (MAE vs the browser reference, and vs this run's own float frame), and GPU ms (interleaved rounds).
# The runner's GPU is paravirtual: its ms are not an Air's; the picture and the compile results are the point.
# Writes half-precision.txt (annotated by the workflow). Exit 1 when a half program fails to compile or a run fails.
B=./target/release/fastrender
out=half-precision.txt
: > $out
fail=0
MODES="off brdf lit all"
WORLD=0
if [ -s js/dist/world-objects.js ] && [ -d ../3d-world-objects/public ]; then WORLD=1; export FASTRENDER_ASSETS="$PWD/../3d-world-objects/public"; fi
mae() { node -e "try{console.log(JSON.parse(require('fs').readFileSync('$1','utf8')).mae)}catch(e){console.log('n/a')}"; }

echo "## compile (xcrun metal -std=metal3.0, fragment stages)" >> $out
for m in brdf lit all; do
  for set in tests/fixtures/xlat/programs city-programs wo-programs; do
    [ -d "$set" ] || continue
    d=half-xlat-$m-$(basename $set)
    $B xlat-dump "$set" "$d" --half $m > "$d.txt" 2>&1 || true
    n=0; bad=0; halfn=0
    for f in "$d"/*.frag.metal; do
      [ -f "$f" ] || continue
      n=$((n+1)); grep -q "half" "$f" && halfn=$((halfn+1))
      xcrun -sdk macosx metal -std=metal3.0 -c "$f" -o /tmp/half.air >> half-metal.txt 2>&1 || { bad=$((bad+1)); echo "METAL FAIL $f" >> half-metal.txt; }
    done
    echo "$m $(basename $set): $n fragment programs, $halfn with half, $bad fail to compile; $(grep -c '^FAIL' "$d.txt") fail to translate; $(grep -c 'stays 32-bit' "$d.txt") refused by the pass" >> $out
    [ $bad = 0 ] || fail=1
  done
done
[ -f half-metal.txt ] && grep -B1 -A6 "error:" half-metal.txt | head -60 > half-metal-errors.txt

echo "## goldens (MAE 0-255: vs the browser reference | vs this run's float frame)" >> $out
for m in $MODES; do
  rc=0; $B bench --three js/dist/three-city.js --programs three --time 1 --scale 1 --upscale bilinear --size 1280x800 --frames 3 --warmup 2 --half $m --shot half-city-$m.png > half-city-$m.txt 2>&1 || rc=$?
  node js/golden/diff.mjs js/golden/ref-t1.png half-city-$m.png half-diff-city-$m.png > half-city-$m.json 2>/dev/null
  node js/golden/diff.mjs half-city-off.png half-city-$m.png half-self-city-$m.png > half-city-self-$m.json 2>/dev/null
  echo "city  $m: $(mae half-city-$m.json) | $(mae half-city-self-$m.json) (exit $rc)" >> $out
  [ $rc = 0 ] || { fail=1; tail -4 half-city-$m.txt | cut -c1-300 >> $out; }
  if [ $WORLD = 1 ]; then
    rc=0; $B bench --three js/dist/world-objects.js --scale 1 --upscale bilinear --size 1280x800 --frames 3 --warmup 11 --half $m --shot half-world-$m.png > half-world-$m.txt 2>&1 || rc=$?
    node js/golden/diff.mjs js/golden/world-objects-ref.png half-world-$m.png half-diff-world-$m.png > half-world-$m.json 2>/dev/null
    node js/golden/diff.mjs half-world-off.png half-world-$m.png half-self-world-$m.png > half-world-self-$m.json 2>/dev/null
    echo "world $m: $(mae half-world-$m.json) | $(mae half-world-self-$m.json) (exit $rc)" >> $out
    [ $rc = 0 ] || { fail=1; tail -4 half-world-$m.txt | cut -c1-300 >> $out; }
  fi
done

echo "## GPU ms avg, main pass | frame (3 interleaved rounds; paravirtual GPU)" >> $out
gpu() { # <label> <mode> <args...>
  local label=$1 m=$2; shift 2
  $B bench "$@" --half $m --gpu-passes 1 > half-gpu.txt 2>&1 || { echo "$label $m: FAIL" >> $out; fail=1; return; }
  echo "$label $m: $(grep '^gpu passes' half-gpu.txt | sed -E 's/.*main ([0-9.]+).*frame ([0-9.]+).*/\1 | \2/')" >> $out
}
for round in 1 2 3; do
  for m in $MODES; do
    gpu "city  r$round" $m --three js/dist/three-city.js --scale 1 --upscale bilinear --size 1280x800 --frames 200 --warmup 20
    [ $WORLD = 1 ] && gpu "world r$round" $m --three js/dist/world-objects.js --scale 1 --upscale bilinear --size 1280x800 --frames 30 --warmup 60
  done
done
exit $fail
