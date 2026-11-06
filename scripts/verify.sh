#!/usr/bin/env bash
# Required verification entry for Metera.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
bash scripts/build-kernel.sh
kernel=crates/metera-sdk/wasm/metera.wasm
{ git ls-files --error-unmatch -- "$kernel" >/dev/null && git diff --quiet -- "$kernel"; } || {
    echo "the rebuilt kernel differs from the tracked $kernel; commit the rebuilt file" >&2
    exit 1
}
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
(
    cd sdk/go
    go vet ./...
    go test ./...
)
