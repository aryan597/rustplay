#!/usr/bin/env bash
# Merge gate. CI runs exactly this. There is no skip flag for evals.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n== %s\n' "$*"; }

step "format"
cargo fmt --all -- --check

step "clippy (all features, deny warnings)"
cargo clippy --all-targets -- -D warnings

step "clippy (CLI-only build)"
cargo clippy --all-targets --no-default-features -- -D warnings

step "unit tests"
cargo test --lib

step "docs drift"
cargo test --test docs_sync

have_browser=${RUSTPLAY_CHROME:-}
for b in chromium chromium-browser google-chrome google-chrome-stable chrome; do
  command -v "$b" >/dev/null 2>&1 && have_browser=1
done
if [ -z "$have_browser" ]; then
  echo "FAIL: no Chrome/Chromium. Browser tests and evals are mandatory; set RUSTPLAY_CHROME." >&2
  exit 1
fi

step "integration tests (real Chromium, local fixtures)"
cargo test --test integration

step "actionability hit test vs open shadow roots"
cargo test --test shadow_hit_test

step "evals (8-layer failure modes)"
cargo test --test evals

step "CDP transport (pipe: no port; tcp: deprecated control)"
cargo test --test transport

step "resource limits (real kernel: enforced, or LIMIT_UNSUPPORTED)"
cargo test --test limits

step "example suite via the CLI"
args=()
[ "$(id -u)" = "0" ] && args+=(--no-chrome-sandbox)
cargo run -q -- test examples/suite --serve tests/fixtures -j 4 "${args[@]}" >/dev/null

echo
echo "gate: PASS"
