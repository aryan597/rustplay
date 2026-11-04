#!/usr/bin/env bash
# One-time setup for rustplay on Ubuntu (WSL2 or native).
# Installs: build tools, Rust (rustup), Google Chrome stable, bubblewrap.
# Safe to re-run. Needs sudo for apt.
set -euo pipefail

say() { printf '\n== %s\n' "$*"; }

if ! grep -qi ubuntu /etc/os-release 2>/dev/null; then
  echo "This script targets Ubuntu. On other distros install: a C toolchain, rustup, Chrome or Chromium, bubblewrap." >&2
  exit 1
fi

say "apt packages (build tools, bubblewrap, fonts)"
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential pkg-config curl ca-certificates bubblewrap fonts-liberation

# Ubuntu's `chromium-browser` package is a snap shim, and rustplay refuses
# snap browsers (private /tmp, rebuilt environment). Use Google's .deb instead (it also ships the setuid
# chrome-sandbox helper, so Chromium's sandbox works as a normal user).
if ! command -v google-chrome >/dev/null 2>&1; then
  say "Google Chrome stable"
  tmp=$(mktemp -d)
  curl -fsSL -o "$tmp/chrome.deb" https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
  sudo apt-get install -y -qq "$tmp/chrome.deb"
  rm -rf "$tmp"
else
  say "Google Chrome already installed: $(google-chrome --version)"
fi

if ! command -v cargo >/dev/null 2>&1; then
  say "Rust toolchain (rustup, stable)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt,clippy
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
else
  say "Rust already installed: $(cargo --version)"
fi

need="1.87"
have=$(rustc --version | awk '{print $2}')
if [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | head -1)" != "$need" ]; then
  say "rustc $have is older than $need; updating"
  rustup update stable
fi

say "done"
cat <<'EOF'
Next:
  cd ~/rustplay
  cargo build --release
  ./target/release/rustplay doctor --pretty
EOF
