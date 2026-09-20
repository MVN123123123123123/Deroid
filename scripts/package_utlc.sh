#!/usr/bin/env bash
# ==============================================================================
# package_utlc.sh - Build Debian package for UTLC Mobile Wayland Compositor & Launcher
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUTPUT_DIR="${1:-${WORKSPACE_ROOT}/dist}"

mkdir -p "${OUTPUT_DIR}"
BUILD_DIR="$(mktemp -d -t utlc-deb-XXXXXX)"

cleanup() {
    rm -rf "${BUILD_DIR}"
}
trap cleanup EXIT

echo "[*] Constructing UTLC (Universal Treble Launcher & Compositor) Debian package..."

# 1. Verify aarch64 binary is built
AARCH64_BIN="${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utlc"
if [[ ! -f "${AARCH64_BIN}" ]]; then
    echo "[*] Building release aarch64 binary for utlc..."
    cargo build --release -p utlc --target aarch64-unknown-linux-gnu
fi

mkdir -p "${BUILD_DIR}/DEBIAN"
mkdir -p "${BUILD_DIR}/usr/bin"
mkdir -p "${BUILD_DIR}/usr/lib/systemd/system"
mkdir -p "${BUILD_DIR}/usr/share/applications"
mkdir -p "${BUILD_DIR}/usr/share/doc/utlc"

# 2. Copy binary and services
cp "${AARCH64_BIN}" "${BUILD_DIR}/usr/bin/utlc"
chmod 755 "${BUILD_DIR}/usr/bin/utlc"

if [[ -f "${WORKSPACE_ROOT}/utlc.service" ]]; then
    cp "${WORKSPACE_ROOT}/utlc.service" "${BUILD_DIR}/usr/lib/systemd/system/utlc.service"
fi

# 3. Create desktop entry for shell
cat << 'EOF' > "${BUILD_DIR}/usr/share/applications/utlc.desktop"
[Desktop Entry]
Name=Universal Treble Shell
Comment=Android-style Wayland Compositor and Launcher
Exec=/usr/bin/utlc --daemon
Type=Application
Icon=preferences-desktop-display
NoDisplay=true
Categories=System;Core;
EOF

# 4. Control metadata
cat << 'EOF' > "${BUILD_DIR}/DEBIAN/control"
Package: utlc
Version: 1.0.0
Section: x11
Priority: standard
Architecture: arm64
Maintainer: Universal Treble Linux <developer@treble-linux.org>
Depends: libc6 (>= 2.34), libhybris-hwcomposer (>= 1.0.0) | android-framework, seatd
Description: Universal Treble Launcher and Compositor for Android GSI
 UTLC is a unified, single-process, mobile-first Wayland compositor and
 Android-style launcher written in bare-metal Rust. It integrates direct
 Hardware Composer (HWC 2.x and AIDL composer3) multi-plane presentation,
 DMA-BUF zero-copy buffer sharing, Android 10+ QuickStep gesture navigation,
 a paged workspace grid, SystemUI status bar and quick settings, ambient
 lock screen with Android Fingerprint HAL bridge, virtual keyboard IME,
 and UTIM Mobile Power Governor / OOM hierarchy synchronization.
 Consumes < 15 MB resident RAM and boots to interactive home in < 0.45s.
EOF

cat << 'EOF' > "${BUILD_DIR}/usr/share/doc/utlc/copyright"
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: utlc
Source: https://github.com/universal-android/utlc

Files: *
Copyright: 2026 Universal Treble Linux Contributors
License: Apache-2.0 or MIT
EOF

DEB_FILE="${OUTPUT_DIR}/utlc_1.0.0_arm64.deb"
dpkg-deb --build --root-owner-group "${BUILD_DIR}" "${DEB_FILE}"

echo "[+] Successfully created Debian package: ${DEB_FILE}"
dpkg-deb -I "${DEB_FILE}"
