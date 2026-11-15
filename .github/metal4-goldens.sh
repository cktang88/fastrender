#!/bin/bash
# .github/metal4-goldens.sh [backend, default metal4]
# The selftests and the golden scenes through one backend (docs/notes/metal4-port.md), under Metal API validation.
# For the Metal 4 execute lane of metal4.yml: runs where `fastrender probe` reported the Metal4 family (a
# self-hosted Apple silicon runner; no hosted runner has it). Every scene must
#   - report `backend: <backend>` (a silent fallback to Metal 3 is a failure here),
#   - finish without a GPU error or an API-validation abort,
#   - match the browser reference within the same MAE bound the Metal 3 lane gates on (mac.yml; the hand-written
#     fs_three path is reported, not gated, there and here).
# Output: metal4-goldens.txt (one line per scene) and the shots / diffs, uploaded by the workflow.
set -u
backend=${1:-metal4}
bin=./target/release/fastrender
out=metal4-goldens.txt
: > "$out"
fail=0
export MTL_DEBUG_LAYER=1

bundle() { test -s "js/dist/$2" || node js/build.mjs "js/examples/$1" "js/dist/$2"; }
test -d js/node_modules || (cd js && npm ci --silent) || { echo "npm ci failed" | tee -a "$out"; exit 1; }
bundle three-city.entry.mjs three-city.js
bundle parity-scene.entry.mjs parity.js
bundle parity-scene-vsm.entry.mjs parity-vsm.js
bundle parity-scene-basic.entry.mjs parity-basic.js
bundle ibl-scene.entry.mjs ibl.js
bundle ibl-bloom.entry.mjs ibl-bloom.js

# the run used the backend that was asked for
on_backend() { grep -q "backend: $backend" "$1"; }

# selftest <name> <args...>: stage-by-stage diagnostics; the output must not be black
selftest() {
  local name=$1; shift
  local log="m4-selftest-$name.txt"
  "$bin" selftest --backend "$backend" "$@" > "$log" 2>&1; local rc=$?
  local why=""
  [ $rc -eq 0 ] || why="exit $rc"
  on_backend "$log" || why="$why not on $backend"
  grep "3 frame: output" "$log" | grep -qv "mean rgba = 0 0 0 0" || why="$why black output"
  grep -q "three-programs selftest: FAIL" "$log" && why="$why three-programs FAIL"
  if [ -z "$why" ]; then echo "PASS selftest $name" | tee -a "$out"; else echo "FAIL selftest $name:$why" | tee -a "$out"; fail=1; tail -n 15 "$log"; fi
}

# golden <name> <reference png> <MAE bound> <args...>: one frame at t=1, compared with the browser reference
golden() {
  local name=$1 ref=$2 bound=$3; shift 3
  local log="m4-golden-$name.txt" shot="m4-golden-$name.png"
  "$bin" bench --backend "$backend" "$@" --time 1 --scale 1 --upscale bilinear --frames 3 --warmup 2 --shot "$shot" > "$log" 2>&1; local rc=$?
  local why="" r="{}"
  [ $rc -eq 0 ] || why="exit $rc"
  on_backend "$log" || why="$why not on $backend"
  grep -q "GPU error" "$log" && why="$why GPU error"
  if [ -s "$shot" ]; then
    r=$(node js/golden/diff.mjs "$ref" "$shot" "m4-diff-$name.png" 2>&1) || why="$why diff failed"
    node -e "const r=JSON.parse(process.argv[1]);process.exit(r.mae<$bound?0:1)" "$r" 2>/dev/null || why="$why MAE not below $bound"
  else
    why="$why no screenshot"
  fi
  if [ -z "$why" ]; then echo "PASS golden $name $r (bound $bound)" | tee -a "$out"; else echo "FAIL golden $name $r:$why" | tee -a "$out"; fail=1; tail -n 15 "$log"; fi
  grep "three frame cost\|three programs:\|frame loop:" "$log" | cut -c1-400 >> "$out"
}

selftest city-native --path forward --upscale bilinear --scale 1 --size 640x400 --props 20000
selftest city-upscaled --path forward --upscale bilinear --scale 0.5 --size 640x400 --props 20000
selftest three-city --three js/dist/three-city.js --upscale bilinear --scale 1 --size 640x400
golden three-native js/golden/ref-t1.png 255 --three js/dist/three-city.js --programs native --size 1280x800
golden three-city js/golden/ref-t1.png 1.0 --three js/dist/three-city.js --programs three --size 1280x800
golden parity js/golden/ref-parity-t1.png 1.5 --three js/dist/parity.js --size 960x600
golden parity-vsm js/golden/ref-parity-vsm-t1.png 1.5 --three js/dist/parity-vsm.js --size 960x600
golden parity-basic js/golden/ref-parity-basic-t1.png 1.5 --three js/dist/parity-basic.js --size 960x600
golden ibl js/golden/ref-ibl-t1.png 1.5 --three js/dist/ibl.js --size 960x600
golden ibl-bloom js/golden/ref-ibl-bloom-t1.png 1.5 --three js/dist/ibl-bloom.js --size 960x600

echo "backend $backend: $(grep -c '^PASS' "$out") passed, $(grep -c '^FAIL' "$out") failed" | tee -a "$out"
exit $fail
