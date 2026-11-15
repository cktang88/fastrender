#!/bin/bash
# Sign a fastrender binary (or .app) ad hoc with the entitlements JavaScriptCore's JIT needs.
#   macos/sign.sh [path, default target/release/fastrender] [--hardened]
# Without com.apple.security.cs.allow-jit the system JavaScriptCore.framework runs JS in its interpreter (~6x slower;
# docs/research/jsc-vs-v8.md). `fastrender` signs itself on first run when it finds the entitlement missing
# (src/jsc.rs ensure_jit); this script is for builds that must be signed ahead (CI artifacts, an app bundle).
# --hardened adds the hardened runtime, as notarization requires (use a Developer ID identity instead of "-" there).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-target/release/fastrender}
opts=()
[ "${2:-}" = "--hardened" ] && opts=(--options runtime)
codesign -f -s - "${opts[@]}" --entitlements "$here/fastrender.entitlements" "$bin"
codesign -d --entitlements - "$bin" 2>&1 | grep -q allow-jit && echo "signed $bin (allow-jit)"
