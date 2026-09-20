#!/usr/bin/env bash
# ==============================================================================
# build_image.sh - Pack rootfs into sparse Android ext4 system.img
# Compatible with Android Fastbootd / Dynamic Partitions
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

ROOTFS_DIR="${1:-${WORKSPACE_ROOT}/build/rootfs}"
OUTPUT_IMG="${2:-${WORKSPACE_ROOT}/dist/system.img}"
RAW_IMG="${WORKSPACE_ROOT}/dist/system.raw.img"

# Default partition size: 4GB (4294967296 bytes)
IMAGE_SIZE_BYTES="${IMAGE_SIZE_BYTES:-4294967296}"

mkdir -p "${WORKSPACE_ROOT}/dist"

cd "${WORKSPACE_ROOT}"

REL_RAW_IMG="./dist/system.raw.img"
REL_OUTPUT_IMG="./dist/system.img"
REL_ROOTFS="./build/rootfs"

echo "[*] Creating raw ext4 image backing file..."
truncate -s "${IMAGE_SIZE_BYTES}" "${REL_RAW_IMG}"

echo "[*] Formatting ext4 filesystem with 4096-byte blocks..."
mke2fs -F -q -t ext4 -O ^metadata_csum_seed,^orphan_file -b 4096 "${REL_RAW_IMG}"

if command -v e2fsdroid >/dev/null 2>&1; then
    echo "[*] Populating ext4 filesystem using e2fsdroid..."
    e2fsdroid -e -a /system -f "${REL_ROOTFS}" "${REL_RAW_IMG}"
else
    echo "[!] Warning: e2fsdroid not found, skipping user-space population."
fi

if command -v img2simg >/dev/null 2>&1; then
    echo "[*] Converting raw ext4 image to Android sparse format (img2simg)..."
    img2simg "${REL_RAW_IMG}" "${REL_OUTPUT_IMG}"
    rm -f "${REL_RAW_IMG}"
else
    echo "[*] Renaming raw image to target..."
    mv "${REL_RAW_IMG}" "${REL_OUTPUT_IMG}"
fi

echo "[+] Successfully created Universal Treble Linux GSI image: ${REL_OUTPUT_IMG}"
ls -lh "${REL_OUTPUT_IMG}"
