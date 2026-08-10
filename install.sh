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
# Prefer a home bin dir that's ALREADY on PATH (no profile edit, no symlink
# needed); only fall back to ~/.local/bin (which ensure_path will wire up).
pick_home_dir() {
  for d in "$HOME/.local/bin" "$HOME/bin"; do
    case ":$PATH:" in *":$d:"*) printf '%s' "$d"; return 0 ;; esac
  done
  printf '%s' "$HOME/.local/bin"
}
if [ -n "${RUCKUS_INSTALL_DIR:-}" ]; then
  dir="$RUCKUS_INSTALL_DIR"
elif [ -w /usr/local/bin ] 2>/dev/null; then
  dir="/usr/local/bin"
else
  dir="$(pick_home_dir)"
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

# --- PATH: add $dir if it isn't reachable (idempotent; opt out with RUCKUS_NO_MODIFY_PATH=1) ---
ensure_path() {
  case ":$PATH:" in *":$dir:"*) return 0 ;; esac   # already reachable

  line="export PATH=\"$dir:\$PATH\""
  if [ "${RUCKUS_NO_MODIFY_PATH:-0}" = "1" ]; then
    say ""; say "⚠ $dir is not on your PATH. Add it yourself:"; say "    $line"
    return 0
  fi

  case "$(basename "${SHELL:-sh}")" in
    zsh)  prof="${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash) [ -f "$HOME/.bashrc" ] && prof="$HOME/.bashrc" || prof="$HOME/.bash_profile" ;;
    fish) prof="$HOME/.config/fish/config.fish"; line="fish_add_path $dir" ;;
    *)    prof="$HOME/.profile" ;;
  esac

  # idempotent: skip if this dir is already wired into the profile
  if [ -f "$prof" ] && grep -qF "$dir" "$prof" 2>/dev/null; then
    say ""; say "⚠ $dir not on PATH, but already referenced in $prof — restart your shell."
    return 0
  fi

  mkdir -p "$(dirname "$prof")"
  { printf '\n# added by ruckus installer\n%s\n' "$line" >> "$prof"; } 2>/dev/null \
    && { say ""; say "→ added $dir to PATH in $prof"; say "  restart your shell (or: source $prof)"; } \
    || { say ""; say "⚠ couldn't edit $prof. Add it yourself:"; say "    $line"; }
}
ensure_path

say ""
say "Run:  ruckus"
