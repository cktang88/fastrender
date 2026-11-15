#!/bin/bash
# .github/compile-launch.sh <tag> <fastrender bench args...>
# Launch cost of one scene with the compile pipeline of src/mac_programs.rs + src/pipeline_manifest.rs: cold (no
# manifest, no archive, no translated-shader cache, the OS's own Metal shader cache removed) vs warm (second launch),
# with and without the binary archive, against a blocking single-worker launch (what the renderer did before: every
# compile on the render thread's time). Rows go to compile-launch-<tag>.txt; logs to compile-launch-<tag>-*.log.
tag=$1; shift
ARGS=("$@")
bin=./target/release/fastrender
pc="$PWD/pc-$tag"; sc="$PWD/sc-$tag"; out="compile-launch-$tag.txt"; : > "$out"
os_cache() {  # the shader cache Metal keeps for the process by itself (functions compiled for a pipeline)
  find "$(getconf DARWIN_USER_CACHE_DIR)" -type d -name 'com.apple.metal*' 2>/dev/null | grep -i fastrender
}
clear_os_cache() {
  os_cache | while read -r d; do echo "os metal cache: $(du -sk "$d" | cut -f1) KB in $d: removed" | tee -a "$out"; rm -rf "$d"; done
}
now() { node -e 'console.log(Date.now())'; }
run() {  # <label> <env assignments...>
  label=$1; shift
  log="compile-launch-$tag-$(echo "$label" | cut -c1-1).log"
  t0=$(now); rc=0
  env FASTRENDER_PIPELINE_CACHE="$pc" FASTRENDER_SHADER_CACHE="$sc" "$@" "$bin" bench "${ARGS[@]}" > "$log" 2>&1 || rc=$?
  node .github/compile-launch.mjs "$label" "$log" $(( $(now) - t0 )) "$rc" | tee -a "$out"
}
echo "os metal caches: $(find "$(getconf DARWIN_USER_CACHE_DIR)" -type d -name 'com.apple.metal*' 2>/dev/null | sed "s|$(getconf DARWIN_USER_CACHE_DIR)||" | tr '\n' ' ' | cut -c1-250)" | tee -a "$out"
rm -rf "$pc" "$sc"; clear_os_cache
run "1 before: blocking 1 worker, cold" FASTRENDER_COMPILE=blocking FASTRENDER_COMPILE_THREADS=1 FASTRENDER_PIPELINE_CACHE=off
rm -rf "$pc" "$sc"; clear_os_cache
run "2 blocking (harness), cold" FASTRENDER_COMPILE=blocking FASTRENDER_PIPELINE_CACHE=off
rm -rf "$pc" "$sc"; clear_os_cache
run "3 async, cold" FASTRENDER_COMPILE=async
echo "pipeline cache after the cold launch: $(du -sk "$pc" 2>/dev/null | cut -f1) KB, $(find "$pc" -name '*.metal' 2>/dev/null | wc -l | tr -d ' ') stages, $(find "$pc" -name '*.binarchive' -exec du -k {} \; 2>/dev/null | cut -f1 | tr '\n' ' ')KB archive, manifest $(find "$pc" -name manifest.json -exec wc -c {} \; 2>/dev/null | awk '{print $1}') bytes" | tee -a "$out"
run "4 async, warm (manifest+archive)" FASTRENDER_COMPILE=async
clear_os_cache
run "5 async, warm, OS cache removed" FASTRENDER_COMPILE=async
clear_os_cache
run "6 async, warm, no archive, OS cache removed" FASTRENDER_COMPILE=async FASTRENDER_ARCHIVE=0
run "7 blocking (harness), warm" FASTRENDER_COMPILE=blocking
grep -h "pipeline cache:\|built-in metallib" compile-launch-"$tag"-?.log | cut -c1-260 | sort | uniq -c | head -5 | tee -a "$out"
for k in 3 4; do
  grep -h "^three compile:" "compile-launch-$tag-$k.log" | sed "s/.*archive: /archive, run $k: /" | cut -c1-280 | tee -a "$out"
  grep -h "^three compile:" "compile-launch-$tag-$k.log" | sed "s/, [0-9]* libraries.*//" | cut -c1-200 | tee -a "$out"
done
exit 0
