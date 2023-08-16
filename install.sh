#!/bin/bash

set -e

# Determine the OS and Architecture.
OS="$(uname)"
ARCH="$(uname -m)"

# Check for curl and tar dependencies
command -v curl >/dev/null 2>&1 || { echo >&2 "curl is required but it's not installed. Please install and run again."; exit 1; }
command -v tar >/dev/null 2>&1 || { echo >&2 "tar is required but it's not installed. Please install and run again."; exit 1; }

OWNER="skipprd"
REPO="skipprd"

# Destination directory for the binary
DEST="/usr/local/bin"

# Define the binary asset pattern based on OS and Architecture
if [[ "$OS" == "Darwin" && "$ARCH" == "arm64" ]]; then
  ASSET_PATTERN="macos_arm64.tar.gz"
elif [[ "$OS" == "Darwin" ]]; then
  ASSET_PATTERN="macos_x86.tar.gz"
elif [[ "$OS" == "Linux" ]]; then
  ASSET_PATTERN="linux_x86.tar.gz"
else
  echo "Unsupported OS or Architecture. Exiting."
  exit 1
fi

# Get the latest release download URL for the binary based on the defined pattern.
DOWNLOAD_URL=$(curl -s "https://api.github.com/repos/$OWNER/$REPO/releases/latest" | grep "browser_download_url.*$ASSET_PATTERN" | cut -d "\"" -f 4)

# Exit if the download URL is empty.
if [[ -z "$DOWNLOAD_URL" ]]; then
  echo "Failed to find a binary asset from the latest release. Exiting."
  exit 1
fi

# Extract the binary name from the download URL.
BIN_NAME=$(basename "$DOWNLOAD_URL")

# Print download message with file size.
echo "Downloading $BIN_NAME from $DOWNLOAD_URL"

# Download the binary.
TEMP_PATH="/tmp/$BIN_NAME"
curl --progress-bar -L "$DOWNLOAD_URL" -o "$TEMP_PATH"

# Extract to a temp directory
TEMP_DIR="/tmp/skippr_extracted"
mkdir -p "$TEMP_DIR"
tar -xzf "$TEMP_PATH" -C "$TEMP_DIR"

# Move the binary to the desired location and clean up
mv "$TEMP_DIR"/*/skippr "$DEST/skippr"
rm -r "$TEMP_DIR"      # Remove the temporary directory
rm "$TEMP_PATH"        # Clean up the downloaded archive

# Make the binary executable.
chmod +x "$DEST/skippr"

echo "Installed skippr to $DEST"
