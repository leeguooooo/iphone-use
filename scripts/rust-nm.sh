#!/usr/bin/env bash
# Inspect Rust archives with the LLVM reader shipped by the active Rust compiler.
set -euo pipefail
rust_nm_sysroot="$(rustc --print sysroot)"
rust_nm_host="$(rustc -vV | sed -n 's/^host: //p')"
test -n "$rust_nm_host"
nm_bin="$rust_nm_sysroot/lib/rustlib/$rust_nm_host/bin/llvm-nm"
if [ ! -x "$nm_bin" ]; then
  echo 'rust-nm: install the active toolchain component with rustup component add llvm-tools-preview' >&2
  exit 1
fi
exec "$nm_bin" "$@"
