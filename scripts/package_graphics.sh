#!/usr/bin/env bash
# ==============================================================================
# package_graphics.sh - Master packaging orchestrator for Phase 2 Graphics HAL
# Compiles and packages libhybris, Mesa Turnip/Zink, and libhybris-egl
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUTPUT_DIR="${1:-${WORKSPACE_ROOT}/dist}"

mkdir -p "${OUTPUT_DIR}"

echo "============================================================"
echo " Packaging Universal Treble Linux Graphics HAL Subsystem"
echo " Target Directory: ${OUTPUT_DIR}"
echo "============================================================"

# 1. Build modern libhybris packages (AIDL composer3 & HIDL)
echo "[*] Step 1/3: Building libhybris packages..."
"${SCRIPT_DIR}/build_hybris.sh" "${OUTPUT_DIR}"

# 2. Build Mesa Turnip (KGSL) and Zink packages
echo "[*] Step 2/3: Building Mesa Turnip and Zink packages..."
"${SCRIPT_DIR}/build_mesa.sh" "${OUTPUT_DIR}"

# 3. Build libhybris-egl package
echo "[*] Step 3/3: Building libhybris-egl package..."
"${SCRIPT_DIR}/build_hybris_egl.sh" "${OUTPUT_DIR}"

echo "============================================================"
echo " All Phase 2 Display & Graphics HAL Packages Ready:"
echo "============================================================"
ls -lh "${OUTPUT_DIR}"/*.deb
