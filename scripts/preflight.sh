#!/usr/bin/env bash
# Runs exactly what .github/workflows/ci.yml runs, with the pinned toolchain
# from rust-toolchain.toml. Green here means CI's test+lint jobs are green
# (OS-specific failures on the other runners still cannot be reproduced from
# one machine). Run before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "toolchain: $(rustc --version)"

echo
echo "[1/3] cargo test --workspace --locked"
cargo test --workspace --locked

echo
echo "[2/3] cargo build --workspace --locked --features cdylib"
cargo build --workspace --locked --features cdylib

echo
echo "[3/3] cargo clippy --workspace --locked -- -D warnings"
cargo clippy --workspace --locked -- -D warnings

echo
echo "preflight OK - CI test+lint will pass on this toolchain"
