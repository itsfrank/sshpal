#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="$ROOT_DIR/target/coverage"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to run coverage" >&2
  exit 1
fi

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is required. Install it with:" >&2
  echo "  cargo install cargo-llvm-cov" >&2
  exit 1
fi

mkdir -p "$OUTPUT_DIR"

extra_test_args=()
if [[ "${1:-}" == "--include-docker" ]]; then
  extra_test_args=(-- --ignored)
fi

cd "$ROOT_DIR"

cargo llvm-cov \
  --workspace \
  --text \
  "${extra_test_args[@]}"

cargo llvm-cov report \
  --html \
  --output-dir "$OUTPUT_DIR/html"

cargo llvm-cov report \
  --lcov \
  --output-path "$OUTPUT_DIR/lcov.info"
