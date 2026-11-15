#!/bin/bash
# JS cost of the real game (3d-world-objects) in the JavaScriptCore host, per engine configuration, on this runner:
# `fastrender jsbench` (system JavaScriptCore.framework, no Metal) under JSC_* options and code-signing variants, vs
# the same bundle in node (V8) and bun (its own JavaScriptCore). Non-gating; results go to annotations.
# docs/research/jsc-vs-v8.md has the reading of the numbers.
#   .github/jsc-profile.sh <bundle.js> <game dir>
set -u
BUNDLE=$1; GAME=$2
B=./target/release/fastrender
export FASTRENDER_ASSETS="$GAME/public"
PARAMS='{"boxes": 0, "lights": 0, "programs": "three", "width": 1280, "height": 800, "prof": 1}'
OUT=jsc-profile; mkdir -p $OUT
SUM=$OUT/summary.txt; : > $SUM

{
  echo "runner: $(sw_vers -productVersion) $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cpus, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB"
  echo "JavaScriptCore.framework $(defaults read /System/Library/Frameworks/JavaScriptCore.framework/Resources/Info CFBundleVersion 2>/dev/null)"
  echo "binary signature: $(codesign -dvv $B 2>&1 | grep -E 'flags|Signature|TeamIdentifier' | tr '\n' ' ')"
  echo "binary entitlements: $(codesign -d --entitlements - $B 2>&1 | grep -v Executable | tr '\n' ' ' | cut -c1-200)"
} | tee -a $SUM

# run <name> <binary> [ENV=VALUE ...]: one jsbench run; the summary line carries the result + the probe
run() {
  local name=$1 bin=$2; shift 2
  local t0=$(date +%s)
  env "$@" "$bin" jsbench "$BUNDLE" --warmup 71 --frames 30 --params "$PARAMS" > $OUT/$name.out 2> $OUT/$name.err
  local rc=$?
  echo "[$name] exit $rc, $(( $(date +%s) - t0 )) s wall: $(grep -h 'jsbench result' $OUT/$name.out | sed 's/jsbench result: //' | cut -c1-260)" | tee -a $SUM
  echo "[$name] $(grep -h 'jsbench probe\|bundle evaluated' $OUT/$name.out | tr '\n' ' ' | cut -c1-260)" | tee -a $SUM
  [ $rc -ne 0 ] && tail -3 $OUT/$name.err | cut -c1-260 | tee -a $SUM
  return 0
}

run default $B
run default-again $B
run dollarvm $B JSC_useDollarVM=1
run no-jit $B JSC_useJIT=0
run no-dfg $B JSC_useDFGJIT=0
run no-ftl $B JSC_useFTLJIT=0
run no-concurrent-jit $B JSC_useConcurrentJIT=0
run no-concurrent-gc $B JSC_useConcurrentGC=0
run big-heap $B JSC_smallHeapSize=268435456 JSC_minHeapSize=268435456

# which options the framework actually runs with (does it read JSC_* at all, are the tiers on)
run dump-options $B JSC_dumpOptions=1
grep -hE '(useJIT|useBaselineJIT|useDFGJIT|useFTLJIT|useConcurrentJIT|useLLInt|useJITCage|useWebAssembly|useBBQJIT|useOMGJIT|useWasmLLInt|useConcurrentGC|useGenerationalGC|numberOfDFGCompilerThreads|numberOfFTLCompilerThreads|numberOfGCMarkers|priorityDeltaOfDFGCompilerThreads|priorityDeltaOfFTLCompilerThreads|smallHeapSize|largeHeapSize|thresholdForOptimizeAfterWarmUp|thresholdForFTLOptimizeAfterWarmUp|useSamplingProfiler|useHandlerIC|forceRAMSize)=' $OUT/dump-options.out $OUT/dump-options.err | sed 's/^[^:]*://' | tr -s ' ' | tr '\n' ';' | cut -c1-1500 > $OUT/options.txt
echo "options: $(cat $OUT/options.txt)" | tee -a $SUM

# what compiles, per tier
run compile-times $B JSC_reportCompileTimes=1
echo "compiles by tier: $(cat $OUT/compile-times.err $OUT/compile-times.out | grep -oE 'with (Baseline|DFG|FTL|B3|Air|LLInt)[A-Za-z]*|using [A-Za-z]+Mode' | sort | uniq -c | tr '\n' ';')" | tee -a $SUM
cat $OUT/compile-times.err $OUT/compile-times.out | grep -E 'Optimized|Compiled' | head -3 | cut -c1-200 | tee -a $SUM

# collections during the run
run log-gc $B JSC_logGC=1
echo "GC log lines: $(cat $OUT/log-gc.err $OUT/log-gc.out | grep -c 'GC')" | tee -a $SUM
cat $OUT/log-gc.err $OUT/log-gc.out | grep 'GC' | tail -4 | cut -c1-280 | tee -a $SUM

# JavaScriptCore's own sampling profiler: top functions + the tier each sample ran in
mkdir -p $OUT/sampling
run sampling $B JSC_useSamplingProfiler=1 JSC_samplingProfilerPath=$PWD/$OUT/sampling JSC_samplingProfilerTopFunctionsCount=40 JSC_samplingProfilerTopBytecodesCount=40 JSC_sampleInterval=500
ls -la $OUT/sampling | tail -5 | tee -a $SUM
grep -A45 -i 'top functions\|Sampling rate' $OUT/sampling.err $OUT/sampling.out | head -120 | cut -c1-200 > $OUT/sampling-top.txt
grep -B2 -A12 -i 'tier breakdown' $OUT/sampling.err $OUT/sampling.out | head -40 | cut -c1-200 >> $OUT/sampling-top.txt

# code signing: ad hoc (what cargo's linker makes), hardened runtime without and with the JIT entitlement
cp $B $OUT/fr-hardened && codesign -f -s - --options runtime $OUT/fr-hardened 2>&1 | tail -1
run signed-hardened-no-entitlement $OUT/fr-hardened
cp $B $OUT/fr-hardened-jit && codesign -f -s - --options runtime --entitlements macos/fastrender.entitlements $OUT/fr-hardened-jit 2>&1 | tail -1
run signed-hardened-allow-jit $OUT/fr-hardened-jit
cp $B $OUT/fr-jit && codesign -f -s - --entitlements macos/fastrender.entitlements $OUT/fr-jit 2>&1 | tail -1
run signed-allow-jit $OUT/fr-jit

# the same bundle + polyfills + virtual timers in other engines on this machine (js/test/world-objects.jsc-prof.mjs)
other() {
  local name=$1; shift
  echo "[$name] $("$@" js/test/world-objects.jsc-prof.mjs "$BUNDLE" "$GAME" 2> $OUT/$name.err | tail -1 | cut -c1-420)" | tee -a $SUM
}
other node-v8 node
if npm install -g bun > $OUT/bun-install.txt 2>&1 && command -v bun > /dev/null; then
  other bun-jsc bun
  other bun-jsc-no-ftl env BUN_JSC_useFTLJIT=0 bun
  other bun-jsc-no-dfg env BUN_JSC_useDFGJIT=0 bun
  other bun-jsc-no-jit env BUN_JSC_useJIT=0 bun
else
  echo "[bun] not installed: $(tail -2 $OUT/bun-install.txt | tr '\n' ' ')" | tee -a $SUM
fi
# the game unbundled-for-node (real timers, no vm context): js/test/world-objects.node.mjs
(cd js && node test/world-objects.node.mjs "$GAME" --three three --frames 30 --json "$OLDPWD/$OUT/node.json" > "$OLDPWD/$OUT/node.txt" 2>&1)
echo "[node, ESM build, real timers] $(node -e "const r=require('./$OUT/node.json');console.log(JSON.stringify(r.perFrameMs))" 2>&1 | cut -c1-300)" | tee -a $SUM

# with Metal: the full host, same frames (JS line of `three frame cost`)
env -u MTL_DEBUG_LAYER $B bench --three "$BUNDLE" --scale 1 --upscale bilinear --size 1280x800 --frames 30 --warmup 71 > $OUT/bench.out 2>&1
echo "[bench --three, with Metal] $(grep -h 'three frame cost' $OUT/bench.out | tail -1 | cut -c1-200)" | tee -a $SUM

for f in default no-jit sampling; do grep -h 'jsbench frames' $OUT/$f.out | sed "s/^/[$f] /" | cut -c1-200; done > $OUT/windows.txt
