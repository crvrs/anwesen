#!/usr/bin/env bash
# Local CI entrypoint: fmt / clippy / test.
# Mirrors what a forge-side runner would execute. Run from the repo root.

set -euo pipefail

cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
