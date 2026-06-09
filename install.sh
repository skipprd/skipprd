#!/usr/bin/env bash

set -euo pipefail

cleanup() {
  rm -f "${TEMP_PATH:-}" 2>/dev/null
  rm -rf "${TEMP_DIR:-}" 2>/dev/null
}
trap cleanup EXIT

OS="$(uname)"
ARCH="$(uname -m)"

command -v curl >/dev/null 2>&1 || { echo >&2 "Error: curl is required but not installed."; exit 1; }
command -v tar >/dev/null 2>&1 || { echo >&2 "Error: tar is required but not installed."; exit 1; }

OWNER="skipprd"
REPO="skipprd"
DEST="${SKIPPR_INSTALL_DIR:-/usr/local/bin}"
REQUESTED_BINARY="${SKIPPR_BINARY:-skippr}"
case "$REQUESTED_BINARY" in
  skippr|skipprd|skippr-admin) ;;
  *)
    echo >&2 "Error: unsupported SKIPPR_BINARY=$REQUESTED_BINARY (expected skippr, skipprd, or skippr-admin)"
    exit 1
    ;;
esac

if [[ "$OS" == "Darwin" && "$ARCH" == "arm64" ]]; then
  ASSET_PATTERN="macos_arm64.tar.gz"
elif [[ "$OS" == "Darwin" ]]; then
  ASSET_PATTERN="macos_x86.tar.gz"
elif [[ "$OS" == "Linux" && "$ARCH" == "aarch64" ]]; then
  ASSET_PATTERN="linux_arm64.tar.gz"
elif [[ "$OS" == "Linux" ]]; then
  ASSET_PATTERN="linux_x86.tar.gz"
else
  echo >&2 "Error: unsupported platform: OS=$OS ARCH=$ARCH"
  exit 1
fi

if [ ! -w "$DEST" ]; then
  echo >&2 "Error: $DEST is not writable. Re-run with sudo or set SKIPPR_INSTALL_DIR to a writable path."
  exit 1
fi

# Resolve release URL — use SKIPPR_VERSION to pin, otherwise latest
if [[ -n "${SKIPPR_VERSION:-}" ]]; then
  RELEASE_URL="https://api.github.com/repos/$OWNER/$REPO/releases/tags/$SKIPPR_VERSION"
  echo "Installing $REQUESTED_BINARY $SKIPPR_VERSION for $OS/$ARCH..."
else
  RELEASE_URL="https://api.github.com/repos/$OWNER/$REPO/releases/latest"
  echo "Installing latest $REQUESTED_BINARY for $OS/$ARCH..."
fi

DOWNLOAD_URL=$(curl -sf "$RELEASE_URL" | grep "browser_download_url.*${REQUESTED_BINARY}-${ASSET_PATTERN}" | cut -d '"' -f 4 | head -1)

if [[ -z "${DOWNLOAD_URL:-}" ]]; then
  echo >&2 "Error: no release asset matching $ASSET_PATTERN found at $RELEASE_URL"
  exit 1
fi

BIN_NAME=$(basename "$DOWNLOAD_URL")
TEMP_PATH="/tmp/$BIN_NAME"
TEMP_DIR="/tmp/skipprd_install_$$"

echo "Downloading $BIN_NAME..."
curl --progress-bar -fL "$DOWNLOAD_URL" -o "$TEMP_PATH"

mkdir -p "$TEMP_DIR"
tar -xzf "$TEMP_PATH" -C "$TEMP_DIR"

# Find the extracted binary
EXTRACTED=$(find "$TEMP_DIR" -name "$REQUESTED_BINARY" -type f | head -1)
if [[ -z "$EXTRACTED" ]]; then
  echo >&2 "Error: $REQUESTED_BINARY binary not found in archive."
  exit 1
fi

mv "$EXTRACTED" "$DEST/$REQUESTED_BINARY"
chmod +x "$DEST/$REQUESTED_BINARY"

echo "Installed $REQUESTED_BINARY to $DEST/$REQUESTED_BINARY"
"$DEST/$REQUESTED_BINARY" --version 2>/dev/null || true
