#!/usr/bin/env bash
# ==============================================================================
# run_qemu.sh - Boot Universal Treble Linux in QEMU (Android GKI / Ranchu)
#
# Supports interactive running or automated test mode (--test).
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

KERNEL_PATH="/opt/android-sdk/system-images/android-34/google_apis/arm64-v8a/kernel-ranchu"
INITRD_PATH="${WORKSPACE_ROOT}/dist/initramfs.cpio.gz"
DRIVE_PATH="${WORKSPACE_ROOT}/dist/system.raw.img"
TIMEOUT_SECS=15
TEST_MODE=0
GRAPHIC_MODE=0

usage() {
    echo "Usage: $0 [options]"
    echo "Options:"
    echo "  --kernel <path>   Kernel binary (default: ${KERNEL_PATH})"
    echo "  --initrd <path>   Initramfs archive (default: ${INITRD_PATH})"
    echo "  --drive <path>    Raw rootfs disk image (default: ${DRIVE_PATH})"
    echo "  --test            Run in automated test mode (verifies UTIM boot and exits)"
    echo "  --timeout <sec>   Timeout in seconds (default: ${TIMEOUT_SECS})"
    echo "  --gui             Enable graphical display (default: -nographic)"
    echo "  -h, --help        Show this help message"
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
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

if [[ ! -f "${INITRD_PATH}" ]]; then
    echo "[*] Initramfs not found at ${INITRD_PATH}. Building..."
    "${SCRIPT_DIR}/build_initramfs.sh" "${INITRD_PATH}"
fi

if [[ ! -f "${DRIVE_PATH}" ]]; then
    echo "[*] Drive image not found at ${DRIVE_PATH}. Building..."
    "${SCRIPT_DIR}/build_image.sh"
fi

DISPLAY_OPTS=("-nographic")
if [[ "${GRAPHIC_MODE}" == "1" ]]; then
    DISPLAY_OPTS=(
        -device "virtio-gpu-pci,xres=1080,yres=2400"
        -device "virtio-keyboard-pci"
        -device "virtio-tablet-pci"
        -display "gtk,gl=off,zoom-to-fit=on"
    )
fi

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
)

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
    grep -E "UTIM|Init Manager|Kernel panic|EXT4-fs|vda" "${LOG_FILE}" || tail -n 20 "${LOG_FILE}"
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
