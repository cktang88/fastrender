#!/bin/sh
# C++ compiler for the `aarch64-apple-darwin` target (.cargo/config.toml points cc-rs at it).
# On a Mac it is the system compiler, unchanged. On Linux, `cargo check --target aarch64-apple-darwin` only
# type-checks the Rust: the C++ of a -sys crate (meshoptimizer in `meshopt`) is compiled for the host with the
# Apple-only flags dropped, because cargo runs build scripts even for a check and nothing is linked.
if [ "$(uname -s)" = Darwin ]; then exec c++ "$@"; fi
n=$#
while [ "$n" -gt 0 ]; do
  a=$1; shift; n=$((n-1))
  case "$a" in
    -arch) shift; n=$((n-1));;
    -mmacosx-version-min=*|-stdlib=*|--target=*) ;;
    *) set -- "$@" "$a";;
  esac
done
exec c++ "$@"
