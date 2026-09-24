#!/usr/bin/env bash
# ==============================================================================
# run_qemu.sh - Boot Universal Treble Linux in QEMU (Android GKI / Ranchu)
#
# Supports all-in-one packaging (--build / --rebuild / --full-debian),
# interactive testing, graphical desktop, networking, and automated test mode (--test).
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

KERNEL_PATH="/opt/android-sdk/system-images/android-34/google_apis/arm64-v8a/kernel-ranchu"
INITRD_PATH="${WORKSPACE_ROOT}/dist/initramfs.cpio.gz"
DRIVE_PATH="${WORKSPACE_ROOT}/dist/system.raw.img"
ROOTFS_DIR="${WORKSPACE_ROOT}/build/rootfs"

TIMEOUT_SECS=15
TEST_MODE=0
GRAPHIC_MODE=0
DO_BUILD=0
DO_REBUILD=0
FULL_DEBIAN=0
ENABLE_NET=1

usage() {
    cat << EOF
Usage: $0 [options]

All-in-One Packaging & Testing Options:
  -b, --build       Pack and build all components (Rust binaries, rootfs, disk image, initramfs) before boot
  --rebuild         Clean old artifacts and rebuild everything fresh from scratch
  --full-debian     Bootstrap full Debian Sid ARM64 rootfs (requires sudo debootstrap for real apt/dpkg)
  --no-net          Disable virtual network interface (virtio-net-pci)

QEMU & Runtime Options:
  --kernel <path>   Kernel binary (default: ${KERNEL_PATH})
  --initrd <path>   Initramfs archive (default: ${INITRD_PATH})
  --drive <path>    Raw rootfs disk image (default: ${DRIVE_PATH})
  --test            Run in automated test mode (verifies UTIM boot and exits)
  --timeout <sec>   Timeout in seconds for test mode (default: ${TIMEOUT_SECS})
  --gui             Enable graphical display with virtio-gpu (default: -nographic)
  -h, --help        Show this help message
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -b|--build)
            DO_BUILD=1
            shift
            ;;
        --rebuild)
            DO_REBUILD=1
            shift
            ;;
        --full-debian)
            FULL_DEBIAN=1
            DO_BUILD=1
            shift
            ;;
        --no-net)
            ENABLE_NET=0
            shift
            ;;
        --net)
            ENABLE_NET=1
            shift
            ;;
        --kernel)
            KERNEL_PATH="$2"
            shift 2
            ;;
        --initrd)
            INITRD_PATH="$2"
            shift 2
            ;;
        --drive)
            DRIVE_PATH="$2"
            shift 2
            ;;
        --timeout)
            TIMEOUT_SECS="$2"
            shift 2
            ;;
        --test)
            TEST_MODE=1
            shift
            ;;
        --gui)
            GRAPHIC_MODE=1
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Unknown option: $1"
            usage
            ;;
    esac
done

if [[ ! -f "${KERNEL_PATH}" ]]; then
    echo "Error: Kernel not found at ${KERNEL_PATH}"
    exit 1
fi

pack_components() {
    local is_full="${1:-0}"
    echo "============================================================"
    echo " [PACK] Packaging Universal Treble Linux for QEMU"
    echo "============================================================"

    echo "[*] Step 1/3: Assembling Rootfs and packaging deb/binaries..."
    if [[ "${is_full}" == "1" ]]; then
        echo "[*] Executing privileged debootstrap build (sudo)..."
        sudo "${SCRIPT_DIR}/build_rootfs.sh" "${ROOTFS_DIR}"
    else
        "${SCRIPT_DIR}/build_rootfs.sh" "${ROOTFS_DIR}"
    fi

    echo "[*] Step 2/3: Packaging ext4 rootfs disk image (system.raw.img)..."
    "${SCRIPT_DIR}/build_image.sh" "${ROOTFS_DIR}" "${WORKSPACE_ROOT}/dist/system.img"

    echo "[*] Step 3/3: Packaging early boot initramfs (initramfs.cpio.gz)..."
    "${SCRIPT_DIR}/build_initramfs.sh" "${INITRD_PATH}"

    echo "[+] All components packaged successfully!"
    echo "============================================================"
}

# 1. Handle explicit packaging or auto-packing of missing dependencies
if [[ "${DO_REBUILD}" == "1" ]]; then
    echo "[*] Rebuild requested: removing stale build and disk artifacts..."
    rm -rf "${ROOTFS_DIR}" "${INITRD_PATH}" "${DRIVE_PATH}" "${WORKSPACE_ROOT}/dist/system.img"
    pack_components "${FULL_DEBIAN}"
elif [[ "${DO_BUILD}" == "1" ]]; then
    pack_components "${FULL_DEBIAN}"
else
    NEED_PACK=0
    if [[ ! -d "${ROOTFS_DIR}" || ! -f "${ROOTFS_DIR}/usr/bin/utim" ]]; then
        echo "[*] Rootfs not assembled at ${ROOTFS_DIR}."
        NEED_PACK=1
    fi

    if [[ ! -f "${DRIVE_PATH}" ]]; then
        echo "[*] Disk image not found at ${DRIVE_PATH}."
        NEED_PACK=1
    fi

    if [[ "${NEED_PACK}" == "1" ]]; then
        echo "[*] Auto-packing rootfs and disk image for testing..."
        pack_components 0
    fi

    if [[ ! -f "${INITRD_PATH}" ]]; then
        echo "[*] Initramfs not found at ${INITRD_PATH}. Building..."
        "${SCRIPT_DIR}/build_initramfs.sh" "${INITRD_PATH}"
    fi
fi

# 2. Display configuration
DISPLAY_OPTS=("-nographic")
if [[ "${GRAPHIC_MODE}" == "1" ]]; then
    DISPLAY_OPTS=(
        -device "virtio-gpu-pci,xres=1080,yres=2400"
        -device "virtio-keyboard-pci"
        -device "virtio-tablet-pci"
        -device "virtio-mouse-pci"
        -display "gtk,gl=off,zoom-to-fit=on"
    )
fi

# 3. Network configuration
NET_OPTS=()
if [[ "${ENABLE_NET}" == "1" ]]; then
    NET_OPTS=(
        -netdev "user,id=net0,hostfwd=tcp::2222-:22"
        -device "virtio-net-pci,netdev=net0"
    )
fi

# 4. Assemble QEMU execution command
QEMU_CMD=(
    qemu-system-aarch64
    -M virt,gic-version=3
    -cpu cortex-a76
    -smp 4
    -m 4096
    -kernel "${KERNEL_PATH}"
    -initrd "${INITRD_PATH}"
    -drive "file=${DRIVE_PATH},if=virtio,format=raw"
    -append "console=ttyAMA0 root=/dev/vda rw init=/init loglevel=7 printk.devkmsg=on"
    "${DISPLAY_OPTS[@]}"
    "${NET_OPTS[@]}"
)

# 5. Execute QEMU
if [[ "${TEST_MODE}" == "1" ]]; then
    echo "============================================================"
    echo " Testing Universal Treble Linux QEMU Boot"
    echo " Timeout: ${TIMEOUT_SECS}s"
    echo "============================================================"

    LOG_FILE=$(mktemp /tmp/utim_qemu_test.XXXXXX.log)
    "${QEMU_CMD[@]}" -serial "file:${LOG_FILE}" &
    QEMU_PID=$!

    BOOT_SUCCESS=0
    ELAPSED=0
    while [[ ${ELAPSED} -lt ${TIMEOUT_SECS} ]]; do
        if grep -q "Entering permanent epoll event loop" "${LOG_FILE}" 2>/dev/null; then
            BOOT_SUCCESS=1
            break
        fi
        if grep -q "Kernel panic" "${LOG_FILE}" 2>/dev/null; then
            echo "[!] Detected kernel panic in boot log!"
            break
        fi
        sleep 1
        ELAPSED=$((ELAPSED + 1))
    done

    kill -9 "${QEMU_PID}" 2>/dev/null || true
    wait "${QEMU_PID}" 2>/dev/null || true

    echo "--- QEMU Boot Log Snippet ---"
    grep -E "UTIM|Init Manager|Kernel panic|EXT4-fs|vda|UTLC" "${LOG_FILE}" || tail -n 25 "${LOG_FILE}"
    echo "-----------------------------"

    if [[ "${BOOT_SUCCESS}" == "1" ]]; then
        echo "[+] TEST PASSED: UTIM PID 1 booted and entered event loop successfully without panics!"
        rm -f "${LOG_FILE}"
        exit 0
    else
        echo "[-] TEST FAILED: UTIM did not enter event loop within ${TIMEOUT_SECS}s or panicked."
        echo "Full log at: ${LOG_FILE}"
        exit 1
    fi
else
    exec "${QEMU_CMD[@]}" -serial mon:stdio
fi
