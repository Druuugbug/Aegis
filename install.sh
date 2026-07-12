#!/bin/sh
# Aegis installer: downloads the latest release binary (Linux only).
#   curl -fsSL https://raw.githubusercontent.com/Druuugbug/Aegis/main/install.sh | sh
set -eu

REPO="Druuugbug/Aegis"
BIN="aegis"

os=$(uname -s)
arch=$(uname -m)

if [ "$os" != "Linux" ]; then
  echo "error: aegis targets Linux only (detected: $os)" >&2
  echo "hint: on other systems, build from source: cargo install aegis-agent" >&2
  exit 1
fi

case "$arch" in
  x86_64)          asset="linux-x86_64" ;;
  aarch64 | arm64) asset="linux-aarch64" ;;
  *) echo "error: unsupported architecture: $arch" >&2; exit 1 ;;
esac

if [ "$(id -u)" = "0" ]; then
  install_dir="/usr/local/bin"
else
  install_dir="${HOME}/.local/bin"
fi
mkdir -p "$install_dir"

url="https://github.com/${REPO}/releases/latest/download/${BIN}-${asset}.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${BIN} (${asset})..."
curl -fSL --proto '=https' "$url" -o "$tmp/${BIN}.tar.gz"
tar xzf "$tmp/${BIN}.tar.gz" -C "$tmp"
install -m 755 "$tmp/${BIN}" "$install_dir/${BIN}"

echo "Installed to ${install_dir}/${BIN}"
"$install_dir/${BIN}" --version 2>/dev/null || echo "Run '${BIN} --help' to get started."

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "note: add ${install_dir} to your PATH" ;;
esac
