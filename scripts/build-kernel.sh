#!/usr/bin/env bash
# Builds the Metera WebAssembly kernel into crates/metera-sdk/wasm/metera.wasm.
# The build always runs in one pinned linux/amd64 Rust image with fixed mount
# paths, a fixed CARGO_HOME and fixed path remapping, so every host produces
# the same bytes and no host path or user name reaches the kernel. Requires
# Docker (amd64 emulation on ARM hosts).
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

# rust:1.95.0-bookworm, the linux/amd64 image manifest of index
# sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1.
base=rust@sha256:4c2fd73ef19c5ef9d54bee03b06b2839a392604fbfcd578ed948b71b37c1d7fb
image=metera-kernel-build:1.95.0
docker build --quiet --platform linux/amd64 --tag "$image" - >/dev/null <<EOF
FROM $base
RUN rustup target add --toolchain 1.95.0 wasm32-unknown-unknown
EOF

# Crate downloads and build output persist between runs under target/; the
# Metera checkout is read-only. Every dependency resolves from crates.io.
cache=target/kernel-build
mkdir -p "$cache"
sep=$(printf '\037')
rustflags="--remap-path-prefix=/build/cargo=/_build/cargo${sep}--remap-path-prefix=/src/Metera=/_build/metera"
docker run --rm --platform linux/amd64 --user "$(id -u):$(id -g)" \
    -v "$root:/src/Metera:ro" -v "$root/$cache:/build" --workdir /src/Metera \
    --env HOME=/build --env CARGO_HOME=/build/cargo --env CARGO_TARGET_DIR=/build/target \
    --env "CARGO_ENCODED_RUSTFLAGS=$rustflags" \
    "$image" \
    sh -c 'cargo +1.95.0 build --release --locked --target wasm32-unknown-unknown -p metera-wasm &&
        cp /build/target/wasm32-unknown-unknown/release/metera_wasm.wasm /build/metera.wasm'
mkdir -p crates/metera-sdk/wasm
cp "$cache/metera.wasm" crates/metera-sdk/wasm/metera.wasm
chmod 0644 crates/metera-sdk/wasm/metera.wasm
