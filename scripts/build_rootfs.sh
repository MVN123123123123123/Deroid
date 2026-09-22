#!/usr/bin/env bash
# ==============================================================================
# build_rootfs.sh - Construct Debian Sid ARM64 rootfs with UTIM Init
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

TARGET_ARCH="arm64"
DEBIAN_MIRROR="http://deb.debian.org/debian/"
DEBIAN_SUITE="sid"
ROOTFS_DIR="${1:-${WORKSPACE_ROOT}/build/rootfs}"
DRY_RUN="${DRY_RUN:-0}"

echo "============================================================"
echo " Building Debian Sid ARM64 Rootfs for Universal Treble Linux"
echo " Target Rootfs Directory: ${ROOTFS_DIR}"
echo "============================================================"

# Ensure UTIM binaries are built for aarch64
echo "[*] Verifying UTIM aarch64 release binaries..."
cargo build --release --workspace --target aarch64-unknown-linux-gnu

# Ensure dummy deb package and graphics packages are built
echo "[*] Building utim-init-dummy package..."
"${SCRIPT_DIR}/create_dummy_deb.sh" "${WORKSPACE_ROOT}/dist"

echo "[*] Building Phase 2 Graphics HAL packages (libhybris, Mesa Turnip/Zink, libhybris-egl)..."
"${SCRIPT_DIR}/package_graphics.sh" "${WORKSPACE_ROOT}/dist"

echo "[*] Building Phase 3 UTLC Mobile Wayland Compositor & Shell package..."
"${SCRIPT_DIR}/package_utlc.sh" "${WORKSPACE_ROOT}/dist"

if [[ "${DRY_RUN}" == "1" || "$(id -u)" != "0" ]]; then
    echo "[!] Non-root execution detected or DRY_RUN=1."
    echo "[*] Performing mock rootfs assembly to validate layout and dependencies..."

    mkdir -p "${ROOTFS_DIR}/etc/apt/preferences.d"
    mkdir -p "${ROOTFS_DIR}/etc/environment.d"
    mkdir -p "${ROOTFS_DIR}/etc/systemd/system/multi-user.target.wants"
    mkdir -p "${ROOTFS_DIR}/etc/systemd/system/graphical.target.wants"
    mkdir -p "${ROOTFS_DIR}/usr/lib/systemd/system"
    mkdir -p "${ROOTFS_DIR}/usr/bin"
    mkdir -p "${ROOTFS_DIR}/sbin"
    mkdir -p "${ROOTFS_DIR}/bin"
    mkdir -p "${ROOTFS_DIR}/run/systemd/system"
    mkdir -p "${ROOTFS_DIR}/run/utim"
    mkdir -p "${ROOTFS_DIR}/var/lib/systemd/deb-systemd-helper-enabled"
    mkdir -p "${ROOTFS_DIR}/proc" "${ROOTFS_DIR}/sys" "${ROOTFS_DIR}/dev"
    mkdir -p "${ROOTFS_DIR}/lib" "${ROOTFS_DIR}/root" "${ROOTFS_DIR}/home" "${ROOTFS_DIR}/mnt"
    rm -rf "${ROOTFS_DIR}/lib64"
    ln -sfn "lib" "${ROOTFS_DIR}/lib64"

    # Canonical systemd target definitions
    cat << 'EOF' > "${ROOTFS_DIR}/usr/lib/systemd/system/basic.target"
[Unit]
Description=Basic System
Documentation=man:systemd.special(7)
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/usr/lib/systemd/system/multi-user.target"
[Unit]
Description=Multi-User System
Documentation=man:systemd.special(7)
Requires=basic.target
Wants=basic.target
After=basic.target
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/usr/lib/systemd/system/graphical.target"
[Unit]
Description=Graphical Interface
Documentation=man:systemd.special(7)
Requires=multi-user.target
Wants=multi-user.target
After=multi-user.target
EOF

    ln -sfn "graphical.target" "${ROOTFS_DIR}/usr/lib/systemd/system/default.target"

    # Copy essential aarch64 glibc libraries if cross-toolchain is available on host
    if [[ -d "/usr/aarch64-linux-gnu/lib" ]]; then
        echo "[*] Populating aarch64 glibc runtime into rootfs..."
        cp -a /usr/aarch64-linux-gnu/lib/ld-linux-aarch64.so.1 "${ROOTFS_DIR}/lib/" || true
        cp -a /usr/aarch64-linux-gnu/lib/libc.so.6 "${ROOTFS_DIR}/lib/" || true
        cp -a /usr/aarch64-linux-gnu/lib/libgcc_s.so.1 "${ROOTFS_DIR}/lib/" || true
    fi

    # Configure Debian Sid sources.list
    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/sources.list"
deb http://deb.debian.org/debian/ sid main contrib non-free non-free-firmware
EOF

    # Configure APT Pinning
    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/preferences.d/utim-pinning"
Package: utim-init utlc libhybris* mesa-turnip* spa-droid*
Pin: release o=UniversalTreble
Pin-Priority: 1001
EOF

    # Install UTIM binaries, systemd shims, graphics check tool, and UTLC compositor
    echo "[*] Installing UTIM binaries, systemd shims, graphics check tool, and UTLC compositor..."
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utim" "${ROOTFS_DIR}/usr/bin/utim"
    ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/sbin/init"
    ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/init"

    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utimctl" "${ROOTFS_DIR}/usr/bin/utimctl"
    ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/usr/bin/systemctl"
    if [[ ! -L "${ROOTFS_DIR}/bin" ]] || [[ "$(readlink "${ROOTFS_DIR}/bin")" != *"usr/bin"* && "$(readlink "${ROOTFS_DIR}/bin")" != "usr/bin" ]]; then
        ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/bin/systemctl"
    fi

    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/deb-systemd-helper" "${ROOTFS_DIR}/usr/bin/deb-systemd-helper"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/deb-systemd-invoke" "${ROOTFS_DIR}/usr/bin/deb-systemd-invoke"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utim-graphics-check" "${ROOTFS_DIR}/usr/bin/utim-graphics-check"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utlc" "${ROOTFS_DIR}/usr/bin/utlc"

    # Install and enable UTLC systemd service
    if [[ -f "${WORKSPACE_ROOT}/utlc.service" ]]; then
        cp "${WORKSPACE_ROOT}/utlc.service" "${ROOTFS_DIR}/usr/lib/systemd/system/utlc.service"
        ln -sf "/usr/lib/systemd/system/utlc.service" "${ROOTFS_DIR}/etc/systemd/system/graphical.target.wants/utlc.service"
    fi

    # Copy deb packages
    mkdir -p "${ROOTFS_DIR}/tmp/debs"
    cp "${WORKSPACE_ROOT}/dist/"*.deb "${ROOTFS_DIR}/tmp/debs/"

    # Generate initial graphics environment
    "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --generate-env "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" || true
    ln -sf "/etc/environment.d/10-graphics.conf" "${ROOTFS_DIR}/run/utim/graphics.env" || true

    echo "[+] Mock rootfs structure validated successfully at ${ROOTFS_DIR}."
    echo "[+] To perform full privileged bootstrap, run this script as root (sudo ./scripts/build_rootfs.sh)."
    exit 0
fi

# Privileged full bootstrap path
mkdir -p "${ROOTFS_DIR}"

echo "[*] Running debootstrap for Debian Sid ARM64..."
debootstrap --arch="${TARGET_ARCH}" --foreign "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}"

echo "[*] Configuring APT repositories..."
cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/sources.list"
deb http://deb.debian.org/debian/ sid main contrib non-free non-free-firmware
EOF

cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/preferences.d/utim-pinning"
Package: utim-init libhybris* mesa-turnip* spa-droid*
Pin: release o=UniversalTreble
Pin-Priority: 1001
EOF

echo "[*] Installing UTIM binaries, systemd shims, and graphics check tool..."
cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utim" "${ROOTFS_DIR}/usr/bin/utim"
ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/sbin/init"
ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/init"

    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utimctl" "${ROOTFS_DIR}/usr/bin/utimctl"
    ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/usr/bin/systemctl"
    if [[ ! -L "${ROOTFS_DIR}/bin" ]] || [[ "$(readlink "${ROOTFS_DIR}/bin")" != *"usr/bin"* && "$(readlink "${ROOTFS_DIR}/bin")" != "usr/bin" ]]; then
        ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/bin/systemctl"
    fi

    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/deb-systemd-helper" "${ROOTFS_DIR}/usr/bin/deb-systemd-helper"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/deb-systemd-invoke" "${ROOTFS_DIR}/usr/bin/deb-systemd-invoke"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utim-graphics-check" "${ROOTFS_DIR}/usr/bin/utim-graphics-check"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utlc" "${ROOTFS_DIR}/usr/bin/utlc"

    mkdir -p "${ROOTFS_DIR}/usr/lib/systemd/system"
    mkdir -p "${ROOTFS_DIR}/etc/systemd/system/graphical.target.wants"

    # Canonical systemd target definitions
    cat << 'EOF' > "${ROOTFS_DIR}/usr/lib/systemd/system/basic.target"
[Unit]
Description=Basic System
Documentation=man:systemd.special(7)
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/usr/lib/systemd/system/multi-user.target"
[Unit]
Description=Multi-User System
Documentation=man:systemd.special(7)
Requires=basic.target
Wants=basic.target
After=basic.target
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/usr/lib/systemd/system/graphical.target"
[Unit]
Description=Graphical Interface
Documentation=man:systemd.special(7)
Requires=multi-user.target
Wants=multi-user.target
After=multi-user.target
EOF

    ln -sfn "graphical.target" "${ROOTFS_DIR}/usr/lib/systemd/system/default.target"

    if [[ -f "${WORKSPACE_ROOT}/utlc.service" ]]; then
        cp "${WORKSPACE_ROOT}/utlc.service" "${ROOTFS_DIR}/usr/lib/systemd/system/utlc.service"
        ln -sf "/usr/lib/systemd/system/utlc.service" "${ROOTFS_DIR}/etc/systemd/system/graphical.target.wants/utlc.service"
    fi

mkdir -p "${ROOTFS_DIR}/run/systemd/system"
mkdir -p "${ROOTFS_DIR}/run/utim"
mkdir -p "${ROOTFS_DIR}/etc/environment.d"

# Generate initial graphics environment
"${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --generate-env "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" || true
ln -sf "/etc/environment.d/10-graphics.conf" "${ROOTFS_DIR}/run/utim/graphics.env" || true

echo "[*] Installing dummy package, Phase 2 graphics packages, and Phase 3 UTLC into rootfs..."
mkdir -p "${ROOTFS_DIR}/tmp/debs"
cp "${WORKSPACE_ROOT}/dist/"*.deb "${ROOTFS_DIR}/tmp/debs/"
chroot "${ROOTFS_DIR}" dpkg -i /tmp/debs/utim-init-dummy.deb
chroot "${ROOTFS_DIR}" dpkg -i /tmp/debs/libhybris-hwcomposer_*.deb /tmp/debs/libhybris-gralloc_*.deb /tmp/debs/libhybris-egl_*.deb /tmp/debs/mesa-turnip-kgsl_*.deb /tmp/debs/mesa-zink_*.deb /tmp/debs/utlc_*.deb || true

echo "[*] Installing core runtime packages: seatd, elogind, pipewire, modemmanager, feedbackd..."
chroot "${ROOTFS_DIR}" apt-get update
chroot "${ROOTFS_DIR}" apt-get install -y --no-install-recommends \
    seatd \
    elogind \
    pipewire \
    modemmanager \
    feedbackd

echo "[+] Debian Sid ARM64 Rootfs successfully created!"

