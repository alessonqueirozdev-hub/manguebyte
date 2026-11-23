#!/usr/bin/env bash
# Cloud setup for Manguebyte on Claude Code on the web (Ubuntu 24.04, run as root).
# Idempotent: run it as often as you like. It installs only what this
# repository needs, fetches dependencies and leaves the tests running.
#
#   bash scripts/cloud-setup.sh           # set up and build the tests
#   bash scripts/cloud-setup.sh --test    # ... and run the full check

set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
RUN_TESTS=0
[ "${1:-}" = "--test" ] && RUN_TESTS=1

apt_install() {
  local missing=()
  for p in "$@"; do dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p"); done
  if [ "${#missing[@]}" -gt 0 ]; then
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${missing[@]}"
  fi
}

rust_ready() {
  command -v rustup >/dev/null 2>&1 || { echo "rustup is missing" >&2; exit 1; }
  rustup toolchain install stable --profile minimal --component rustfmt,clippy >/dev/null
  rustup default stable >/dev/null
}

apt_install pkg-config libasound2-dev libjack-jackd2-dev
rust_ready
cargo fetch
cargo test --workspace --no-run --quiet
if [ "$RUN_TESTS" = 1 ]; then
  ./scripts/check.sh
fi
echo "Manguebyte ready. Full check: ./scripts/check.sh"
