#!/bin/bash
# One command: three's WebGLRenderer vs three's WebGPURenderer vs fastrender, in the same Electron app, on THIS Mac.
# Builds the addon and the bundles, runs every variant, writes <out>/bench-3way.md and <out>/bench-3way.json
# (docs/HOST.md "3-way benchmark", docs/research/webgpu-vs-fastrender.md).
#
#   host/electron/scripts/bench-3way.sh [options]
#     --world DIR        a 3d-world-objects checkout with patches/ applied (default: fetched + patched into
#                        host/electron/.bench/3d-world-objects; the real-game runs are skipped if that fails)
#     --out DIR          default host/electron/bench-out
#     --sizes "A B"      colour buffer sizes in device pixels (default "1280x800 2560x1600")
#     --scenes "A B"     of: empty city world (default: all three)
#     --variants "A B"   of: webgl webgpu fastrender (default: all three)
#     --warmup S         default 10        --measure S   default 20
#     --quick            3 s warmup, 8 s measure, 1280x800 only (what CI runs)
#     --no-vsync-pass    skip the second pass with vsync ON (CPU % at the display's rate, first size only)
#     --no-build         reuse the addon and bundles of a previous run
#     --power            also sample `sudo powermetrics` (GPU power / active residency) during each run
#
# Needs: macOS on Apple Silicon, Xcode command line tools, Rust (cargo), Node 20+. Close other apps, plug the Mac in,
# leave the window alone (it is always on top, 640x400 / 1280x800 points on a Retina display). About 25 minutes.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
el="$(cd "$here/.." && pwd)"
repo="$(cd "$el/../.." && pwd)"
world=""; out="$el/bench-out"; sizes="1280x800 2560x1600"; scenes="empty city world"; variants="webgl webgpu fastrender"
warmup=10; measure=20; vsync_pass=1; build=1; power=0; note=""
electron="${BENCH_ELECTRON:-npx electron}"   # testing elsewhere: BENCH_ELECTRON="xvfb-run -a npx electron --no-sandbox"
while [ $# -gt 0 ]; do
  case "$1" in
    --world) world="$2"; shift 2;;
    --out) out="$2"; shift 2;;
    --sizes) sizes="$2"; shift 2;;
    --scenes) scenes="$2"; shift 2;;
    --variants) variants="$2"; shift 2;;
    --warmup) warmup="$2"; shift 2;;
    --measure) measure="$2"; shift 2;;
    --quick) warmup=3; measure=8; sizes="1280x800"; shift;;
    --no-vsync-pass) vsync_pass=0; shift;;
    --no-build) build=0; shift;;
    --power) power=1; shift;;
    --note) note="$2"; shift 2;;
    -h|--help) sed -n '2,22p' "$0"; exit 0;;
    *) echo "unknown option $1"; exit 2;;
  esac
done
mkdir -p "$out"; out="$(cd "$out" && pwd)"
cd "$el" || exit 2
say() { printf '\n== %s\n' "$*"; }

# the real game: the commit patches/ were made on, + patches (air mode, three r187), as CI's "real game" step does
WO_BASE=073aa9eae0ba86cecb16970162593281714214e3
case " $scenes " in *" world "*)
  if [ -z "$world" ]; then
    world="$el/.bench/3d-world-objects"
    if [ ! -f "$world/.bench-patched" ]; then
      say "fetching cktang88/3d-world-objects @ ${WO_BASE:0:7} + patches -> $world"
      rm -rf "$world"; mkdir -p "$world"
      if git -C "$world" init -q && git -C "$world" fetch -q --depth 1 https://github.com/cktang88/3d-world-objects "$WO_BASE" && git -C "$world" checkout -q FETCH_HEAD; then
        ok=1
        for p in "$repo"/patches/3d-world-objects-000*.patch; do
          git -C "$world" -c user.name=bench -c user.email=bench@localhost am -q "$p" || { echo "patch $p does not apply"; ok=0; break; }
        done
        [ "$ok" = 1 ] && touch "$world/.bench-patched" || world=""
      else
        echo "cannot fetch the game (private repo? pass --world DIR): the real-game runs are skipped"; world=""
      fi
    fi
  fi
  if [ -z "$world" ] || [ ! -f "$world/index.html" ]; then scenes="$(echo " $scenes " | sed 's/ world / /')"; world=""; fi
  ;;
esac

if [ "$build" = 1 ]; then
  say "npm ci + addon (cargo, release)"
  [ -d node_modules/electron ] || npm ci || exit 2
  case " $variants " in *" fastrender "*) npm run build || { echo "addon build failed: running without the fastrender variant"; variants="$(echo " $variants " | sed 's/ fastrender / /')"; };; esac
  say "bundles"
  if [ -n "$world" ]; then node scripts/bench-3way.bundle.mjs --world "$world" || exit 2; else node scripts/bench-3way.bundle.mjs || exit 2; fi
  (cd "$repo/js" && [ -d node_modules/pngjs ] || npm ci --silent) > /dev/null 2>&1 || true   # screenshot diff (optional)
fi

rm -f "$out"/bench-*.json "$out"/bench-*-shot.png "$out"/bench-*.log "$out"/bench-*-power.txt
n=0
one() { # one <scene> <variant> <px> <aa> <vsync> [fx]   (fx: fastrender renders at half size, MetalFX upscales)
  local scene="$1" variant="$2" px="$3" aa="$4" vs="$5" fx="${6:-}" tag args pm=""
  tag="bench-$scene-$variant-$px"; [ "$aa" = 1 ] && tag="$tag-aa"; [ "$vs" = 1 ] && tag="$tag-vsync"; [ -n "$fx" ] && tag="$tag-fx"
  args="--variant $variant --scene $scene --px $px --aa $aa --vsync $vs --warmup $warmup --measure $measure --out $out --tag $tag --timeout $((warmup + measure + 600))"
  [ -n "$fx" ] && args="$args --renderer {\"scale\":0.5,\"upscale\":\"metalfx\"} --label MetalFX-spatial-from-0.5x-per-axis"
  n=$((n + 1)); printf '%2d  %-44s ' "$n" "$tag"
  if [ "$power" = 1 ]; then sudo -n powermetrics --samplers gpu_power,cpu_power -i 1000 > "$out/$tag-power.txt" 2>/dev/null & pm=$!; fi
  if [ "$scene" = world ]; then
    # shellcheck disable=SC2086
    $electron game/bench-main.js $args --html "$world/index.html" --assets "$world/public" > "$out/$tag.log" 2>&1
  else
    # shellcheck disable=SC2086
    $electron game/bench-main.js $args > "$out/$tag.log" 2>&1
  fi
  [ -n "$pm" ] && { sudo -n kill "$pm" 2>/dev/null; wait "$pm" 2>/dev/null; }
  node -e 'const fs=require("fs");let r={};try{r=JSON.parse(fs.readFileSync(process.argv[1],"utf8"))}catch{}
    if(!r.ok||!r.frame_ms)console.log("FAILED: "+String(r.error||"no result (see the .log)").replace(/\s+/g," ").slice(0,140));
    else console.log(`${r.fps.toFixed(1)} fps  frame p50 ${r.frame_ms.p50} ms  JS ${r.js_ms.p50} ms  GPU ${r.gpu_ms?r.gpu_ms.mean:"n/a"} ms  CPU ${r.processes&&r.processes.total?r.processes.total.cpu_percent:"?"}%  [${r.backend&&r.backend.api}]`)' "$out/$tag.json"
  sleep 2   # let the machine settle (and cool) between runs
}

say "runs: vsync off (uncapped), ${warmup} s warmup + ${measure} s measured each"
for scene in $scenes; do
  for px in $sizes; do
    for v in $variants; do one "$scene" "$v" "$px" 0 0; done
    # the game as shipped asks for MSAA 4x; fastrender has none, so the rows above turn it off everywhere
    if [ "$scene" = world ]; then for v in $variants; do [ "$v" = fastrender ] || one world "$v" "$px" 1 0; done; fi
    # what only the native renderer has: a quarter of the pixels shaded, MetalFX spatial upscaling to the same output
    # (NOT like-for-like with the rows above: a different image; the browser's equivalent is FSR1 in a shader)
    if [ "$scene" != empty ]; then case " $variants " in *" fastrender "*) one "$scene" fastrender "$px" 0 0 fx;; esac; fi
  done
done
if [ "$vsync_pass" = 1 ]; then
  px="${sizes%% *}"
  say "runs: vsync ON (CPU and GPU cost at the display's rate), $px"
  for scene in $scenes; do for v in $variants; do one "$scene" "$v" "$px" 0 1; done; done
fi

say "report"
[ -n "$note" ] || note="$(sysctl -n machdep.cpu.brand_string 2>/dev/null), $(sw_vers -productName 2>/dev/null) $(sw_vers -productVersion 2>/dev/null); $(date '+%Y-%m-%d %H:%M')"
node scripts/bench-3way.report.mjs "$out" --note "$note" > /dev/null || exit 2
if [ "$power" = 1 ]; then
  { echo; echo "## powermetrics (mean over each run, warmup included)"; echo; echo "| run | GPU power mW | GPU active % | CPU power mW |"; echo "|---|---:|---:|---:|"
    for f in "$out"/bench-*-power.txt; do
      [ -s "$f" ] || continue
      awk -v name="$(basename "$f" -power.txt)" '/^GPU Power:/ {g+=$3; gn++} /^CPU Power:/ {c+=$3; cn++} /GPU HW active residency:/ {gsub("%","",$5); a+=$5; an++}
        END { if (gn) printf "| %s | %.0f | %.1f | %.0f |\n", name, g/gn, an ? a/an : 0, cn ? c/cn : 0 }' "$f"
    done; } >> "$out/bench-3way.md"
fi
cat "$out/bench-3way.md"
echo
echo "wrote $out/bench-3way.md and $out/bench-3way.json (per-run JSON, logs and screenshots next to them)"
