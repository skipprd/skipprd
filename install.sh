#!/bin/sh
set -e

# Public install CDN (Cloudflare Worker → R2 skippr-web-install). Not GitHub Releases.
RELEASES_BASE_URL="${SKIPPR_RELEASES_BASE_URL:-https://install.skippr.io/releases}"
INSTALL_DIR="${SKIPPR_INSTALL_DIR:-/usr/local/bin}"
BINARY="${SKIPPR_BINARY:-skippr}"

case "$BINARY" in
  skippr)
    RELEASE_SUBDIR="skippr"
    LATEST_RELEASE_URL="${RELEASES_BASE_URL}/latest-skippr.txt"
    ;;
  skipprd)
    RELEASE_SUBDIR="skipprd"
    LATEST_RELEASE_URL="${RELEASES_BASE_URL}/latest-skipprd.txt"
    ;;
  skippr-admin)
    RELEASE_SUBDIR="skippr-admin"
    LATEST_RELEASE_URL="${RELEASES_BASE_URL}/latest-skippr-admin.txt"
    ;;
  *)
    err() { printf 'Error: %s\n' "$1" >&2; exit 1; }
    err "unsupported SKIPPR_BINARY=$BINARY (expected skippr, skipprd, or skippr-admin)"
    ;;
esac

say() {
    printf '%s\n' "$1"
}

err() {
    say "Error: $1" >&2
    exit 1
}

need_cmd() {
    if ! command -v "$1" > /dev/null 2>&1; then
        err "Required command not found: $1"
    fi
}

fetch_latest_tag() {
    if [ -n "${SKIPPR_VERSION:-}" ]; then
        printf '%s' "$SKIPPR_VERSION"
        return
    fi
    curl -fsSL "$LATEST_RELEASE_URL" | tr -d '\r\n'
}

validate_archive() {
    local archive_path="$1"
    local mime_type

    if command -v file > /dev/null 2>&1; then
        mime_type="$(file -b --mime-type "$archive_path" 2>/dev/null || true)"
        case "$mime_type" in
            application/gzip|application/x-gzip) return 0 ;;
            text/html|text/plain)
                err "Download did not return a tar.gz archive. Check that the release asset exists for this platform."
                ;;
        esac
    fi
}

main() {
    need_cmd curl
    need_cmd tar
    need_cmd uname

    say "Installing Skippr means accepting the Skippr EULA:"
    say "  https://skippr.io/terms/eula"
    say ""

    local os arch target

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) target="macos_arm64" ;;
                x86_64)        err "macOS x86_64 release assets are not published yet" ;;
                *)             err "Unsupported macOS architecture: $arch" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64)  target="linux_x86" ;;
                aarch64|arm64) err "Linux arm64 release assets are not published yet" ;;
                *)             err "Unsupported Linux architecture: $arch" ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*)
            err "Windows detected. Install with PowerShell instead:  irm https://install.skippr.io/install.ps1 | iex"
            ;;
        *)
            err "Unsupported operating system: $os"
            ;;
    esac

    local tag url tmpdir

    say "Detecting latest release..."
    tag="$(fetch_latest_tag)"

    if [ -z "$tag" ]; then
        err "Could not determine latest release."
    fi

    say "Latest release: $tag ($BINARY)"

    url="${RELEASES_BASE_URL}/${RELEASE_SUBDIR}/${tag}/${BINARY}-${target}.tar.gz"

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    say "Downloading $BINARY for $target from install.skippr.io..."
    curl -fSL --progress-bar "$url" -o "$tmpdir/skippr.tar.gz"
    validate_archive "$tmpdir/skippr.tar.gz"

    say "Extracting..."
    tar -xzf "$tmpdir/skippr.tar.gz" -C "$tmpdir"

    local bin_path="$tmpdir/$BINARY-$target/$BINARY"
    if [ ! -f "$bin_path" ]; then
        bin_path="$(find "$tmpdir" -name "$BINARY" -type f | head -1)"
    fi

    if [ ! -f "$bin_path" ]; then
        err "Binary not found in archive."
    fi

    chmod +x "$bin_path"

    if [ -w "$INSTALL_DIR" ]; then
        mv "$bin_path" "$INSTALL_DIR/$BINARY"
    else
        say "Installing to $INSTALL_DIR (requires sudo)..."
        sudo mv "$bin_path" "$INSTALL_DIR/$BINARY"
    fi

    say ""
    say "  $BINARY $tag installed to $INSTALL_DIR/$BINARY"
    say ""
    if [ "$BINARY" = "skippr" ]; then
        say "  Get started:"
        say "    skippr user login"
        say "    skippr init my-project"
        say ""
    fi
}

main "$@"
