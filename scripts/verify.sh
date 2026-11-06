#!/usr/bin/env bash
# Required verification entry for Metera.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
bash scripts/build-kernel.sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
(
    cd sdk/go
    go vet ./...
    go test ./...
)
