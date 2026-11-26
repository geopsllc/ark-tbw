#!/usr/bin/env bash
# Builds static (musl) Linux binaries in Docker, cross-compiled with cargo-zigbuild on the host's
# own architecture (no emulation needed): dist/ark-tbw-x86_64-linux, dist/ark-tbw-aarch64-linux.
# Usage: tools/build-linux.sh [x86_64|aarch64 ...]   (default: both)
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p dist
arches=("$@"); [ ${#arches[@]} -eq 0 ] && arches=(x86_64 aarch64)
targets=""; for a in "${arches[@]}"; do
  case "$a" in x86_64|aarch64) targets="$targets --target $a-unknown-linux-musl" ;; *) echo "unknown arch $a" >&2; exit 1 ;; esac
done
docker run --rm -v "$PWD":/src:ro -v ark-tbw-cargo:/usr/local/cargo/registry -v ark-tbw-tools:/tools \
  -v ark-tbw-target:/target -v "$PWD/dist":/dist -e CARGO_TARGET_DIR=/target -w /src rust:1-alpine sh -euc "
    apk add --no-cache musl-dev zig >/dev/null
    [ -x /tools/bin/cargo-zigbuild ] || cargo install --locked --root /tools cargo-zigbuild
    for t in $(echo $targets | sed 's/--target//g'); do rustup target add \$t >/dev/null; done
    /tools/bin/cargo-zigbuild zigbuild --release --locked $targets
    for a in ${arches[*]}; do cp /target/\$a-unknown-linux-musl/release/ark-tbw /dist/ark-tbw-\$a-linux; done"
ls -l dist/
