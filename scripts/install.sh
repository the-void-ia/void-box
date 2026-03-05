#!/bin/sh
set -eu

# void-box installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/the-void-ia/void-box/main/scripts/install.sh | sh
#   VERSION=v0.1.1 curl -fsSL ... | sh
#
# Installs:
#   /usr/local/bin/voidbox
#   /usr/local/lib/voidbox/vmlinuz     (Linux)
#   /usr/local/lib/voidbox/vmlinux     (macOS)
#   /usr/local/lib/voidbox/initramfs.cpio.gz

REPO="the-void-ia/void-box"
INSTALL_BIN="/usr/local/bin"
INSTALL_LIB="/usr/local/lib/voidbox"

main() {
    detect_platform
    resolve_version
    download_and_install
    verify_install
}

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)  PLATFORM="linux" ;;
        Darwin) PLATFORM="darwin" ;;
        *)
            echo "Error: unsupported OS: $OS" >&2
            exit 1
            ;;
    esac

    case "$ARCH" in
        x86_64|amd64) ARCH="x86_64" ;;
        aarch64|arm64) ARCH="aarch64" ;;
        *)
            echo "Error: unsupported architecture: $ARCH" >&2
            exit 1
            ;;
    esac

    echo "Detected platform: ${PLATFORM}-${ARCH}"
}

resolve_version() {
    if [ -n "${VERSION:-}" ]; then
        echo "Using specified version: $VERSION"
        return
    fi

    echo "Fetching latest version..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | head -1 \
        | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

    if [ -z "$VERSION" ]; then
        echo "Error: could not determine latest version" >&2
        echo "Set VERSION=vX.Y.Z and retry" >&2
        exit 1
    fi

    echo "Latest version: $VERSION"
}

download_and_install() {
    TARBALL_NAME="voidbox-${VERSION}-${PLATFORM}-${ARCH}.tar.gz"
    TARBALL_URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL_NAME}"

    TMPDIR_INSTALL="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR_INSTALL"' EXIT

    echo "Downloading ${TARBALL_URL}..."
    curl -fsSL "$TARBALL_URL" -o "${TMPDIR_INSTALL}/${TARBALL_NAME}"

    echo "Extracting..."
    tar -xzf "${TMPDIR_INSTALL}/${TARBALL_NAME}" -C "$TMPDIR_INSTALL"

    # Use sudo only if we can't write to the install directories
    SUDO=""
    if [ ! -w "$INSTALL_BIN" ] 2>/dev/null; then
        if command -v sudo >/dev/null 2>&1; then
            SUDO="sudo"
            echo "Using sudo for installation to ${INSTALL_BIN} and ${INSTALL_LIB}"
        else
            echo "Error: no write access to $INSTALL_BIN and sudo not available" >&2
            exit 1
        fi
    fi

    $SUDO mkdir -p "$INSTALL_BIN" "$INSTALL_LIB"
    $SUDO install -m 755 "${TMPDIR_INSTALL}/voidbox" "$INSTALL_BIN/voidbox"

    # Install kernel — macOS uses vmlinux, Linux uses vmlinuz
    if [ -f "${TMPDIR_INSTALL}/vmlinux" ]; then
        $SUDO install -m 644 "${TMPDIR_INSTALL}/vmlinux" "$INSTALL_LIB/vmlinux"
    fi
    if [ -f "${TMPDIR_INSTALL}/vmlinuz" ]; then
        $SUDO install -m 644 "${TMPDIR_INSTALL}/vmlinuz" "$INSTALL_LIB/vmlinuz"
    fi

    $SUDO install -m 644 "${TMPDIR_INSTALL}/initramfs.cpio.gz" "$INSTALL_LIB/initramfs.cpio.gz"
}

verify_install() {
    echo ""
    if command -v voidbox >/dev/null 2>&1; then
        echo "voidbox installed successfully!"
        echo "  Binary:    ${INSTALL_BIN}/voidbox"
        echo "  Artifacts: ${INSTALL_LIB}/"
        echo ""
        voidbox version 2>/dev/null || true
    else
        echo "Installation complete, but voidbox is not in PATH."
        echo "Add ${INSTALL_BIN} to your PATH:"
        echo "  export PATH=\"${INSTALL_BIN}:\$PATH\""
    fi
}

main
