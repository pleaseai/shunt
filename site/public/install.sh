#!/bin/sh
# shunt installer: downloads a prebuilt binary from the GitHub release,
# verifies it against the release's SHA256SUMS, and installs it.
#
#   curl -fsSL https://shunt.sh/install.sh | sh
#
# Environment:
#   SHUNT_VERSION      release to install (e.g. 0.55.1 or v0.55.1); defaults to
#                      the release this script was published with
#   SHUNT_INSTALL_DIR  destination directory (default: $HOME/.local/bin)
set -eu

REPO="pleaseai/shunt"
# Stamped by release-please on every release (release-please-config.json).
DEFAULT_VERSION="0.57.1" # x-release-please-version

err() {
  printf 'shunt-install: error: %s\n' "$*" >&2
  exit 1
}

info() {
  printf 'shunt-install: %s\n' "$*"
}

detect_os() {
  case "$(uname -s)" in
    Darwin) echo darwin ;;
    Linux) echo linux ;;
    *) err "unsupported OS: $(uname -s) (prebuilt binaries cover macOS and Linux)" ;;
  esac
}

detect_arch() {
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch=x64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) err "unsupported CPU architecture: $arch (prebuilt binaries cover arm64 and x64)" ;;
  esac
  # An x64 shell under Rosetta 2 still runs on Apple silicon; install native.
  if [ "$1" = darwin ] && [ "$arch" = x64 ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
    arch=arm64
  fi
  echo "$arch"
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    err "curl or wget is required"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    err "sha256sum or shasum is required to verify the download"
  fi
}

main() {
  version="${SHUNT_VERSION:-$DEFAULT_VERSION}"
  version="${version#v}"
  if [ -z "${SHUNT_INSTALL_DIR:-}" ] && [ -z "${HOME:-}" ]; then
    err "HOME is unset; set SHUNT_INSTALL_DIR"
  fi
  install_dir="${SHUNT_INSTALL_DIR:-${HOME}/.local/bin}"
  # A quoted SHUNT_INSTALL_DIR="~/bin" reaches us unexpanded.
  # shellcheck disable=SC2088 # matching the literal, unexpanded tilde
  case "$install_dir" in
    "~" | "~/"*)
      [ -n "${HOME:-}" ] || err "HOME is unset; cannot expand ${install_dir}"
      install_dir="${HOME}${install_dir#"~"}"
      ;;
  esac
  # Drop trailing slashes so the PATH check below matches.
  while :; do
    case "$install_dir" in
      ?*/) install_dir="${install_dir%/}" ;;
      *) break ;;
    esac
  done

  os="$(detect_os)"
  arch="$(detect_arch "$os")"
  asset="shunt-${os}-${arch}"
  base_url="https://github.com/${REPO}/releases/download/v${version}"

  tmp="$(mktemp -d)"
  staged=""
  trap 'rm -rf "$tmp" || :; [ -z "$staged" ] || rm -f "$staged" || :' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  info "downloading ${asset} v${version}"
  download "${base_url}/${asset}" "${tmp}/${asset}" ||
    err "failed to download ${base_url}/${asset}"
  download "${base_url}/SHA256SUMS" "${tmp}/SHA256SUMS" ||
    err "failed to download ${base_url}/SHA256SUMS"

  expected="$(awk -v f="$asset" '$NF == f {print $1}' "${tmp}/SHA256SUMS")"
  [ -n "$expected" ] || err "no checksum for ${asset} in SHA256SUMS"
  actual="$(sha256 "${tmp}/${asset}")"
  [ "$expected" = "$actual" ] ||
    err "checksum mismatch for ${asset}: expected ${expected}, got ${actual}"

  mkdir -p "$install_dir" || err "cannot create ${install_dir}"
  # Stage next to the target so the final mv is a same-filesystem rename: an
  # interrupted copy never replaces a working shunt.
  staged="${install_dir}/.shunt.$$"
  if ! { cp "${tmp}/${asset}" "$staged" && chmod +x "$staged"; }; then
    err "cannot write ${install_dir} (set SHUNT_INSTALL_DIR to a writable directory)"
  fi
  [ ! -d "${install_dir}/shunt" ] || err "${install_dir}/shunt is a directory"
  mv -f "$staged" "${install_dir}/shunt" ||
    err "cannot replace ${install_dir}/shunt"
  staged=""

  info "installed shunt v${version} to ${install_dir}/shunt"
  case ":${PATH:-}:" in
    *":${install_dir}:"* | *":${install_dir}/:"*) ;;
    *) info "${install_dir} is not on your PATH; add it, e.g.: export PATH=\"${install_dir}:\$PATH\"" ;;
  esac
}

main "$@"
