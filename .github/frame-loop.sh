#!/bin/bash
# .github/frame-loop.sh: the frame loop measured on the runner (docs/LOG.md "frame loop"): frames in flight, slot and
# drawable waits, per-pass GPU ms (--gpu-passes 1), the shared-event fence (--fence event), the windowed app's pacing
# (CAMetalDisplayLink vs the nextDrawable loop), framebufferOnly, thread QoS, the thermal policy (env override).
# Writes frame-loop.txt (annotated by the workflow) and frame-loop-*.png. Timings are a paravirtual GPU's: the
# counts (frames in flight, stalls) and the split between passes are the result, not the absolute ms.
# Exit 1 when a run fails or the per-pass / shared-event frames differ from the plain frame.
B=./target/release/fastrender
out=frame-loop.txt
: > $out
fail=0
# run <name> <seconds limit> <command...>: one section of the report
run() {
  local name=$1 limit=$2; shift 2
  echo "## $name" >> $out
  perl -e 'alarm shift; exec @ARGV' "$limit" "$@" > fl-run.txt 2>&1
  local rc=$?
  grep -E "^frame loop:|^gpu passes|^windowed:|^pacing:|^power:|^gpu timers:|^render thread QoS|^\| [a-z]|three frame cost|GPU error|panicked" fl-run.txt | cut -c1-420 >> $out
  if [ $rc != 0 ]; then echo "FAIL exit $rc" >> $out; tail -5 fl-run.txt | cut -c1-300 >> $out; fail=1; fi
}
CITY="--path forward --upscale metalfx --scale 0.5 --size 1280x800 --frames 120 --warmup 10 --props 60000 --lights 16"
THREE="--three js/dist/three-city.js --size 1280x800 --frames 120 --warmup 10"

run "probe" 60 $B probe
run "city, MetalFX 0.5: one command buffer" 120 $B bench $CITY
run "city, MetalFX 0.5: per-pass GPU ms" 120 $B bench $CITY --gpu-passes 1
run "city, MetalFX 0.5: per-pass, shared-event fence" 120 $B bench $CITY --gpu-passes 1 --fence event
run "three-city, scale 1: one command buffer" 180 $B bench $THREE --upscale bilinear --scale 1
run "three-city, scale 1: per-pass GPU ms" 180 $B bench $THREE --upscale bilinear --scale 1 --gpu-passes 1
run "three-city, scale 1: shared-event fence" 180 $B bench $THREE --upscale bilinear --scale 1 --fence event
run "three-city, MetalFX 0.5: per-pass GPU ms" 180 $B bench $THREE --upscale metalfx --scale 0.5 --gpu-passes 1
if [ -s js/dist/ibl-bloom.js ]; then
  run "IBL + bloom (render targets), scale 1: per-pass GPU ms" 180 $B bench --three js/dist/ibl-bloom.js --size 960x600 --frames 120 --warmup 10 --upscale bilinear --scale 1 --gpu-passes 1
fi
if [ -s js/dist/world-objects.js ] && [ -d ../3d-world-objects/public ]; then
  FASTRENDER_ASSETS="$PWD/../3d-world-objects/public" run "3d-world-objects, scale 1, frames 61-90: per-pass GPU ms" 300 $B bench --three js/dist/world-objects.js --scale 1 --upscale bilinear --size 1280x800 --frames 30 --warmup 60 --gpu-passes 1
fi

# the split frame and the event-fenced frame are the same picture as the plain one (golden time, same diff tool)
G="--three js/dist/three-city.js --programs three --time 1 --scale 1 --upscale bilinear --size 1280x800 --frames 3 --warmup 2"
$B bench $G --shot frame-loop-plain.png > /dev/null 2>&1
$B bench $G --gpu-passes 1 --shot frame-loop-passes.png > /dev/null 2>&1
$B bench $G --fence event --shot frame-loop-event.png > /dev/null 2>&1
echo "## same picture (MAE vs the one-command-buffer frame; vs the browser golden)" >> $out
for v in passes event; do
  d=$(node js/golden/diff.mjs frame-loop-plain.png frame-loop-$v.png frame-loop-diff-$v.png 2>&1 | cut -c1-200)
  echo "$v vs plain: $d" >> $out
  echo "$d" | grep -Eq '"mae":0(\.0[0-4])?[,}]' || { echo "FAIL $v differs from the plain frame" >> $out; fail=1; }
done
echo "per-pass vs browser golden: $(node js/golden/diff.mjs js/golden/ref-t1.png frame-loop-passes.png frame-loop-diff-golden.png 2>&1 | cut -c1-200)" >> $out

# windowed app (needs the runner's window server; a hang is cut by the alarm)
W="run --three js/dist/three-city.js --upscale bilinear --scale 1 --size 1280x800 --run-seconds 6 --gpu-passes 1"
run "windowed, vsync on: CAMetalDisplayLink (falls back when the link never fires)" 90 $B $W
run "windowed, vsync on: nextDrawable loop" 90 $B $W --display-link 0
run "windowed, vsync off (uncapped)" 90 $B $W --vsync 0
run "windowed, MetalFX 0.5 (drawable not framebufferOnly), shared-event fence" 90 $B run --three js/dist/three-city.js --upscale metalfx --scale 0.5 --size 1280x800 --run-seconds 6 --fence event
FASTRENDER_THERMAL=serious run "windowed, thermal state forced to serious (scale <= 0.75, 30 fps cap)" 90 $B $W
FASTRENDER_THERMAL=critical FASTRENDER_LOW_POWER=1 run "windowed, critical + Low Power Mode, display link off" 90 $B $W --display-link 0
cat $out
exit $fail
