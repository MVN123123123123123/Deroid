#!/usr/bin/env bash
# ==============================================================================
# build_rootfs.sh - Construct Debian Sid ARM64 rootfs with UTIM Init
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

TARGET_ARCH="${TARGET_ARCH:-amd64}"
case "${TARGET_ARCH}" in
    amd64|x86_64)
        TARGET_ARCH="amd64"
        RUST_TARGET="x86_64-unknown-linux-gnu"
        ;;
    arm64|aarch64)
        TARGET_ARCH="arm64"
        RUST_TARGET="aarch64-unknown-linux-gnu"
        ;;
    *)
        echo "Unsupported TARGET_ARCH: ${TARGET_ARCH}" >&2
        exit 1
        ;;
esac
DEBIAN_MIRROR="http://deb.debian.org/debian/"
DEBIAN_SUITE="sid"
ROOTFS_DIR="${1:-${WORKSPACE_ROOT}/build/rootfs}"
DRY_RUN="${DRY_RUN:-0}"

shopt -s nullglob

# Shared identity/host setup used by BOTH build branches (B-8): base
# passwd/group/shadow, unprivileged user + canonical group membership,
# sudoers, hostname, hosts, resolv.conf, and the /etc/fstab mount contract.
configure_identity_and_network() {
    local R="$1"

    # Set up basic users and groups if not present
    if [[ ! -f "${R}/etc/passwd" ]]; then
        if [[ -f "${R}/usr/share/base-passwd/passwd.master" ]]; then
            cp "${R}/usr/share/base-passwd/passwd.master" "${R}/etc/passwd"
            cp "${R}/usr/share/base-passwd/group.master" "${R}/etc/group"
        else
            cat << 'EOF' > "${R}/etc/passwd"
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
bin:x:2:2:bin:/bin:/usr/sbin/nologin
EOF
            cat << 'EOF' > "${R}/etc/group"
root:x:0:
daemon:x:1:
bin:x:2:
EOF
        fi
        cat << 'EOF' > "${R}/etc/shadow"
root:*:19700:0:99999:7:::
EOF
        chmod 600 "${R}/etc/shadow"
    fi

    # Ensure unprivileged mobile userspace (UID 1000: user) exists
    if ! grep -q "^user:" "${R}/etc/passwd" 2>/dev/null; then
        echo "user:x:1000:1000:Universal Treble User:/home/user:/bin/bash" >> "${R}/etc/passwd"
    fi
    if ! grep -q "^user:" "${R}/etc/group" 2>/dev/null; then
        echo "user:x:1000:" >> "${R}/etc/group"
    fi
    if ! grep -q "^user:" "${R}/etc/shadow" 2>/dev/null; then
        echo "user:*:19700:0:99999:7:::" >> "${R}/etc/shadow"
    fi
    # Idempotent membership in canonical Debian groups (B-7): create with
    # the real GID when absent, then add "user" exactly once.
    for spec in audio:29 video:44 input:105 render:108 sudo:27 dialout:20 netdev:100 seat:102; do
        g="${spec%%:*}"; want="${spec##*:}"
        if ! grep -q "^${g}:" "${R}/etc/group" 2>/dev/null; then
            echo "${g}:x:${want}:" >> "${R}/etc/group"
        fi
        if grep -q "^${g}:[^:]*:[^:]*:$" "${R}/etc/group"; then
            sed -i "s/^\(${g}:[^:]*:[^:]*:\).*/\1user/" "${R}/etc/group"
        elif ! grep -qE "^${g}:[^:]*:[^:]*:(.*[,])?user([,].*)?$" "${R}/etc/group"; then
            sed -i "s/^\(${g}:[^:]*:[^:]*:.*\)$/\1,user/" "${R}/etc/group"
        fi
    done
    mkdir -p "${R}/home/user" "${R}/run/user/1000" "${R}/run/user/0"
    chmod 755 "${R}/home/user"
    chmod 700 "${R}/run/user/1000"
    # Least-privilege sudoers (B-22): utlc/utimctl/reboot only, never ALL.
    mkdir -p "${R}/etc/sudoers.d"
    rm -f "${R}/etc/sudoers.d/99-universal-treble"
    printf 'user ALL=(ALL:ALL) NOPASSWD: /usr/bin/utlc, /usr/bin/utimctl, /usr/sbin/reboot\n' \
        > "${R}/etc/sudoers.d/99-universal-treble"
    chmod 0440 "${R}/etc/sudoers.d/99-universal-treble"

    echo "treble-gsi" > "${R}/etc/hostname"
    cat << 'EOF' > "${R}/etc/hosts"
127.0.0.1 localhost
127.0.1.1 treble-gsi
::1 localhost ip6-localhost ip6-loopback
EOF

    cat << 'EOF' > "${R}/etc/resolv.conf"
# Configured for QEMU & Universal Treble Linux
nameserver 10.0.2.3
nameserver 8.8.8.8
nameserver 1.1.1.1
EOF

    # Documented mount contract (B-21). PID 1 mounts these itself; fstab
    # exists for tooling (e.g. fstrim --listed-in) and documentation.
    cat << 'EOF' > "${R}/etc/fstab"
/dev/vda   /        ext4  defaults,noatime              0 1
proc       /proc    proc  defaults,hidepid=2            0 0
sysfs      /sys     sysfs defaults,nosuid,nodev,noexec  0 0
devtmpfs   /dev     devtmpfs mode=0755,nosuid           0 0
tmpfs      /tmp     tmpfs mode=1777,nosuid,nodev        0 0
tmpfs      /run     tmpfs mode=0755,nosuid,nodev        0 0
EOF
}

echo "============================================================"
echo " Building Debian Sid ${TARGET_ARCH} Rootfs for Universal Treble Linux"
echo " Target Rootfs Directory: ${ROOTFS_DIR}"
echo "============================================================"

# Ensure UTIM binaries are built for target architecture
echo "[*] Verifying UTIM ${TARGET_ARCH} release binaries (${RUST_TARGET})..."
cargo build --release --workspace --target "${RUST_TARGET}"

# Ensure dummy deb package and graphics packages are built
echo "[*] Building utim-init-dummy package..."
"${SCRIPT_DIR}/create_dummy_deb.sh" "${WORKSPACE_ROOT}/dist"

echo "[*] Building Phase 2 Graphics HAL packages (libhybris, Mesa Turnip/Zink, libhybris-egl)..."
"${SCRIPT_DIR}/package_graphics.sh" "${WORKSPACE_ROOT}/dist"

echo "[*] Building Phase 3 UTLC Mobile Wayland Compositor & Shell package..."
TARGET_ARCH="${TARGET_ARCH}" "${SCRIPT_DIR}/package_utlc.sh" "${WORKSPACE_ROOT}/dist"

if [[ "${DRY_RUN}" == "1" || "$(id -u)" != "0" ]]; then
    echo "[!] Non-root execution detected or DRY_RUN=1."
    echo "[*] Assembling Debian Sid ${TARGET_ARCH} rootfs (with APT, dpkg, bash, and UTIM init)..."

    # Ensure Debian Sid packages (apt, dpkg, bash, coreutils) are populated
    if [[ -f "${ROOTFS_DIR}/usr/bin/apt" ]]; then
        current_elf="$(file -b "${ROOTFS_DIR}/usr/bin/apt" 2>/dev/null || true)"
        if [[ "${TARGET_ARCH}" == "amd64" && "${current_elf}" != *"x86-64"* ]]; then
            echo "[*] Stale non-amd64 rootfs detected at ${ROOTFS_DIR}. Removing..."
            rm -rf "${ROOTFS_DIR}"
        elif [[ "${TARGET_ARCH}" == "arm64" && "${current_elf}" != *"aarch64"* && "${current_elf}" != *"ARM aarch64"* ]]; then
            echo "[*] Stale non-arm64 rootfs detected at ${ROOTFS_DIR}. Removing..."
            rm -rf "${ROOTFS_DIR}"
        fi
    fi

    if [[ ! -f "${ROOTFS_DIR}/usr/bin/apt" ]]; then
        if [[ -d "${WORKSPACE_ROOT}/build/test_debootstrap_${TARGET_ARCH}" && -f "${WORKSPACE_ROOT}/build/test_debootstrap_${TARGET_ARCH}/usr/bin/apt" ]]; then
            echo "[*] Populating Debian Sid base from cached debootstrap (${TARGET_ARCH})..."
            rm -rf "${ROOTFS_DIR}"
            mkdir -p "${ROOTFS_DIR}"
            cp -a "${WORKSPACE_ROOT}/build/test_debootstrap_${TARGET_ARCH}/." "${ROOTFS_DIR}/"
        elif [[ "${TARGET_ARCH}" == "arm64" && -d "${WORKSPACE_ROOT}/build/test_debootstrap" && -f "${WORKSPACE_ROOT}/build/test_debootstrap/usr/bin/apt" ]]; then
            echo "[*] Populating Debian Sid base from cached debootstrap (arm64)..."
            rm -rf "${ROOTFS_DIR}"
            mkdir -p "${ROOTFS_DIR}"
            cp -a "${WORKSPACE_ROOT}/build/test_debootstrap/." "${ROOTFS_DIR}/"
        else
            echo "[*] Bootstrapping Debian Sid ${TARGET_ARCH} base packages via fakeroot..."
            rm -rf "${ROOTFS_DIR}"
            mkdir -p "${ROOTFS_DIR}"
            if [[ "${TARGET_ARCH}" == "amd64" && "$(uname -m)" == "x86_64" ]]; then
                fakeroot debootstrap --variant=minbase --arch="${TARGET_ARCH}" "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}" \
                    || { echo "FATAL: debootstrap ${DEBIAN_SUITE}/${TARGET_ARCH} failed" >&2; exit 1; }
            else
                fakeroot debootstrap --foreign --variant=minbase --arch="${TARGET_ARCH}" "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}" \
                    || { echo "FATAL: debootstrap ${DEBIAN_SUITE}/${TARGET_ARCH} failed" >&2; exit 1; }
            fi
        fi
        [[ -x "${ROOTFS_DIR}/usr/bin/apt" ]] || { echo "FATAL: base system incomplete (no ${ROOTFS_DIR}/usr/bin/apt)" >&2; exit 1; }
    fi

    # Identity, groups, hostname, resolver and fstab (shared with the
    # privileged branch via configure_identity_and_network).
    configure_identity_and_network "${ROOTFS_DIR}"

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
    if [[ "${TARGET_ARCH}" == "arm64" ]]; then
        rm -rf "${ROOTFS_DIR}/lib64"
        ln -sfn "lib" "${ROOTFS_DIR}/lib64"
    elif [[ "${TARGET_ARCH}" == "amd64" ]]; then
        mkdir -p "${ROOTFS_DIR}/usr/lib64"
        if [[ ! -e "${ROOTFS_DIR}/lib64" ]]; then
            ln -sfn "usr/lib64" "${ROOTFS_DIR}/lib64"
        fi
        if [[ -f "${ROOTFS_DIR}/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2" && ! -e "${ROOTFS_DIR}/usr/lib64/ld-linux-x86-64.so.2" ]]; then
            ln -sfn "../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2" "${ROOTFS_DIR}/usr/lib64/ld-linux-x86-64.so.2"
        fi
    fi

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

    # NOTE: no host glibc is ever copied into the image (B-5). Debootstrap
    # provides the matched loader/libc; the merged-usr fixup below relocates it.

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
Package: utim-init-dummy utlc libhybris* mesa-*
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
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utim" "${ROOTFS_DIR}/usr/bin/utim"
    ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/sbin/init"
    ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/init"

    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utimctl" "${ROOTFS_DIR}/usr/bin/utimctl"
    ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/usr/bin/systemctl"

    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/deb-systemd-helper" "${ROOTFS_DIR}/usr/bin/deb-systemd-helper"
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/deb-systemd-invoke" "${ROOTFS_DIR}/usr/bin/deb-systemd-invoke"
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utim-graphics-check" "${ROOTFS_DIR}/usr/bin/utim-graphics-check"
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utlc" "${ROOTFS_DIR}/usr/bin/utlc"

    # Install systemd helper shims (tmpfiles, sysusers, notify, escape)
    if [[ -d "${SCRIPT_DIR}/shims" ]]; then
        cp "${SCRIPT_DIR}/shims/systemd-tmpfiles" "${ROOTFS_DIR}/usr/bin/systemd-tmpfiles"
        cp "${SCRIPT_DIR}/shims/systemd-sysusers" "${ROOTFS_DIR}/usr/bin/systemd-sysusers"
        cp "${SCRIPT_DIR}/shims/systemd-notify" "${ROOTFS_DIR}/usr/bin/systemd-notify"
        cp "${SCRIPT_DIR}/shims/systemd-escape" "${ROOTFS_DIR}/usr/bin/systemd-escape"
        shim_bins=("${ROOTFS_DIR}/usr/bin/systemd-"*)
        if (( ${#shim_bins[@]} )); then
            chmod 755 "${shim_bins[@]}"
        fi
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

    # Install launcher icon assets (rendered hicolor theme only; never the
    # vendored third-party source SVGs).
    if [[ ! -d "${WORKSPACE_ROOT}/assets/icons" && -f "${SCRIPT_DIR}/download_icons.py" ]]; then
        python3 "${SCRIPT_DIR}/download_icons.py" || echo "[!] WARNING: icon download failed; continuing without launcher icons" >&2
    fi
    if [[ -d "${WORKSPACE_ROOT}/assets/icons/hicolor" ]]; then
        mkdir -p "${ROOTFS_DIR}/usr/share/icons"
        cp -a "${WORKSPACE_ROOT}/assets/icons/hicolor" "${ROOTFS_DIR}/usr/share/icons/"
        mkdir -p "${ROOTFS_DIR}/usr/share/pixmaps"
        cp -a "${WORKSPACE_ROOT}/assets/icons/hicolor/64x64/apps/"*.png "${ROOTFS_DIR}/usr/share/pixmaps/"
    fi

    # Copy deb packages and kernel modules
    mkdir -p "${ROOTFS_DIR}/tmp/debs"
    for deb in "${WORKSPACE_ROOT}/dist/"*.deb; do
        [[ -f "${deb}" ]] || continue
        case "${deb}" in
            *_"${TARGET_ARCH}".deb|*_all.deb|*utim-init-dummy.deb)
                cp "${deb}" "${ROOTFS_DIR}/tmp/debs/"
                ;;
            *)
                ;;
        esac
    done
    mkdir -p "${ROOTFS_DIR}/lib/modules"
    cp -a "${WORKSPACE_ROOT}/dist/modules/"*.ko "${ROOTFS_DIR}/lib/modules/" 2>/dev/null || true

    # Populate dpkg status database with all base debs and UTIM packages
    if [[ -f "${SCRIPT_DIR}/populate_dpkg_status.py" ]]; then
        python3 "${SCRIPT_DIR}/populate_dpkg_status.py" "${ROOTFS_DIR}"
    fi

    # Prune build-time weight that must not ship on the phone (B-11). This
    # runs AFTER populate_dpkg_status.py, which reads the archives cache.
    rm -rf "${ROOTFS_DIR}/var/lib/apt/lists"/* "${ROOTFS_DIR}/var/cache/apt/archives/"*.deb "${ROOTFS_DIR}/tmp/debs"
    mkdir -p "${ROOTFS_DIR}/var/lib/apt/lists/partial" "${ROOTFS_DIR}/var/cache/apt/archives/partial"
    rm -rf "${ROOTFS_DIR}/usr/share/doc" "${ROOTFS_DIR}/usr/share/man" "${ROOTFS_DIR}/usr/share/info"
    if [[ -d "${ROOTFS_DIR}/usr/lib/aarch64-linux-gnu/gconv" ]]; then
        find "${ROOTFS_DIR}/usr/lib/aarch64-linux-gnu/gconv" -name '*.so' \
            ! -name 'ISO8859-1.so' ! -name 'UTF-16.so' ! -name 'UTF-32.so' \
            ! -name 'ANSI_X3.4-1968.so' -delete
    fi
    if [[ -d "${ROOTFS_DIR}/usr/lib/x86_64-linux-gnu/gconv" ]]; then
        find "${ROOTFS_DIR}/usr/lib/x86_64-linux-gnu/gconv" -name '*.so' \
            ! -name 'ISO8859-1.so' ! -name 'UTF-16.so' ! -name 'UTF-32.so' \
            ! -name 'ANSI_X3.4-1968.so' -delete
    fi

    # Neutral graphics environment (B-6): the target GPU is probed at runtime
    # by utim. Never bake the build host's pipeline into the image.
    mkdir -p "${ROOTFS_DIR}/etc/environment.d" "${ROOTFS_DIR}/run/utim"
    cat > "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" <<'EOF'
# Universal Treble Linux - GPU pipeline is probed at runtime by utim.
# Do not pin a build-host pipeline here.
EOF
    ln -sf "/etc/environment.d/10-graphics.conf" "${ROOTFS_DIR}/run/utim/graphics.env"

    # Generate ld.so.cache for dynamic library resolution
    if [[ -f "${ROOTFS_DIR}/sbin/ldconfig" ]]; then
        if [[ "${TARGET_ARCH}" == "amd64" && "$(uname -m)" == "x86_64" ]]; then
            echo "[*] Generating ld.so.cache using native ldconfig..."
            "${ROOTFS_DIR}/sbin/ldconfig" -r "${ROOTFS_DIR}" -C "/etc/ld.so.cache" -f "/etc/ld.so.conf" 2>/dev/null || true
        elif command -v qemu-aarch64-static >/dev/null 2>&1; then
            echo "[*] Generating ld.so.cache using qemu-aarch64-static ldconfig..."
            qemu-aarch64-static -L "${ROOTFS_DIR}" "${ROOTFS_DIR}/sbin/ldconfig" -C "${ROOTFS_DIR}/etc/ld.so.cache" -f "${ROOTFS_DIR}/etc/ld.so.conf" 2>/dev/null || true
        fi
    fi

    echo "[+] Debian Sid ${TARGET_ARCH} rootfs assembled successfully at ${ROOTFS_DIR}."
    exit 0
fi

# Privileged full bootstrap path
mkdir -p "${ROOTFS_DIR}"

echo "[*] Running debootstrap for Debian Sid ${TARGET_ARCH}..."
if [[ "${TARGET_ARCH}" == "amd64" && "$(uname -m)" == "x86_64" ]]; then
    debootstrap --arch="${TARGET_ARCH}" "${DEBIAN_SUITE}" "${ROOTFS_DIR}" "${DEBIAN_MIRROR}"
else
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
Package: utim-init-dummy utlc libhybris* mesa-*
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

# Identity, groups, hostname, resolver and fstab (B-8): the privileged path
# must produce the same accounts and host files as the non-root path.
configure_identity_and_network "${ROOTFS_DIR}"

echo "[*] Installing UTIM binaries, systemd shims, and graphics check tool..."
cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utim" "${ROOTFS_DIR}/usr/bin/utim"
ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/sbin/init"
ln -sf "/usr/bin/utim" "${ROOTFS_DIR}/init"

    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utimctl" "${ROOTFS_DIR}/usr/bin/utimctl"
    ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/usr/bin/systemctl"
    if [[ ! -L "${ROOTFS_DIR}/bin" ]] || [[ "$(readlink "${ROOTFS_DIR}/bin")" != *"usr/bin"* && "$(readlink "${ROOTFS_DIR}/bin")" != "usr/bin" ]]; then
        ln -sf "/usr/bin/utimctl" "${ROOTFS_DIR}/bin/systemctl"
    fi

    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/deb-systemd-helper" "${ROOTFS_DIR}/usr/bin/deb-systemd-helper"
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/deb-systemd-invoke" "${ROOTFS_DIR}/usr/bin/deb-systemd-invoke"
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utim-graphics-check" "${ROOTFS_DIR}/usr/bin/utim-graphics-check"
    cp "${WORKSPACE_ROOT}/target/${RUST_TARGET}/release/utlc" "${ROOTFS_DIR}/usr/bin/utlc"

    # Install systemd helper shims (tmpfiles, sysusers, notify, escape)
    if [[ -d "${SCRIPT_DIR}/shims" ]]; then
        cp "${SCRIPT_DIR}/shims/systemd-tmpfiles" "${ROOTFS_DIR}/usr/bin/systemd-tmpfiles"
        cp "${SCRIPT_DIR}/shims/systemd-sysusers" "${ROOTFS_DIR}/usr/bin/systemd-sysusers"
        cp "${SCRIPT_DIR}/shims/systemd-notify" "${ROOTFS_DIR}/usr/bin/systemd-notify"
        cp "${SCRIPT_DIR}/shims/systemd-escape" "${ROOTFS_DIR}/usr/bin/systemd-escape"
        shim_bins=("${ROOTFS_DIR}/usr/bin/systemd-"*)
        if (( ${#shim_bins[@]} )); then
            chmod 755 "${shim_bins[@]}"
        fi
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

# Neutral graphics environment (B-6): the target GPU is probed at runtime
# by utim. Never bake the build host's pipeline into the image.
cat > "${ROOTFS_DIR}/etc/environment.d/10-graphics.conf" <<'EOF'
# Universal Treble Linux - GPU pipeline is probed at runtime by utim.
# Do not pin a build-host pipeline here.
EOF
ln -sf "/etc/environment.d/10-graphics.conf" "${ROOTFS_DIR}/run/utim/graphics.env"

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
echo "[*] Installing icon themes (PNG-based) so the launcher can resolve real app icons..."
chroot "${ROOTFS_DIR}" apt-get update
chroot "${ROOTFS_DIR}" apt-get install -y --no-install-recommends \
    seatd \
    elogind \
    pipewire \
    modemmanager \
    feedbackd \
    hicolor-icon-theme \
    adwaita-icon-theme-legacy \
    gnome-icon-theme \
    tango-icon-theme

cleanup_chroot_mounts
trap - EXIT

echo "[+] Debian Sid ${TARGET_ARCH} Rootfs successfully created!"

