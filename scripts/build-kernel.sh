set -euo pipefail
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
sep=$(printf '\037')
if [ "${CARGO_ENCODED_RUSTFLAGS+x}" != x ]; then
    CARGO_ENCODED_RUSTFLAGS=
    set -f
    for flag in ${RUSTFLAGS-}; do
        CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${CARGO_ENCODED_RUSTFLAGS:+$sep}$flag"
    done
fi
append_remap() {
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${CARGO_ENCODED_RUSTFLAGS:+$sep}--remap-path-prefix=$1=$2"
}
append_remap "$HOME" "/_build/home"
append_remap "${CARGO_HOME:-$HOME/.cargo}" "/_build/cargo"
append_remap "${RUSTUP_HOME:-$HOME/.rustup}" "/_build/rustup"
append_remap "$(CDPATH= cd -- "$root/../ACOP" && pwd)" "/_build/acop"
append_remap "$root" "/_build/metera"
export CARGO_ENCODED_RUSTFLAGS
artifact=$(
    cargo build --release --locked --target wasm32-unknown-unknown -p metera-wasm --message-format=json-render-diagnostics |
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
