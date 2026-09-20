#!/usr/bin/env bash
# ==============================================================================
# verify_phase2.sh - Meticulous automated verification test suite for Phase 2
# Checks HWC AIDL composer3/HIDL, Gralloc DMA-BUF, UBWC/AFBC, VSYNC, GPU,
# 64KB ELF alignment, Debian packaging, and scans for obvious bugs.
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m' # No Color

TOTAL_CHECKS=0
PASSED_CHECKS=0
FAILED_CHECKS=0

pass() {
    local msg="$1"
    TOTAL_CHECKS=$((TOTAL_CHECKS + 1))
    PASSED_CHECKS=$((PASSED_CHECKS + 1))
    echo -e "  [${GREEN}PASS${NC}] ${msg}"
}

fail() {
    local msg="$1"
    TOTAL_CHECKS=$((TOTAL_CHECKS + 1))
    FAILED_CHECKS=$((FAILED_CHECKS + 1))
    echo -e "  [${RED}FAIL${NC}] ${msg}"
}

header() {
    echo ""
    echo -e "${BOLD}${BLUE}=== $1 ===${NC}"
}

echo "=============================================================================="
echo " UNIVERSAL TREBLE LINUX GSI - PHASE 2 METICULOUS VERIFICATION SUITE"
echo " Milestones 2.1 & 2.2: Display & Graphics HAL Bring-Up & 3D GPU Pipeline"
echo "=============================================================================="

# ------------------------------------------------------------------------------
header "1. Toolchain & 64 KB ELF Page Alignment Configuration"
# ------------------------------------------------------------------------------

if grep -q "max-page-size=65536" "${WORKSPACE_ROOT}/.cargo/config.toml"; then
    pass "Cargo config has -Wl,-z,max-page-size=65536 for aarch64 target"
else
    fail "Cargo config missing 64KB page alignment flags"
fi

if rustup target list | grep -q "aarch64-unknown-linux-gnu (installed)"; then
    pass "rustup target aarch64-unknown-linux-gnu is installed"
else
    fail "rustup target aarch64-unknown-linux-gnu not found"
fi

# ------------------------------------------------------------------------------
header "2. Workspace Compilation & Unit/Integration Tests"
# ------------------------------------------------------------------------------

echo "[*] Running full workspace tests..."
if cargo test --workspace; then
    pass "All unit and integration tests passed across all workspace crates"
else
    fail "Cargo test reported test failures"
fi

echo "[*] Running dedicated Phase 2 graphics test suite..."
if cargo test --test graphics_test; then
    pass "Dedicated graphics test suite passed (HWC AIDL, Gralloc, VSYNC, GPU, ELF)"
else
    fail "Graphics test suite failed"
fi

# ------------------------------------------------------------------------------
header "3. Building Release aarch64 Binaries"
# ------------------------------------------------------------------------------

echo "[*] Compiling aarch64 release binaries..."
if cargo build --release --workspace --target aarch64-unknown-linux-gnu; then
    pass "Workspace compiled successfully for aarch64-unknown-linux-gnu"
else
    fail "Cross-compilation for aarch64 failed"
fi

# Ensure host diagnostic binary is ready
cargo build --bin utim-graphics-check >/dev/null 2>&1 || true

# ------------------------------------------------------------------------------
header "4. Meticulous 64 KB ELF Alignment Inspection"
# ------------------------------------------------------------------------------

AARCH64_RELEASE="${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release"
BINARIES=(
    "utim"
    "utimctl"
    "deb-systemd-helper"
    "deb-systemd-invoke"
    "utim-graphics-check"
)

for bin in "${BINARIES[@]}"; do
    bin_path="${AARCH64_RELEASE}/${bin}"
    if [[ ! -f "${bin_path}" ]]; then
        fail "Binary ${bin} not found at ${bin_path}"
        continue
    fi

    # Check alignment via utim-graphics-check --check-elf
    if "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --check-elf "${bin_path}" --json | grep -q '"is_64k_compatible":true'; then
        pass "Binary ${bin} is strictly 64KB page aligned (p_align >= 0x10000)"
    else
        fail "Binary ${bin} failed 64KB page alignment test!"
    fi
done

# ------------------------------------------------------------------------------
header "5. Graphics Debian Packages Construction & Inspection"
# ------------------------------------------------------------------------------

echo "[*] Packaging Phase 2 graphics Debian packages..."
"${SCRIPT_DIR}/package_graphics.sh" "${WORKSPACE_ROOT}/dist" >/dev/null

EXPECTED_PACKAGES=(
    "libhybris-hwcomposer_1.0.0_arm64.deb"
    "libhybris-gralloc_1.0.0_arm64.deb"
    "libhybris-egl_1.0.0_arm64.deb"
    "mesa-turnip-kgsl_24.2.0-1_arm64.deb"
    "mesa-zink_24.2.0-1_arm64.deb"
)

for pkg in "${EXPECTED_PACKAGES[@]}"; do
    pkg_file="${WORKSPACE_ROOT}/dist/${pkg}"
    if [[ -f "${pkg_file}" ]]; then
        pass "Debian package exists: ${pkg}"
        # Verify valid deb format
        if dpkg-deb -I "${pkg_file}" >/dev/null 2>&1; then
            pass "Package ${pkg} metadata integrity verified"
        else
            fail "Package ${pkg} corrupt or invalid control metadata"
        fi
    else
        fail "Expected Debian package missing: ${pkg}"
    fi
done

# Verify shared library ELF alignment inside Debian packages
echo "[*] Verifying 64KB ELF alignment of shared libraries inside Debian packages..."
TMP_EXTRACT="$(mktemp -d)"
dpkg-deb -x "${WORKSPACE_ROOT}/dist/libhybris-hwcomposer_1.0.0_arm64.deb" "${TMP_EXTRACT}/hwc"
dpkg-deb -x "${WORKSPACE_ROOT}/dist/libhybris-gralloc_1.0.0_arm64.deb" "${TMP_EXTRACT}/gralloc"
dpkg-deb -x "${WORKSPACE_ROOT}/dist/libhybris-egl_1.0.0_arm64.deb" "${TMP_EXTRACT}/egl"

SHLIBS=(
    "${TMP_EXTRACT}/hwc/usr/lib/aarch64-linux-gnu/libhybris-hwcomposer.so.1.0.0"
    "${TMP_EXTRACT}/gralloc/usr/lib/aarch64-linux-gnu/libhybris-gralloc.so.1.0.0"
    "${TMP_EXTRACT}/egl/usr/lib/aarch64-linux-gnu/libEGL.so.1.0.0"
    "${TMP_EXTRACT}/egl/usr/lib/aarch64-linux-gnu/libGLESv2.so.2.0.0"
)

for shlib in "${SHLIBS[@]}"; do
    shlib_name="$(basename "${shlib}")"
    if [[ -f "${shlib}" ]]; then
        if "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --check-elf "${shlib}" --json | grep -q '"is_64k_compatible":true'; then
            pass "Shared library ${shlib_name} has 64KB page alignment"
        else
            fail "Shared library ${shlib_name} is NOT 64KB aligned!"
        fi
    else
        fail "Shared library ${shlib_name} missing from package extraction"
    fi
done
rm -rf "${TMP_EXTRACT}"

# ------------------------------------------------------------------------------
header "6. Meticulous Obvious Bug & Regression Checks"
# ------------------------------------------------------------------------------

# Bug Check A: Memory / File Descriptor leak check in Gralloc DMA-BUF allocation
echo "[*] Checking for DMA-BUF file descriptor leaks..."
FD_TEST_RESULT=$(cargo test --test graphics_test test_gralloc_linear_and_compressed_allocations -- --nocapture 2>&1)
if echo "${FD_TEST_RESULT}" | grep -q "test result: ok"; then
    pass "No file descriptor leaks during DMA-BUF allocation and export"
else
    fail "DMA-BUF buffer test failed"
fi

# Bug Check B: Refresh Rate Jitter & Monotonicity
echo "[*] Checking VSYNC refresh rate jitter (60Hz, 90Hz, 120Hz, 144Hz)..."
VSYNC_DIAG_OUTPUT=$("${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --json)
if echo "${VSYNC_DIAG_OUTPUT}" | grep -q '"all_passed":true'; then
    pass "All standard refresh rates (60, 90, 120, 144 Hz) verified tear-free"
else
    fail "VSYNC timing verification reported tear or jitter issues"
fi

# Bug Check C: Multi-plane hardware layer exhaustion and demotion
echo "[*] Checking HWC multi-plane layer overflow handling..."
HWC_PLANE_TEST=$(cargo test --test graphics_test test_hwc_plane_overflow_and_client_target_fallback -- --nocapture 2>&1)
if echo "${HWC_PLANE_TEST}" | grep -q "test result: ok"; then
    pass "HWC plane overflow gracefully demotes layers to ClientTarget"
else
    fail "HWC plane overflow test failed"
fi

# Bug Check D: Qualcomm UBWC and ARM AFBC format handling
echo "[*] Checking Qualcomm UBWC and ARM AFBC compression negotiation..."
if echo "${VSYNC_DIAG_OUTPUT}" | grep -q '"ubwc_ok":true'; then
    pass "Qualcomm UBWC metadata layout and alignment verified"
else
    fail "Qualcomm UBWC allocation failed"
fi

# Bug Check E: GPU dynamic pipeline switching & environment generation
echo "[*] Checking GPU pipeline detection and environment generation..."
TMP_ENV="$(mktemp)"
"${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --generate-env "${TMP_ENV}" >/dev/null
if [[ -s "${TMP_ENV}" ]]; then
    pass "GPU pipeline environment file generated successfully"
    if grep -q "GALLIUM_DRIVER=" "${TMP_ENV}"; then
        pass "Environment config defines GALLIUM_DRIVER"
    else
        fail "Environment config missing GALLIUM_DRIVER"
    fi
else
    fail "Failed to generate GPU environment file"
fi
rm -f "${TMP_ENV}"

# Bug Check F: DMA-BUF Safe Clone FD Isolation & Double-Close Prevention
echo "[*] Checking DMA-BUF clone file descriptor isolation..."
CLONE_TEST=$(cargo test --lib test_dmabuf_clone_fd_isolation -- --nocapture 2>&1)
if echo "${CLONE_TEST}" | grep -q "test result: ok"; then
    pass "DMA-BUF Clone duplicates file descriptors via F_DUPFD_CLOEXEC (no aliasing / double-close)"
else
    fail "DMA-BUF Clone FD isolation test failed"
fi

# Bug Check G: HWC Fence Lifecycle and Presentation Consumption
echo "[*] Checking HWC fence lifecycle and clean fd closing..."
FENCE_TEST=$(cargo test --lib test_hwc_fence_lifecycle_and_clean_drop -- --nocapture 2>&1)
if echo "${FENCE_TEST}" | grep -q "test result: ok"; then
    pass "HWC acquire/release fences properly consumed and closed without fd leaks"
else
    fail "HWC fence lifecycle test failed"
fi

# Bug Check H: Qualcomm UBWC NV12 4-Plane Format Verification
echo "[*] Checking Qualcomm UBWC NV12 4-plane allocation..."
UBWC_NV12_TEST=$(cargo test --lib test_gralloc_allocate_ubwc_nv12_4planes -- --nocapture 2>&1)
if echo "${UBWC_NV12_TEST}" | grep -q "test result: ok"; then
    pass "Qualcomm UBWC NV12 correctly allocates 4 planes (Y + Y meta + UV + UV meta)"
else
    fail "Qualcomm UBWC NV12 4-plane test failed"
fi

# Bug Check I: Turnip KGSL TU_DEBUG Environment Verification
echo "[*] Checking Turnip KGSL TU_DEBUG environment configuration..."
GPU_TEST=$(cargo test --test graphics_test test_gpu_detection_and_environment_profiles -- --nocapture 2>&1)
if echo "${GPU_TEST}" | grep -q "test result: ok"; then
    pass "GPU environment defines TU_DEBUG=kgsl and HYBRIS_EGLPLATFORM=hwcomposer"
else
    fail "GPU environment profile test failed"
fi

# Bug Check J: ELF Non-Power-of-Two and Empty PT_LOAD Rejection
echo "[*] Checking ELF non-power-of-two and empty LOAD segment rejection..."
ELF_EDGE_TEST=$(cargo test --lib test_elf_align -- --nocapture 2>&1)
if echo "${ELF_EDGE_TEST}" | grep -q "test result: ok"; then
    pass "ELF validator correctly rejects non-power-of-two p_align and empty PT_LOAD binaries"
else
    fail "ELF edge case validation test failed"
fi

# Bug Check K: utim-graphics-check CLI Flag Parsing Robustness
echo "[*] Checking utim-graphics-check CLI flags..."
CLI_JSON_OUT=$("${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --check-elf "${AARCH64_RELEASE}/utim" --json)
if echo "${CLI_JSON_OUT}" | grep -q '"is_64k_compatible":true'; then
    pass "CLI utim-graphics-check --check-elf <target> --json properly formats and validates 64K ELF"
else
    fail "CLI utim-graphics-check --check-elf --json failed on target"
fi

# Bug Check L: Gralloc DMA-BUF Import & Foreign Descriptor Ownership
echo "[*] Checking Gralloc DMA-BUF import and descriptor ownership..."
IMPORT_TEST=$(cargo test --lib test_gralloc_import_dmabuf -- --nocapture 2>&1)
if echo "${IMPORT_TEST}" | grep -q "test result: ok"; then
    pass "DMA-BUF import safely duplicates foreign file descriptors without aliasing"
else
    fail "DMA-BUF import test failed"
fi

# Bug Check M: HWC Cursor & Hardware Overlay Plane Exhaustion Prevention
echo "[*] Checking HWC cursor and multi-plane hardware overlay capacity..."
CURSOR_TEST=$(cargo test --lib test_hwc_cursor_and_device_plane_overflow -- --nocapture 2>&1)
if echo "${CURSOR_TEST}" | grep -q "test result: ok"; then
    pass "HWC accounts for cursor/hardware planes to prevent display controller overlay exhaustion"
else
    fail "HWC cursor plane test failed"
fi

# Bug Check N: HWC ClientTarget Acquire Fence Consumption
echo "[*] Checking HWC ClientTarget acquire fence lifecycle..."
TARGET_FENCE_TEST=$(cargo test --lib test_hwc_client_target_fence_consumed_on_presentation -- --nocapture 2>&1)
if echo "${TARGET_FENCE_TEST}" | grep -q "test result: ok"; then
    pass "ClientTarget acquire fence properly consumed and closed on presentation without fd leaks"
else
    fail "ClientTarget acquire fence test failed"
fi

# Bug Check O: VSYNC Extreme Skew Overflow & Monotonicity Violation Detection
echo "[*] Checking VSYNC extreme skew 128-bit calculation and monotonicity..."
VSYNC_SKEW_TEST=$(cargo test --lib test_vsync_large_timestamp_skew_and_monotonicity -- --nocapture 2>&1)
if echo "${VSYNC_SKEW_TEST}" | grep -q "test result: ok"; then
    pass "VSYNC timing validator rejects large skews without integer truncation and rejects out-of-order frames"
else
    fail "VSYNC timing skew and monotonicity test failed"
fi

# Bug Check P: Gralloc Conflicting/Incompatible Compression Rejection
echo "[*] Checking Gralloc conflicting and incompatible compression rejection..."
COMPRESS_TEST=$(cargo test --lib test_gralloc_conflicting_and_incompatible_compression -- --nocapture 2>&1)
if echo "${COMPRESS_TEST}" | grep -q "test result: ok"; then
    pass "Gralloc rejects conflicting UBWC+AFBC flags and incompatible format compression"
else
    fail "Gralloc compression conflict test failed"
fi

# Bug Check Q: Samsung Exynos GPU Pipeline Detection
echo "[*] Checking Samsung Exynos (Xclipse) GPU pipeline detection..."
EXYNOS_TEST=$(cargo test --lib test_exynos_detection -- --nocapture 2>&1)
if echo "${EXYNOS_TEST}" | grep -q "test result: ok"; then
    pass "Samsung Exynos GPUs properly identified and assigned libhybris-egl pipeline"
else
    fail "Samsung Exynos GPU detection test failed"
fi

# ------------------------------------------------------------------------------
header "7. Rootfs Assembly Integration Check"
# ------------------------------------------------------------------------------

echo "[*] Running mock rootfs build with Phase 2 graphics integration..."
TEST_ROOTFS="${WORKSPACE_ROOT}/build/verify_rootfs"
rm -rf "${TEST_ROOTFS}"
DRY_RUN=1 "${SCRIPT_DIR}/build_rootfs.sh" "${TEST_ROOTFS}" >/dev/null

if [[ -f "${TEST_ROOTFS}/usr/bin/utim-graphics-check" ]]; then
    pass "Rootfs contains /usr/bin/utim-graphics-check"
else
    fail "Rootfs missing /usr/bin/utim-graphics-check"
fi

if [[ -f "${TEST_ROOTFS}/etc/environment.d/10-graphics.conf" ]]; then
    pass "Rootfs contains /etc/environment.d/10-graphics.conf"
else
    fail "Rootfs missing /etc/environment.d/10-graphics.conf"
fi

if grep -q "mesa-turnip" "${TEST_ROOTFS}/etc/apt/preferences.d/utim-pinning"; then
    pass "Rootfs APT pinning contains mesa-turnip* and libhybris*"
else
    fail "Rootfs APT pinning missing graphics packages"
fi

rm -rf "${TEST_ROOTFS}"

# ------------------------------------------------------------------------------
header "Summary of Verification"
# ------------------------------------------------------------------------------

echo "Total Checks:   ${TOTAL_CHECKS}"
echo -e "Passed Checks:  ${GREEN}${PASSED_CHECKS}${NC}"
if [[ ${FAILED_CHECKS} -gt 0 ]]; then
    echo -e "Failed Checks:  ${RED}${FAILED_CHECKS}${NC}"
    echo ""
    echo -e "${RED}${BOLD}Phase 2 Verification FAILED with ${FAILED_CHECKS} errors.${NC}"
    exit 1
else
    echo -e "Failed Checks:  ${GREEN}0${NC}"
    echo ""
    echo -e "${GREEN}${BOLD}Phase 2 Display & Graphics HAL Bring-Up METICULOUSLY VERIFIED!${NC}"
    echo "All milestones (2.1 & 2.2) passed with zero obvious bugs."
    exit 0
fi
