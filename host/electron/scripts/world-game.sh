#!/bin/bash
# The real game for the benchmarks: cktang88/3d-world-objects at the commit patches/ were made on, with every
# patches/3d-world-objects-000*.patch applied in order (as CI's "real game" step does).
#
#   host/electron/scripts/world-game.sh [dir]     default: host/electron/.bench/3d-world-objects
#
# Idempotent: <dir>/.bench-patched lists the patches already applied, so a copy made before a patch existed gets
# the new one (an empty list is a copy from before the list was kept: patches 0001 and 0002). Prints the directory
# on success; exit 1 when the game cannot be fetched or a patch does not apply.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
world="${1:-$here/../.bench/3d-world-objects}"
WO_BASE=073aa9eae0ba86cecb16970162593281714214e3
if [ ! -f "$world/.bench-patched" ]; then
  echo "fetching cktang88/3d-world-objects @ ${WO_BASE:0:7} -> $world" >&2
  rm -rf "$world"; mkdir -p "$world"
  git -C "$world" init -q && git -C "$world" fetch -q --depth 1 https://github.com/cktang88/3d-world-objects "$WO_BASE" && git -C "$world" checkout -q FETCH_HEAD \
    || { echo "cannot fetch the game (private repo? pass a checkout with patches/ applied)" >&2; exit 1; }
  echo "# base $WO_BASE" > "$world/.bench-patched"
fi
[ -s "$world/.bench-patched" ] || printf '3d-world-objects-0001-air-mode.patch\n3d-world-objects-0002-three-r187.patch\n' > "$world/.bench-patched"
for p in "$repo"/patches/3d-world-objects-000*.patch; do
  name="$(basename "$p")"
  grep -qxF "$name" "$world/.bench-patched" && continue
  echo "applying $name" >&2
  git -C "$world" -c user.name=bench -c user.email=bench@localhost am -q "$p" >&2 || { git -C "$world" am --abort >/dev/null 2>&1; echo "patch $name does not apply" >&2; exit 1; }
  echo "$name" >> "$world/.bench-patched"
done
(cd "$world" && pwd)
