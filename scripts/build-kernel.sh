#!/usr/bin/env bash
# Builds the Metera WebAssembly kernel into crates/metera-sdk/wasm/metera.wasm.
# The toolchain is pinned, host RUSTFLAGS are not inherited and every
# machine-specific source prefix is remapped to a fixed virtual path, so the
# kernel carries no machine path or user name.
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
toolchain=1.95.0
sep=$(printf '\037')
CARGO_ENCODED_RUSTFLAGS=
append_remap() {
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${CARGO_ENCODED_RUSTFLAGS:+$sep}--remap-path-prefix=$1=$2"
}
append_remap "$HOME" "/_build/home"
append_remap "${CARGO_HOME:-$HOME/.cargo}" "/_build/cargo"
append_remap "${RUSTUP_HOME:-$HOME/.rustup}" "/_build/rustup"
# With rust-src installed, rustc resolves standard-library paths to the local
# sysroot; map them back to the virtual /rustc/<commit> prefix rustc uses
# without it, so the component's presence cannot change the kernel bytes.
commit=$(rustc "+$toolchain" -vV | sed -n 's/^commit-hash: //p')
append_remap "$(rustc "+$toolchain" --print sysroot)/lib/rustlib/src/rust" "/rustc/$commit"
append_remap "$(CDPATH= cd -- "$root/../ACOP" && pwd)" "/_build/acop"
append_remap "$root" "/_build/metera"
export CARGO_ENCODED_RUSTFLAGS
artifact=$(
    cargo "+$toolchain" build --release --locked --target wasm32-unknown-unknown -p metera-wasm --message-format=json-render-diagnostics |
        python3 -c '
import json
import sys
artifacts = []
for line in sys.stdin:
    event = json.loads(line)
    if event.get("reason") == "compiler-artifact" and event.get("target", {}).get("name") == "metera_wasm":
        artifacts.extend(path for path in event.get("filenames", []) if path.endswith(".wasm"))
if len(artifacts) != 1:
    raise SystemExit("expected one Metera WASM build artifact")
print(artifacts[0])
'
)
mkdir -p crates/metera-sdk/wasm
cp "$artifact" crates/metera-sdk/wasm/metera.wasm
chmod 0644 crates/metera-sdk/wasm/metera.wasm
