#!/usr/bin/env bash
# Required verification entry for Metera.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

# The generated release-rewriter must pass the same helper tests CI runs.
PYTHONDONTWRITEBYTECODE=1 python3 .github/test_release.py
