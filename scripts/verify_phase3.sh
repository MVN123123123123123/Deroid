#!/usr/bin/env bash
# ==============================================================================
# verify_phase3.sh - Comprehensive automated verification test suite for Phase 3
# Validates Android-Style Wayland Compositor & Launcher (UTLC - Rust / Smithay)
# Milestones 3.1 to 3.6:
# - 3.1: Smithay-compatible Mobile Protocols, HWC Backend, RSS < 15MB, Boot < 0.45s
# - 3.2: Paged Home Grid, Spring Physics, Hotseat Dock, Desktop Parser, Search < 1ms
# - 3.3: QuickStep Gestures (< 8ms), Recents Carousel, Swipe-to-Kill (SIGKILL), Split-Screen
# - 3.4: SystemUI Status Bar (5G/LTE), Quick Settings Tiles, org.freedesktop.Notifications
# - 3.5: Lock Screen, Fingerprint HAL (< 300ms), Virtual Keyboard IME & Viewport Push
# - 3.6: UTIM MPG Freezing (cgroup.freeze) & Dynamic OOM Hierarchy Synchronization
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
echo " UNIVERSAL TREBLE LINUX GSI - PHASE 3 METICULOUS VERIFICATION SUITE"
echo " Android-Style Wayland Compositor & Launcher (UTLC - Rust / Smithay Engine)"
echo " Milestones 3.1 - 3.6: Protocols, UI, Gestures, SystemUI, Security, Power Sync"
echo "=============================================================================="

# ------------------------------------------------------------------------------
header "1. Workspace Unit & Integration Tests (Phase 3 Compositor Coverage)"
# ------------------------------------------------------------------------------

echo "[*] Running entire workspace test suite..."
if cargo test --workspace; then
    pass "All unit and integration tests passed across all workspace crates"
else
    fail "Workspace test suite reported test failures"
fi

echo "[*] Running dedicated Phase 3 compositor integration test suite..."
if cargo test --test compositor_test; then
    pass "Dedicated compositor test suite passed (Milestones 3.1 - 3.6)"
else
    fail "Dedicated compositor test suite failed"
fi

# ------------------------------------------------------------------------------
header "2. Building Release aarch64 Binaries"
# ------------------------------------------------------------------------------

echo "[*] Cross-compiling aarch64 release binaries (including UTLC)..."
if cargo build --release --workspace --target aarch64-unknown-linux-gnu; then
    pass "Workspace compiled successfully for aarch64-unknown-linux-gnu"
else
    fail "Cross-compilation for aarch64 failed"
fi

# Build host diagnostics tools
cargo build --bin utim-graphics-check --bin utlc >/dev/null 2>&1 || true

# ------------------------------------------------------------------------------
header "3. Meticulous 64 KB ELF Alignment Inspection (Android 15+ 16KB Pages)"
# ------------------------------------------------------------------------------

AARCH64_RELEASE="${WORKSPACE_ROOT}/target/aarch64-unknown-linux-gnu/release"
BINARIES=(
    "utim"
    "utimctl"
    "deb-systemd-helper"
    "deb-systemd-invoke"
    "utim-graphics-check"
    "utlc"
)

for bin in "${BINARIES[@]}"; do
    bin_path="${AARCH64_RELEASE}/${bin}"
    if [[ ! -f "${bin_path}" ]]; then
        fail "Binary ${bin} not found at ${bin_path}"
        continue
    fi

    if "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --check-elf "${bin_path}" --json | grep '"is_64k_compatible":true' >/dev/null; then
        pass "Binary ${bin} is strictly 64KB page aligned (p_align >= 0x10000)"
    else
        fail "Binary ${bin} failed 64KB page alignment test!"
    fi
done

# ------------------------------------------------------------------------------
header "4. Milestone 3.1: Smithay Wayland Compositor Core & HWC Backend"
# ------------------------------------------------------------------------------

echo "[*] Checking mobile Wayland protocol extensions..."
PROTO_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --check-protocols --json)
if echo "${PROTO_OUT}" | grep -q '"all_supported":true'; then
    pass "All mobile Wayland protocols verified (xdg-shell, wlr-layer-shell, linux-dmabuf, presentation-time, wp_viewporter, ext-idle-notify, text-input-v3, zwp_tablet_manager_v2)"
else
    fail "Missing required mobile Wayland protocol extensions"
fi

echo "[*] Checking boot-to-launcher time and resident RAM footprint (RSS)..."
BENCH_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --benchmark --json)

if echo "${BENCH_OUT}" | grep -q '"boot_target_met":true'; then
    pass "Boot-to-launcher time verified (< 450 ms target achieved)"
else
    fail "Boot-to-launcher time exceeded 450 ms"
fi

if echo "${BENCH_OUT}" | grep -q '"rss_target_met":true'; then
    pass "Resident RAM (RSS) verified (< 15 MB target achieved)"
else
    fail "Resident RAM footprint exceeded 15 MB"
fi

# ------------------------------------------------------------------------------
header "5. Milestone 3.2: Android Home Screen, Hotseat Dock & Zero-Copy App Drawer"
# ------------------------------------------------------------------------------

echo "[*] Checking zero-allocation .desktop parser and real-time fuzzy search..."
DESK_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-desktop --json)
if echo "${DESK_OUT}" | grep -q '"desktop_search_ok":true'; then
    pass "Zero-allocation .desktop parser and fuzzy search validated"
else
    fail "Desktop parser or fuzzy search validation failed"
fi

if echo "${BENCH_OUT}" | grep -q '"search_target_met":true'; then
    pass "Real-time fuzzy search latency verified (< 1.0 ms query latency)"
else
    fail "Fuzzy search query latency exceeded 1.0 ms"
fi

# ------------------------------------------------------------------------------
header "6. Milestone 3.3: QuickStep Gesture Navigation & Recents Carousel"
# ------------------------------------------------------------------------------

echo "[*] Checking QuickStep gesture engine (< 8ms touch latency, Home, Recents, Back, Scrub)..."
GEST_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-gestures --json)
if echo "${GEST_OUT}" | grep -q '"all_passed":true'; then
    pass "All QuickStep gestures validated (Home ease-out, Recents hold/haptic, Back edge injection)"
else
    fail "QuickStep gesture validation failed"
fi

if echo "${BENCH_OUT}" | grep -q '"touch_target_met":true'; then
    pass "Touch input response latency verified (< 8.0 ms input guarantee)"
else
    fail "Touch input processing latency exceeded 8.0 ms"
fi

# ------------------------------------------------------------------------------
header "7. Milestone 3.4: SystemUI Status Bar, Notification Shade & Quick Settings"
# ------------------------------------------------------------------------------

echo "[*] Checking SystemUI status bar (5G/LTE), Quick Settings tiles, and Notifications..."
SYSUI_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-systemui --json)
if echo "${SYSUI_OUT}" | grep -q '"all_passed":true'; then
    pass "SystemUI Status Bar, Quick Settings tiles (Torch, Wi-Fi), and Notification center verified"
else
    fail "SystemUI validation failed"
fi

# ------------------------------------------------------------------------------
header "8. Milestone 3.5: Lock Screen, Biometrics & Virtual Keyboard (IME)"
# ------------------------------------------------------------------------------

echo "[*] Checking ambient lock screen and Android Fingerprint HAL bridge (< 300ms)..."
LOCK_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-lockscreen --json)
if echo "${LOCK_OUT}" | grep -q '"sub_300ms":true'; then
    pass "Lock screen and Fingerprint HAL bridge verified (sub-300ms biometric unlock)"
else
    fail "Fingerprint HAL bridge failed or latency exceeded 300ms"
fi

echo "[*] Checking integrated Gboard-style virtual keyboard (IME) and viewport push..."
IME_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-ime --json)
if echo "${IME_OUT}" | grep -q '"all_passed":true'; then
    pass "Virtual Keyboard (IME) text-input-v3 and window viewport push animation verified"
else
    fail "Virtual Keyboard IME validation failed"
fi

# ------------------------------------------------------------------------------
header "9. Milestone 3.6: UTIM Power & OOM Governance Synchronization"
# ------------------------------------------------------------------------------

echo "[*] Checking UTIM MPG cgroup v2 freezing and dynamic OOM hierarchy..."
PWR_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-power-sync --json)
if echo "${PWR_OUT}" | grep -q '"all_passed":true'; then
    pass "UTIM Power Governor display sleep freezing and dynamic OOM hierarchy verified"
else
    fail "UTIM power and OOM synchronization failed"
fi

# ------------------------------------------------------------------------------
header "10. Debian Packaging & Rootfs Assembly Integration Check"
# ------------------------------------------------------------------------------

echo "[*] Building Phase 3 UTLC Debian package..."
"${SCRIPT_DIR}/package_utlc.sh" "${WORKSPACE_ROOT}/dist" >/dev/null

UTLC_DEB="${WORKSPACE_ROOT}/dist/utlc_1.0.0_arm64.deb"
if [[ -f "${UTLC_DEB}" ]]; then
    pass "Debian package exists: utlc_1.0.0_arm64.deb"
    if dpkg-deb -I "${UTLC_DEB}" >/dev/null 2>&1; then
        pass "UTLC Debian package metadata integrity verified"
    else
        fail "UTLC Debian package corrupt or invalid control metadata"
    fi
else
    fail "Expected UTLC Debian package missing at ${UTLC_DEB}"
fi

echo "[*] Verifying 64KB ELF alignment of utlc inside Debian package..."
TMP_EXTRACT="$(mktemp -d)"
dpkg-deb -x "${UTLC_DEB}" "${TMP_EXTRACT}"
if "${WORKSPACE_ROOT}/target/debug/utim-graphics-check" --check-elf "${TMP_EXTRACT}/usr/bin/utlc" --json | grep '"is_64k_compatible":true' >/dev/null; then
    pass "Packaged /usr/bin/utlc binary is strictly 64KB page aligned"
else
    fail "Packaged /usr/bin/utlc is NOT 64KB page aligned!"
fi
rm -rf "${TMP_EXTRACT}"

echo "[*] Running mock rootfs build with Phase 3 UTLC integration..."
TEST_ROOTFS="${WORKSPACE_ROOT}/build/verify_rootfs_phase3"
rm -rf "${TEST_ROOTFS}"
DRY_RUN=1 "${SCRIPT_DIR}/build_rootfs.sh" "${TEST_ROOTFS}" >/dev/null

if [[ -f "${TEST_ROOTFS}/usr/bin/utlc" ]]; then
    pass "Rootfs contains /usr/bin/utlc binary"
else
    fail "Rootfs missing /usr/bin/utlc binary"
fi

if [[ -f "${TEST_ROOTFS}/usr/lib/systemd/system/utlc.service" ]]; then
    pass "Rootfs contains /usr/lib/systemd/system/utlc.service"
else
    fail "Rootfs missing /usr/lib/systemd/system/utlc.service"
fi

if [[ -L "${TEST_ROOTFS}/etc/systemd/system/graphical.target.wants/utlc.service" ]]; then
    pass "Rootfs has enabled utlc.service in graphical.target.wants"
else
    fail "Rootfs missing graphical.target.wants/utlc.service symlink"
fi

if grep -q "utlc" "${TEST_ROOTFS}/etc/apt/preferences.d/utim-pinning"; then
    pass "Rootfs APT pinning contains utlc package protection"
else
    fail "Rootfs APT pinning missing utlc"
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
    echo -e "${RED}${BOLD}Phase 3 Verification FAILED with ${FAILED_CHECKS} errors.${NC}"
    exit 1
else
    echo -e "Failed Checks:  ${GREEN}0${NC}"
    echo ""
    echo -e "${GREEN}${BOLD}Phase 3 Android-Style Wayland Compositor & Launcher (UTLC) METICULOUSLY VERIFIED!${NC}"
    echo "All milestones (3.1 - 3.6) passed with zero errors."
    exit 0
fi
