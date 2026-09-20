#!/usr/bin/env bash
# ==============================================================================
# create_dummy_deb.sh - Build Debian dummy package for UTIM init compatibility
# Satisfies Debian dependencies for: init, systemd-sysv, init-system-helpers, systemd
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUTPUT_DIR="${1:-${WORKSPACE_ROOT}/dist}"

mkdir -p "${OUTPUT_DIR}"
BUILD_DIR="$(mktemp -d -t utim-deb-XXXXXX)"

cleanup() {
    rm -rf "${BUILD_DIR}"
}
trap cleanup EXIT

echo "[*] Constructing utim-init-dummy Debian package..."

mkdir -p "${BUILD_DIR}/DEBIAN"
mkdir -p "${BUILD_DIR}/usr/share/doc/utim-init-dummy"

cat << 'EOF' > "${BUILD_DIR}/DEBIAN/control"
Package: utim-init-dummy
Version: 1.0.0
Section: admin
Priority: required
Architecture: all
Provides: init, systemd-sysv, init-system-helpers, systemd
Conflicts: sysvinit-core
Replaces: systemd-sysv
Maintainer: Universal Treble Linux <developer@treble-linux.org>
Description: Dummy package satisfying init dependencies for UTIM
 UTIM (Universal Treble Init Manager) is a phone-optimized bare-metal
 PID 1 replacement for systemd on Android Project Treble devices.
 This package satisfies Debian package manager dependencies for init,
 systemd, systemd-sysv, and init-system-helpers without pulling desktop
 systemd bloat.
EOF

cat << 'EOF' > "${BUILD_DIR}/usr/share/doc/utim-init-dummy/copyright"
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: utim-init-dummy
Source: https://github.com/universal-android/utim

Files: *
Copyright: 2026 Universal Treble Linux Contributors
License: Apache-2.0 or MIT
EOF

DEB_FILE="${OUTPUT_DIR}/utim-init-dummy.deb"
dpkg-deb --build --root-owner-group "${BUILD_DIR}" "${DEB_FILE}"

echo "[+] Successfully created Debian package: ${DEB_FILE}"
dpkg-deb -I "${DEB_FILE}"
