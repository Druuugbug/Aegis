#!/bin/sh
# Aegis installer: downloads the latest release binary for your platform.
#   curl -fsSL https://raw.githubusercontent.com/Druuugbug/Aegis/main/install.sh | sh
set -eu

REPO="Druuugbug/Aegis"
BIN="aegis"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Linux)
    case "$arch" in
      x86_64)          target="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
      *) echo "error: unsupported architecture: $arch" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      x86_64)          target="x86_64-apple-darwin" ;;
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      *) echo "error: unsupported architecture: $arch" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "error: unsupported OS: $os (Linux and macOS only)" >&2
    exit 1
    ;;
esac

if [ "$(id -u)" = "0" ]; then
  install_dir="/usr/local/bin"
else
  install_dir="${HOME}/.local/bin"
fi
mkdir -p "$install_dir"

url="https://github.com/${REPO}/releases/latest/download/${BIN}-${target}.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${BIN} (${target})..."
curl -fSL --proto '=https' "$url" -o "$tmp/${BIN}.tar.gz"
tar xzf "$tmp/${BIN}.tar.gz" -C "$tmp"
install -m 755 "$tmp/${BIN}" "$install_dir/${BIN}"

echo "Installed to ${install_dir}/${BIN}"
"$install_dir/${BIN}" --version || true

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "note: add ${install_dir} to your PATH" ;;
esac
