#!/bin/sh
# keepty installer — detects platform, downloads binary from GitHub Releases.
# Usage: curl -sSf https://raw.githubusercontent.com/BorjaGM1/keepty/main/install.sh | sh
set -e

REPO="BorjaGM1/keepty"
INSTALL_DIR="${KEEPTY_INSTALL_DIR:-$HOME/.local/bin}"

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin)
        case "$ARCH" in
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            *) echo "Error: macOS $ARCH is not supported. Only arm64 (Apple Silicon) is available."; exit 1 ;;
        esac
        ;;
    Linux)
        case "$ARCH" in
            x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
            aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
            *) echo "Error: Linux $ARCH is not supported. Only x86_64 and aarch64 are available."; exit 1 ;;
        esac
        ;;
    *) echo "Error: $OS is not supported. Only macOS and Linux are available."; exit 1 ;;
esac

# Get latest release tag
if command -v curl >/dev/null 2>&1; then
    LATEST=$(curl -sSf "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
elif command -v wget >/dev/null 2>&1; then
    LATEST=$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
else
    echo "Error: curl or wget is required"; exit 1
fi

if [ -z "$LATEST" ]; then
    echo "Error: could not determine latest release. Is the repo public?"
    exit 1
fi

TARBALL="keepty-${TARGET}.tar.gz"
URL="https://github.com/$REPO/releases/download/$LATEST/$TARBALL"
CHECKSUM_URL="https://github.com/$REPO/releases/download/$LATEST/checksums.txt"

echo "Installing keepty $LATEST for $TARGET..."
echo "  Binary: $URL"
echo "  Install dir: $INSTALL_DIR"

# Create install directory
mkdir -p "$INSTALL_DIR"

# Download and verify
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

if command -v curl >/dev/null 2>&1; then
    curl -sSfL "$URL" -o "$TMPDIR/$TARBALL"
    curl -sSfL "$CHECKSUM_URL" -o "$TMPDIR/checksums.txt"
else
    wget -q "$URL" -O "$TMPDIR/$TARBALL"
    wget -q "$CHECKSUM_URL" -O "$TMPDIR/checksums.txt"
fi

# Verify checksum
EXPECTED=$(grep "$TARBALL" "$TMPDIR/checksums.txt" | awk '{print $1}')
if [ -n "$EXPECTED" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL=$(sha256sum "$TMPDIR/$TARBALL" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL=$(shasum -a 256 "$TMPDIR/$TARBALL" | awk '{print $1}')
    else
        echo "Warning: cannot verify checksum (no sha256sum or shasum)"
        ACTUAL="$EXPECTED"
    fi

    if [ "$EXPECTED" != "$ACTUAL" ]; then
        echo "Error: checksum mismatch!"
        echo "  Expected: $EXPECTED"
        echo "  Got:      $ACTUAL"
        exit 1
    fi
fi

# Extract and install
tar xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
chmod +x "$TMPDIR/keepty"
mv "$TMPDIR/keepty" "$INSTALL_DIR/keepty"

echo ""
echo "keepty $LATEST installed to $INSTALL_DIR/keepty"

# Check if install dir is in PATH
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo ""
        echo "Add to your PATH:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        echo ""
        echo "Or add to your shell profile (~/.bashrc, ~/.zshrc)."
        ;;
esac
