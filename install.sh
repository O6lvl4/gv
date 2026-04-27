#!/usr/bin/env bash
# gv installer — fetches the latest (or pinned) gv release and drops `gv` and
# `gv-shim` into ~/.local/bin (override with GV_INSTALL_DIR).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/O6lvl4/gv/main/install.sh | sh
#   GV_VERSION=v0.1.0 sh install.sh
#   GV_INSTALL_DIR=/usr/local/bin sh install.sh

set -euo pipefail

REPO="O6lvl4/gv"
INSTALL_DIR="${GV_INSTALL_DIR:-$HOME/.local/bin}"
PIN="${GV_VERSION:-}"

err() { printf 'gv-install: %s\n' "$*" >&2; exit 1; }
say() { printf 'gv-install: %s\n' "$*"; }

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64) echo "x86_64-apple-darwin" ;;
        *) err "unsupported macOS arch: $arch" ;;
      esac ;;
    Linux)
      case "$arch" in
        aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
        x86_64|amd64) echo "x86_64-unknown-linux-musl" ;;
        *) err "unsupported Linux arch: $arch" ;;
      esac ;;
    *) err "unsupported OS: $os" ;;
  esac
}

resolve_tag() {
  if [ -n "$PIN" ]; then
    echo "$PIN"
    return
  fi
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
    | head -n1
}

main() {
  command -v curl >/dev/null 2>&1 || err "curl is required"
  command -v tar  >/dev/null 2>&1 || err "tar is required"

  local target tag asset url tmpdir
  target="$(detect_target)"
  tag="$(resolve_tag)"
  [ -n "$tag" ] || err "could not resolve a release tag (set GV_VERSION=vX.Y.Z to pin)"

  asset="gv-${tag}-${target}.tar.gz"
  url="https://github.com/${REPO}/releases/download/${tag}/${asset}"

  tmpdir="$(mktemp -d)"
  # The trap fires after main returns, when locals are out of scope. Use a
  # parameter default so set -u doesn't complain about an "unset" tmpdir.
  trap 'rm -rf "${tmpdir:-}"' EXIT

  say "downloading $asset"
  curl -fsSL "$url" -o "${tmpdir}/${asset}"

  say "verifying sha256"
  if curl -fsSL "${url}.sha256" -o "${tmpdir}/${asset}.sha256"; then
    if command -v shasum >/dev/null 2>&1; then
      ( cd "$tmpdir" && shasum -a 256 -c "${asset}.sha256" )
    elif command -v sha256sum >/dev/null 2>&1; then
      ( cd "$tmpdir" && sha256sum -c "${asset}.sha256" )
    else
      say "no shasum/sha256sum found; skipping verification"
    fi
  else
    say "no .sha256 file published; skipping verification"
  fi

  say "extracting"
  tar -xzf "${tmpdir}/${asset}" -C "${tmpdir}"
  local stage="${tmpdir}/gv-${tag}-${target}"

  mkdir -p "$INSTALL_DIR"
  install -m 0755 "${stage}/gv"      "${INSTALL_DIR}/gv"
  install -m 0755 "${stage}/gv-shim" "${INSTALL_DIR}/gv-shim"

  # Create the `gvx` ephemeral-run shim. argv[0] dispatch in the gv binary
  # rewrites `gvx <tool> [args]` to `gv x <tool> [args]`.
  ln -sfn gv "${INSTALL_DIR}/gvx"

  say "installed to ${INSTALL_DIR}"
  say "  gv      = $(${INSTALL_DIR}/gv --version 2>/dev/null || echo 'not on PATH yet')"

  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      say ""
      say "${INSTALL_DIR} is not on your PATH. Add this to your shell rc:"
      say "  export PATH=\"\$HOME/.local/bin:\$PATH\""
      ;;
  esac

  say "done. Try: gv install latest"
}

main "$@"
