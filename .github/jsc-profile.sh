#!/bin/bash
# JS cost of the real game (3d-world-objects) in the JavaScriptCore host, per engine configuration, on this runner:
# `fastrender jsbench` (system JavaScriptCore.framework, no Metal) under JSC_* options and code-signing variants, vs
# the same bundle in node (V8) and bun (its own JavaScriptCore). Non-gating; each group's lines become one annotation.
# docs/research/jsc-vs-v8.md has the reading of the numbers.
#   .github/jsc-profile.sh <group: tiers|signing|engines|diagnostics|metal> <bundle.js> <game dir>
set -u
GROUP=$1; BUNDLE=$2; GAME=$3
B=./target/release/fastrender
export FASTRENDER_ASSETS="$GAME/public"
PARAMS='{"boxes": 0, "lights": 0, "programs": "three", "width": 1280, "height": 800, "prof": 1}'
OUT=jsc-profile; mkdir -p $OUT
SUM=$OUT/summary-$GROUP.txt; : > $SUM
limit() { perl -e 'alarm shift; exec @ARGV' "$@"; } # macOS has no timeout(1)

# run <name> <binary> [ENV=VALUE ...]: one jsbench run (5 min limit); the summary lines carry the result + the probe
run() {
  local name=$1 bin=$2; shift 2
  local t0=$(date +%s)
  env "$@" perl -e 'alarm shift; exec @ARGV' 300 "$bin" jsbench "$BUNDLE" --warmup 71 --frames 30 --params "$PARAMS" > $OUT/$name.out 2> $OUT/$name.err
  local rc=$?
  echo "[$name] exit $rc, $(( $(date +%s) - t0 )) s wall: $(grep -h 'jsbench result' $OUT/$name.out | sed -e 's/jsbench result: //' -e 's/in-JS breakdown (ms.frame)/breakdown/' -e 's/ over frames 71-100//' | cut -c1-250)" | tee -a $SUM
  echo "[$name] $(grep -h 'jsbench probe\|bundle evaluated' $OUT/$name.out | tr '\n' ' ' | cut -c1-270)" | tee -a $SUM
  grep -h '^fastrender:' $OUT/$name.err | head -2 | cut -c1-270 | sed "s/^/[$name] /" | tee -a $SUM
  [ $rc -ne 0 ] && { tail -2 $OUT/$name.out; tail -2 $OUT/$name.err; } | cut -c1-270 | tee -a $SUM
  return 0
}

case $GROUP in
tiers)
  {
    echo "runner: macOS $(sw_vers -productVersion), $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cpus, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB"
    echo "JavaScriptCore.framework $(defaults read /System/Library/Frameworks/JavaScriptCore.framework/Resources/Info CFBundleVersion 2>&1 | tail -1)"
    echo "binary signature: $(codesign -dvv $B 2>&1 | grep -E 'flags|Signature|TeamIdentifier' | tr '\n' ' ')"
    echo "binary entitlements: $(codesign -d --entitlements - $B 2>&1 | grep -v Executable | tr '\n' ' ' | cut -c1-200)"
  } | tee -a $SUM
  # before: the binary as cargo links it (ad hoc, no entitlements); FASTRENDER_NO_RESIGN keeps it that way
  cp $B $OUT/fr-unsigned
  run unsigned $OUT/fr-unsigned FASTRENDER_NO_RESIGN=1
  run unsigned-no-jit $OUT/fr-unsigned FASTRENDER_NO_RESIGN=1 JSC_useJIT=0
  # after: the first run signs itself with allow-jit and restarts (src/jsc.rs ensure_jit)
  run self-signed $B
  echo "binary entitlements after the first run: $(codesign -d --entitlements - $B 2>&1 | grep -o 'com.apple[a-z.-]*' | tr '\n' ' ')" | tee -a $SUM
  run signed-again $B
  run signed-dollarvm $B JSC_useDollarVM=1
  run signed-no-ftl $B JSC_useFTLJIT=0
  run signed-no-dfg $B JSC_useDFGJIT=0
  run signed-no-jit $B JSC_useJIT=0
  run signed-no-concurrent-jit $B JSC_useConcurrentJIT=0
  grep -h 'jsbench frames' $OUT/self-signed.out | sed "s/^/[self-signed] /" | cut -c1-200 > $OUT/windows.txt
  grep -h 'jsbench frames' $OUT/unsigned.out | sed "s/^/[unsigned] /" | cut -c1-200 >> $OUT/windows.txt
  ;;
signing)
  # ad hoc (what the linker makes) vs hardened runtime without and with the JIT entitlement
  cp $B $OUT/fr-hardened && codesign -f -s - --options runtime $OUT/fr-hardened 2>&1 | tail -1
  run signed-hardened-no-entitlement $OUT/fr-hardened FASTRENDER_NO_RESIGN=1
  cp $B $OUT/fr-hardened-jit && codesign -f -s - --options runtime --entitlements macos/fastrender.entitlements $OUT/fr-hardened-jit 2>&1 | tail -1
  run signed-hardened-allow-jit $OUT/fr-hardened-jit
  cp $B $OUT/fr-jit && bash macos/sign.sh $OUT/fr-jit
  run signed-allow-jit $OUT/fr-jit
  ;;
engines)
  # the same bundle + polyfills + virtual timers in other engines on this machine (js/test/world-objects.jsc-prof.mjs)
  other() {
    local name=$1; shift
    echo "[$name] $(limit 420 "$@" js/test/world-objects.jsc-prof.mjs "$BUNDLE" "$GAME" 2> $OUT/$name.err | tail -1 | cut -c1-280)" | tee -a $SUM
  }
  other node-v8 node
  if limit 180 npm install -g bun > $OUT/bun-install.txt 2>&1 && command -v bun > /dev/null; then
    other bun-jsc bun
    other bun-jsc-no-ftl env BUN_JSC_useFTLJIT=0 bun
    other bun-jsc-no-dfg env BUN_JSC_useDFGJIT=0 bun
    other bun-jsc-no-jit env BUN_JSC_useJIT=0 bun
  else
    echo "[bun] not installed: $(tail -2 $OUT/bun-install.txt | tr '\n' ' ')" | tee -a $SUM
  fi
  # the game built for node (real timers, no vm context): js/test/world-objects.node.mjs
  (cd js && limit 420 node test/world-objects.node.mjs "$GAME" --three three --frames 30 --json "$OLDPWD/$OUT/node.json" > "$OLDPWD/$OUT/node.txt" 2>&1)
  echo "[node, ESM build, real timers] $(node -e "const r=require('./$OUT/node.json');console.log(JSON.stringify(r.perFrameMs))" 2>&1 | cut -c1-280)" | tee -a $SUM
  ;;
diagnostics)
  # which options the framework runs with (does it read JSC_* at all, are the tiers on)
  run dump-options $B JSC_dumpOptions=1
  cat $OUT/dump-options.out $OUT/dump-options.err | grep -E '(useJIT|useBaselineJIT|useDFGJIT|useFTLJIT|useConcurrentJIT|useLLInt|useJITCage|useWebAssembly|useBBQJIT|useOMGJIT|useConcurrentGC|useGenerationalGC|numberOfDFGCompilerThreads|numberOfFTLCompilerThreads|numberOfGCMarkers|smallHeapSize|largeHeapSize|thresholdForOptimizeAfterWarmUp|thresholdForFTLOptimizeAfterWarmUp|forceRAMSize)=' | tr -s ' ' | tr '\n' ';' | fold -w 280 | tee -a $SUM
  echo | tee -a $SUM
  run compile-times $B JSC_reportCompileTimes=1
  echo "compiles by tier: $(cat $OUT/compile-times.err $OUT/compile-times.out | grep -oE 'with (Baseline|DFG|FTL|B3|Air|LLInt)[A-Za-z]*|using [A-Za-z]+Mode' | sort | uniq -c | tr '\n' ';')" | tee -a $SUM
  cat $OUT/compile-times.err $OUT/compile-times.out | grep -E 'Optimized|Compiled' | head -2 | cut -c1-200 | tee -a $SUM
  run log-gc $B JSC_logGC=1
  echo "GC log lines: $(cat $OUT/log-gc.err $OUT/log-gc.out | grep -c 'GC')" | tee -a $SUM
  cat $OUT/log-gc.err $OUT/log-gc.out | grep 'GC' | tail -3 | cut -c1-280 | tee -a $SUM
  run no-concurrent-gc $B JSC_useConcurrentGC=0
  # JavaScriptCore's own sampling profiler: top functions + the tier each sample ran in
  mkdir -p $OUT/sampling
  run sampling $B JSC_useSamplingProfiler=1 JSC_samplingProfilerPath=$PWD/$OUT/sampling JSC_samplingProfilerTopFunctionsCount=40 JSC_samplingProfilerTopBytecodesCount=30 JSC_sampleInterval=500
  ls $OUT/sampling | head -5 | tee -a $SUM
  cat $OUT/sampling.err $OUT/sampling.out | grep -A45 -i 'top functions\|Sampling rate' | head -70 | cut -c1-200 > $OUT/sampling-top.txt
  cat $OUT/sampling.err $OUT/sampling.out | grep -B2 -A12 -i 'tier breakdown' | head -30 | cut -c1-200 >> $OUT/sampling-top.txt
  ;;
metal)
  # with Metal: the full host, same frames (the JS figure of `three frame cost`)
  metal() {
    local name=$1 bin=$2; shift 2
    env -u MTL_DEBUG_LAYER "$@" perl -e 'alarm shift; exec @ARGV' 600 $bin bench --three "$BUNDLE" --scale 1 --upscale bilinear --size 1280x800 --frames 30 --warmup 71 > $OUT/bench-$name.out 2>&1
    echo "[bench --three, with Metal, $name] exit $?: $(grep -h 'three frame cost' $OUT/bench-$name.out | tail -1 | cut -c1-240)" | tee -a $SUM
  }
  metal signed $B
  if [ -f $OUT/fr-unsigned ]; then metal unsigned $OUT/fr-unsigned FASTRENDER_NO_RESIGN=1; fi
  ;;
esac
cat $SUM >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
exit 0
