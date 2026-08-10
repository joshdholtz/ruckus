#!/bin/sh
# ruckus installer — downloads the right prebuilt binary from the latest
# GitHub release and drops it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/joshdholtz/ruckus/main/install.sh | sh
#
# Env overrides:
#   RUCKUS_VERSION=v0.1.0   pin a version (default: latest release)
#   RUCKUS_INSTALL_DIR=...  install dir (default: /usr/local/bin, else ~/.local/bin)
set -eu

REPO="joshdholtz/ruckus"
VERSION="${RUCKUS_VERSION:-latest}"

say()  { printf '%s\n' "$*"; }
err()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- detect platform → release asset target triple ---
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Darwin) plat="apple-darwin" ;;
  Linux)  plat="unknown-linux-gnu" ;;
  *) err "unsupported OS: $os (try: cargo install --git https://github.com/$REPO)" ;;
esac
case "$arch" in
  arm64|aarch64) cpu="aarch64" ;;
  x86_64|amd64)  cpu="x86_64" ;;
  *) err "unsupported arch: $arch" ;;
esac
target="${cpu}-${plat}"

# --- resolve version ---
if [ "$VERSION" = "latest" ]; then
  if have gh; then
    VERSION="$(gh release view --repo "$REPO" --json tagName --jq .tagName 2>/dev/null || true)"
  fi
  if [ -z "${VERSION:-}" ] || [ "$VERSION" = "latest" ]; then
    api="https://api.github.com/repos/$REPO/releases/latest"
    VERSION="$(curl -fsSL "$api" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  fi
fi
[ -n "${VERSION:-}" ] || err "could not resolve latest version"

asset="ruckus-${VERSION}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${VERSION}/${asset}"

# --- choose install dir ---
if [ -n "${RUCKUS_INSTALL_DIR:-}" ]; then
  dir="$RUCKUS_INSTALL_DIR"
elif [ -w /usr/local/bin ] 2>/dev/null; then
  dir="/usr/local/bin"
else
  dir="$HOME/.local/bin"
fi
mkdir -p "$dir"

say "Installing ruckus $VERSION ($target) → $dir"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fSL "$url" -o "$tmp/ruckus.tar.gz" \
  || err "download failed: $url (no binary for $target? try cargo install)"
tar -xzf "$tmp/ruckus.tar.gz" -C "$tmp"

if [ -w "$dir" ]; then
  install -m 0755 "$tmp/ruckus" "$dir/ruckus"
else
  say "  (need sudo to write $dir)"
  sudo install -m 0755 "$tmp/ruckus" "$dir/ruckus"
fi

# macOS: clear the quarantine bit so Gatekeeper doesn't block the unsigned binary
[ "$os" = "Darwin" ] && xattr -d com.apple.quarantine "$dir/ruckus" 2>/dev/null || true

say ""
say "✓ installed: $("$dir/ruckus" --version 2>/dev/null || echo "$dir/ruckus")"
case ":$PATH:" in
  *":$dir:"*) : ;;
  *) say ""; say "⚠ $dir is not on your PATH. Add it:"; say "    export PATH=\"$dir:\$PATH\"" ;;
esac
say ""
say "Run:  ruckus"
