#!/bin/bash
# One command: PLAY the real game (cktang88/3d-world-objects + patches/) on fastrender, on THIS Mac.
# Checks what it needs, installs Electron, builds the addon, fetches + patches the game, bundles it and opens the
# window (docs/HOST.md "Play mode"). Every step is skipped or incremental the next time.
#
#   host/electron/scripts/play-world.sh [options]
#     --webgl            the same game copy on stock three WebGLRenderer, same window (to feel the difference)
#     --fps              frame-rate overlay: fps, JS ms, GPU ms, draws, pixels (F3 toggles it either way)
#     --search '?...'    the game's own URL parameters (default '?mode=world&quality=low&seamless=0&nodyn'), e.g.
#                        '?mode=world&quality=low&seamless=0&nodyn&tod=night&weather=rain'
#     --renderer '{..}'  fastrender options over the defaults {"scale":1,"upscale":"bilinear","vsync":1}, e.g.
#                        '{"scale":0.5,"upscale":"metalfx"}' (a quarter of the pixels, MetalFX upscaling),
#                        '{"vsync":0}' (uncapped), '{"dynres":14}' (dynamic resolution towards 14 ms of GPU)
#     --size 1280x800    window content size in points (default: the screen's work area)
#     --dpr game|N       pixel ratio the game sees (default: the display's, on both renderers; `game` = the game's
#                        own choice: 1 at quality=low, which makes WebGL shade a quarter of the pixels on Retina)
#     --no-aa            --webgl only: MSAA off (the game asks for 4x; fastrender has none)
#     --online           let the game reach its CDNs (objects streamed from Hugging Face; refused by default, as in
#                        the golden and the benchmark; not tested on fastrender)
#     --game-mode        mark the dev Electron as a game (macOS Game Mode in full screen; scripts/game-mode.mjs)
#     --frame off        frameless window
#     --world DIR        a 3d-world-objects checkout with patches/ applied (default: fetched + patched into
#                        host/electron/.bench/3d-world-objects, the copy bench-3way.sh uses)
#     --no-build         skip npm ci / the addon / the bundle (reuse the previous run's)
#     --play-seconds N   CI: run unattended for N seconds (resize, click, W), print PASS / FAIL, exit
#
# In the game: click the view to capture the mouse (Esc releases it and pauses), WASD / arrows move, Shift runs,
# Space jumps, E enters / shrinks in, Q backs out, hold Z to zoom, T time of day, Y weather, M map, C credits.
# Window: ctrl-cmd-F or F11 full screen, F3 fps overlay, cmd-Q quit.
#
# Needs: macOS on Apple Silicon, Xcode command line tools, Rust (cargo), Node 20+, git. First run: a few minutes
# (the addon's release build); later runs: seconds.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
el="$(cd "$here/.." && pwd)"
variant=fastrender; world=""; build=1; game_mode=0
pass=()
electron="${PLAY_ELECTRON:-npx electron}"   # testing elsewhere: PLAY_ELECTRON="xvfb-run -a npx electron --no-sandbox"
while [ $# -gt 0 ]; do
  case "$1" in
    --webgl) variant=webgl; shift;;
    --world) world="$2"; shift 2;;
    --no-build) build=0; shift;;
    --game-mode) game_mode=1; shift;;
    --fps|--online|--no-aa) pass+=("$1"); shift;;
    --search|--renderer|--size|--dpr|--frame|--play-seconds|--out|--tag|--timeout)
      [ $# -ge 2 ] || { echo "$1 needs a value"; exit 2; }
      pass+=("$1" "$2"); shift 2;;
    -h|--help) sed -n '2,33p' "$0"; exit 0;;
    *) echo "unknown option $1 (--help)"; exit 2;;
  esac
done
say() { printf '\n== %s\n' "$*"; }

# ---- what this needs
missing=0
miss() { echo "MISSING: $*"; missing=1; }
if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  if [ "$variant" = fastrender ] && [ -z "${PLAY_ELECTRON:-}" ]; then miss "fastrender is a Metal renderer: macOS on Apple Silicon (this is $(uname -s) $(uname -m)); --webgl runs elsewhere"; fi
elif ! xcode-select -p > /dev/null 2>&1; then
  miss "Xcode command line tools:  xcode-select --install"
fi
command -v git > /dev/null 2>&1 || miss "git (it comes with the Xcode command line tools)"
if command -v node > /dev/null 2>&1 && command -v npm > /dev/null 2>&1; then
  major="$(node -p 'process.versions.node.split(".")[0]')"
  [ "$major" -ge 20 ] || miss "Node 20 or newer (this is $(node --version)):  brew install node"
else
  miss "Node 20 or newer with npm:  brew install node   (or https://nodejs.org)"
fi
if [ "$variant" = fastrender ] && [ "$build" = 1 ]; then
  command -v cargo > /dev/null 2>&1 || miss "Rust:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   (then open a new terminal)"
  if [ "$(uname -s)" = Darwin ] && ! xcrun -sdk macosx metal --version > /dev/null 2>&1; then
    echo "note: no Metal Toolchain: the renderer compiles its built-in shaders at each launch instead of at build time."
    echo "      Optional, once:  xcodebuild -downloadComponent MetalToolchain"
  fi
fi
[ "$missing" = 0 ] || exit 2
cd "$el" || exit 2

# ---- the game: the commit patches/ were made on + every patch (scripts/world-game.sh; adds new patches to the copy)
if [ -z "$world" ]; then
  say "the game: cktang88/3d-world-objects + patches/"
  world="$(bash "$here/world-game.sh" "$el/.bench/3d-world-objects")" || { echo "cannot get the game (pass --world DIR: a checkout with patches/ applied)"; exit 2; }
fi
[ -f "$world/index.html" ] || { echo "$world: no index.html (not a 3d-world-objects checkout)"; exit 2; }
world="$(cd "$world" && pwd)"

if [ "$build" = 1 ]; then
  # node_modules is current when npm's own copy of the lockfile is not older than package-lock.json
  if [ ! -d node_modules/electron/dist ] || [ ! -d node_modules/esbuild ] || [ package-lock.json -nt node_modules/.package-lock.json ]; then
    say "npm ci (Electron, esbuild, three)"
    npm ci || exit 2
  fi
  if [ "$variant" = fastrender ]; then
    say "the addon (cargo, release; incremental)"
    npm run build || { echo "the addon did not build (see above). The game still runs on WebGL: $0 --webgl"; exit 2; }
  fi
  say "bundle"
  # the game's only runtime dependency is three, taken from host/electron/node_modules: no npm ci in the game copy
  node scripts/bundle-game.mjs --world "$world" --play "$variant" || exit 2
fi
if [ "$game_mode" = 1 ]; then node scripts/game-mode.mjs || true; fi

say "play ($variant)"
echo "click the view to capture the mouse (Esc releases it) - WASD move, Shift run, Space jump, E enter, Q back,"
echo "Z zoom, T time of day, Y weather, M map - ctrl-cmd-F full screen, F3 fps overlay, cmd-Q quit"
# shellcheck disable=SC2086
exec $electron game/main.js --play --variant "$variant" --html "$world/index.html" --assets "$world/public" ${pass[@]+"${pass[@]}"}
