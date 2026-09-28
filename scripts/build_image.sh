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

REL_ROOTFS="$(realpath --relative-to="${WORKSPACE_ROOT}" "${ROOTFS_DIR}")"
REL_RAW_IMG="$(realpath --relative-to="${WORKSPACE_ROOT}" "${RAW_IMG}")"
REL_OUTPUT_IMG="$(realpath --relative-to="${WORKSPACE_ROOT}" "${OUTPUT_IMG}")"

echo "[*] Creating raw ext4 image backing file..."
truncate -s "${IMAGE_SIZE_BYTES}" "${REL_RAW_IMG}"

echo "[*] Formatting ext4 filesystem with 4096-byte blocks..."
mke2fs -F -q -t ext4 -O ^metadata_csum_seed,^orphan_file -b 4096 -m 0 \
    -e remount-ro -E root_owner=0:0 "${REL_RAW_IMG}"

command -v e2fsdroid >/dev/null 2>&1 || { echo "FATAL: e2fsdroid required to populate ${REL_RAW_IMG}" >&2; exit 1; }
echo "[*] Populating ext4 filesystem using e2fsdroid from ${REL_ROOTFS}..."
e2fsdroid -e -a /system -f "${REL_ROOTFS}" "${REL_RAW_IMG}" || exit 1

# Replay the journal and clear needs_recovery so every boot does not pay a replay.
e2fsck -fp "${REL_RAW_IMG}" || exit 1

command -v img2simg >/dev/null 2>&1 || { echo "FATAL: img2simg required to produce sparse ${REL_OUTPUT_IMG}" >&2; exit 1; }
echo "[*] Converting raw ext4 image to Android sparse format (img2simg)..."
img2simg "${REL_RAW_IMG}" "${REL_OUTPUT_IMG}" || exit 1
echo "[*] Retained raw image at ${REL_RAW_IMG} for QEMU / direct boot."

echo "[+] Successfully created Universal Treble Linux GSI image: ${REL_OUTPUT_IMG}"
ls -lh "${REL_OUTPUT_IMG}" "${REL_RAW_IMG}"
