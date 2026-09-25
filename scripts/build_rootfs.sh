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
    echo "[*] Assembling Debian Sid ARM64 rootfs (with APT, dpkg, bash, and UTIM init)..."

    # Ensure Debian Sid ARM64 packages (apt, dpkg, bash, coreutils) are populated
    if [[ ! -f "${ROOTFS_DIR}/usr/bin/apt" ]]; then
        if [[ -d "${WORKSPACE_ROOT}/build/test_debootstrap" && -f "${WORKSPACE_ROOT}/build/test_debootstrap/usr/bin/apt" ]]; then
            echo "[*] Populating Debian Sid base from cached debootstrap..."
            rm -rf "${ROOTFS_DIR}"
            mkdir -p "${ROOTFS_DIR}"
            cp -a "${WORKSPACE_ROOT}/build/test_debootstrap/." "${ROOTFS_DIR}/"
        else
            echo "[*] Bootstrapping Debian Sid ARM64 base packages via fakeroot..."
            rm -rf "${ROOTFS_DIR}"
            mkdir -p "${ROOTFS_DIR}"
            fakeroot debootstrap --foreign --variant=minbase --arch="${TARGET_ARCH}" "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}" || true
        fi
    fi

    # Set up basic users and groups if not present
    if [[ ! -f "${ROOTFS_DIR}/etc/passwd" ]]; then
        if [[ -f "${ROOTFS_DIR}/usr/share/base-passwd/passwd.master" ]]; then
            cp "${ROOTFS_DIR}/usr/share/base-passwd/passwd.master" "${ROOTFS_DIR}/etc/passwd"
            cp "${ROOTFS_DIR}/usr/share/base-passwd/group.master" "${ROOTFS_DIR}/etc/group"
        else
            cat << 'EOF' > "${ROOTFS_DIR}/etc/passwd"
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
bin:x:2:2:bin:/bin:/usr/sbin/nologin
EOF
            cat << 'EOF' > "${ROOTFS_DIR}/etc/group"
root:x:0:
daemon:x:1:
bin:x:2:
EOF
        fi
        cat << 'EOF' > "${ROOTFS_DIR}/etc/shadow"
root:*:19700:0:99999:7:::
EOF
        chmod 600 "${ROOTFS_DIR}/etc/shadow" 2>/dev/null || true
    fi

    echo "treble-gsi" > "${ROOTFS_DIR}/etc/hostname"
    cat << 'EOF' > "${ROOTFS_DIR}/etc/hosts"
127.0.0.1 localhost
127.0.1.1 treble-gsi
::1 localhost ip6-localhost ip6-loopback
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/etc/resolv.conf"
# Configured for QEMU & Universal Treble Linux
nameserver 10.0.2.3
nameserver 8.8.8.8
nameserver 1.1.1.1
EOF

    mkdir -p "${ROOTFS_DIR}/etc/apt/preferences.d"
    mkdir -p "${ROOTFS_DIR}/etc/environment.d"
    mkdir -p "${ROOTFS_DIR}/etc/systemd/system/multi-user.target.wants"
    mkdir -p "${ROOTFS_DIR}/etc/systemd/system/graphical.target.wants"
    mkdir -p "${ROOTFS_DIR}/usr/lib/systemd/system"
    mkdir -p "${ROOTFS_DIR}/run/systemd/system"
    mkdir -p "${ROOTFS_DIR}/run/utim"
    mkdir -p "${ROOTFS_DIR}/var/lib/systemd/deb-systemd-helper-enabled"
    mkdir -p "${ROOTFS_DIR}/proc" "${ROOTFS_DIR}/sys" "${ROOTFS_DIR}/dev"
    mkdir -p "${ROOTFS_DIR}/root" "${ROOTFS_DIR}/home" "${ROOTFS_DIR}/mnt"
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

    # Configure APT Pinning (Block systemd completely and prioritize Treble packages)
    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/preferences.d/00-no-systemd"
Package: systemd systemd-sysv systemd-boot systemd-timesyncd systemd-resolved
Pin: release *
Pin-Priority: -1
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/preferences.d/utim-pinning"
Package: utim-init utim-init-dummy utlc libhybris* mesa-turnip* spa-droid*
Pin: release o=UniversalTreble
Pin-Priority: 1001
EOF

    # Configure Universal Treble Linux APT defaults
    mkdir -p "${ROOTFS_DIR}/etc/apt/apt.conf.d"
    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/apt.conf.d/90universal-android"
APT::Get::Assume-Yes "true";
APT::Get::AutomaticRemove "true";
APT::Install-Recommends "false";
APT::Install-Suggests "false";
Dpkg::Options {
   "--force-confdef";
   "--force-confold";
};
EOF

    # Ensure standard merged-usr symlinks (bin -> usr/bin, sbin -> usr/sbin, lib -> usr/lib)
    for d in bin sbin lib; do
        if [[ -d "${ROOTFS_DIR}/${d}" && ! -L "${ROOTFS_DIR}/${d}" ]]; then
            cp -a "${ROOTFS_DIR}/${d}/." "${ROOTFS_DIR}/usr/${d}/" 2>/dev/null || true
            rm -rf "${ROOTFS_DIR}/${d}"
            ln -sfn "usr/${d}" "${ROOTFS_DIR}/${d}"
        elif [[ ! -e "${ROOTFS_DIR}/${d}" ]]; then
            ln -sfn "usr/${d}" "${ROOTFS_DIR}/${d}"
        fi
    done

    # Install UTIM binaries, systemd shims, graphics check tool, and UTLC compositor
    echo "[*] Installing UTIM binaries, systemd shims, graphics check tool, and UTLC compositor..."
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utim" "${ROOTFS_DIR}/usr/bin/utim"
    ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/sbin/init"
    ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/init"

    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utimctl" "${ROOTFS_DIR}/usr/bin/utimctl"
    ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/usr/bin/systemctl"

    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/deb-systemd-helper" "${ROOTFS_DIR}/usr/bin/deb-systemd-helper"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/deb-systemd-invoke" "${ROOTFS_DIR}/usr/bin/deb-systemd-invoke"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utim-graphics-check" "${ROOTFS_DIR}/usr/bin/utim-graphics-check"
    cp "${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release/utlc" "${ROOTFS_DIR}/usr/bin/utlc"

    # Install systemd helper shims (tmpfiles, sysusers, notify, escape)
    if [[ -d "${SCRIPT_DIR}/shims" ]]; then
        cp "${SCRIPT_DIR}/shims/systemd-tmpfiles" "${ROOTFS_DIR}/usr/bin/systemd-tmpfiles"
        cp "${SCRIPT_DIR}/shims/systemd-sysusers" "${ROOTFS_DIR}/usr/bin/systemd-sysusers"
        cp "${SCRIPT_DIR}/shims/systemd-notify" "${ROOTFS_DIR}/usr/bin/systemd-notify"
        cp "${SCRIPT_DIR}/shims/systemd-escape" "${ROOTFS_DIR}/usr/bin/systemd-escape"
        chmod 755 "${ROOTFS_DIR}/usr/bin/systemd-"*
        if [[ -d "${ROOTFS_DIR}/bin" && ! -L "${ROOTFS_DIR}/bin" ]]; then
            ln -sf "/usr/bin/systemd-tmpfiles" "${ROOTFS_DIR}/bin/systemd-tmpfiles"
            ln -sf "/usr/bin/systemd-sysusers" "${ROOTFS_DIR}/bin/systemd-sysusers"
            ln -sf "/usr/bin/systemd-notify" "${ROOTFS_DIR}/bin/systemd-notify"
            ln -sf "/usr/bin/systemd-escape" "${ROOTFS_DIR}/bin/systemd-escape"
        fi
    fi

    # Install and enable UTLC systemd service
    if [[ -f "${WORKSPACE_ROOT}/utlc.service" ]]; then
        cp "${WORKSPACE_ROOT}/utlc.service" "${ROOTFS_DIR}/usr/lib/systemd/system/utlc.service"
        ln -sf "/usr/lib/systemd/system/utlc.service" "${ROOTFS_DIR}/etc/systemd/system/graphical.target.wants/utlc.service"
    fi

    # Copy deb packages and kernel modules
    mkdir -p "${ROOTFS_DIR}/tmp/debs"
    cp "${WORKSPACE_ROOT}/dist/"*.deb "${ROOTFS_DIR}/tmp/debs/"
    mkdir -p "${ROOTFS_DIR}/lib/modules"
    cp -a "${WORKSPACE_ROOT}/dist/modules/"*.ko "${ROOTFS_DIR}/lib/modules/" 2>/dev/null || true

    # Populate dpkg status database with all base debs and UTIM packages
    if [[ -f "${SCRIPT_DIR}/populate_dpkg_status.py" ]]; then
        python3 "${SCRIPT_DIR}/populate_dpkg_status.py" "${ROOTFS_DIR}" || true
    fi

    # Generate initial graphics environment
    mkdir -p "${ROOTFS_DIR}/etc/environment.d" "${ROOTFS_DIR}/run/utim"
    if [[ -x "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" ]]; then
        "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --generate-env "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" || true
    elif [[ -x "${WORKSPACE_ROOT}/target/release/utim-graphics-check" ]]; then
        "${WORKSPACE_ROOT}/target/release/utim-graphics-check" --generate-env "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" || true
    else
        cargo run --bin utim-graphics-check -- --generate-env "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" || true
    fi
    ln -sf "/etc/environment.d/10-graphics.conf" "${ROOTFS_DIR}/run/utim/graphics.env" || true

    # Generate ld.so.cache for dynamic library resolution
    if [[ -f "${ROOTFS_DIR}/sbin/ldconfig" ]] && command -v qemu-aarch64-static >/dev/null 2>&1; then
        echo "[*] Generating ld.so.cache using qemu-aarch64-static ldconfig..."
        qemu-aarch64-static -L "${ROOTFS_DIR}" "${ROOTFS_DIR}/sbin/ldconfig" -C "${ROOTFS_DIR}/etc/ld.so.cache" -f "${ROOTFS_DIR}/etc/ld.so.conf" 2>/dev/null || true
    fi

    echo "[+] Debian Sid ARM64 rootfs assembled successfully at ${ROOTFS_DIR}."
    exit 0
fi

# Privileged full bootstrap path
mkdir -p "${ROOTFS_DIR}"

echo "[*] Running debootstrap for Debian Sid ARM64..."
debootstrap --arch="${TARGET_ARCH}" --foreign "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}"

if [[ -f "${ROOTFS_DIR}/debootstrap/debootstrap" ]]; then
    echo "[*] Completing foreign debootstrap second-stage via qemu-aarch64-static..."
    cp "$(command -v qemu-aarch64-static 2>/dev/null || echo /usr/bin/qemu-aarch64-static)" "${ROOTFS_DIR}/usr/bin/" 2>/dev/null || true
    mount -t proc proc "${ROOTFS_DIR}/proc" || true
    mount -t sysfs sysfs "${ROOTFS_DIR}/sys" || true
    mount --bind /dev "${ROOTFS_DIR}/dev" || true
    chroot "${ROOTFS_DIR}" /debootstrap/debootstrap --second-stage || true
    umount -l "${ROOTFS_DIR}/dev" 2>/dev/null || true
    umount -l "${ROOTFS_DIR}/sys" 2>/dev/null || true
    umount -l "${ROOTFS_DIR}/proc" 2>/dev/null || true
fi

echo "[*] Configuring APT repositories..."
cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/sources.list"
deb http://deb.debian.org/debian/ sid main contrib non-free non-free-firmware
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/preferences.d/00-no-systemd"
Package: systemd systemd-sysv systemd-boot systemd-timesyncd systemd-resolved
Pin: release *
Pin-Priority: -1
EOF

    cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/preferences.d/utim-pinning"
Package: utim-init utim-init-dummy utlc libhybris* mesa-turnip* spa-droid*
Pin: release o=UniversalTreble
Pin-Priority: 1001
EOF

mkdir -p "${ROOTFS_DIR}/etc/apt/apt.conf.d"
cat << 'EOF' > "${ROOTFS_DIR}/etc/apt/apt.conf.d/90universal-android"
APT::Get::Assume-Yes "true";
APT::Get::AutomaticRemove "true";
APT::Install-Recommends "false";
APT::Install-Suggests "false";
Dpkg::Options {
   "--force-confdef";
   "--force-confold";
};
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

    # Install systemd helper shims (tmpfiles, sysusers, notify, escape)
    if [[ -d "${SCRIPT_DIR}/shims" ]]; then
        cp "${SCRIPT_DIR}/shims/systemd-tmpfiles" "${ROOTFS_DIR}/usr/bin/systemd-tmpfiles"
        cp "${SCRIPT_DIR}/shims/systemd-sysusers" "${ROOTFS_DIR}/usr/bin/systemd-sysusers"
        cp "${SCRIPT_DIR}/shims/systemd-notify" "${ROOTFS_DIR}/usr/bin/systemd-notify"
        cp "${SCRIPT_DIR}/shims/systemd-escape" "${ROOTFS_DIR}/usr/bin/systemd-escape"
        chmod 755 "${ROOTFS_DIR}/usr/bin/systemd-"*
        if [[ -d "${ROOTFS_DIR}/bin" && ! -L "${ROOTFS_DIR}/bin" ]]; then
            ln -sf "/usr/bin/systemd-tmpfiles" "${ROOTFS_DIR}/bin/systemd-tmpfiles"
            ln -sf "/usr/bin/systemd-sysusers" "${ROOTFS_DIR}/bin/systemd-sysusers"
            ln -sf "/usr/bin/systemd-notify" "${ROOTFS_DIR}/bin/systemd-notify"
            ln -sf "/usr/bin/systemd-escape" "${ROOTFS_DIR}/bin/systemd-escape"
        fi
    fi

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

cleanup_chroot_mounts() {
    umount -l "${ROOTFS_DIR}/dev" 2>/dev/null || true
    umount -l "${ROOTFS_DIR}/sys" 2>/dev/null || true
    umount -l "${ROOTFS_DIR}/proc" 2>/dev/null || true
}
trap cleanup_chroot_mounts EXIT
mount -t proc proc "${ROOTFS_DIR}/proc" || true
mount -t sysfs sysfs "${ROOTFS_DIR}/sys" || true
mount --bind /dev "${ROOTFS_DIR}/dev" || true

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

cleanup_chroot_mounts
trap - EXIT

echo "[+] Debian Sid ARM64 Rootfs successfully created!"

