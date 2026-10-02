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

echo "[*] Running entire workspace test suite in RELEASE mode..."
# Release, not debug. The whole point of this tree's budget guards is the
# absolute frame time: `full_frame_stays_inside_the_vsync_budget` is
# `#[cfg_attr(debug_assertions, ignore)]`, so a debug run silently measures
# nothing and reports "all green" for a launcher that would drop every frame
# on the device. Running `--release` here is what makes the run mean anything.
if cargo test --workspace --release; then
    pass "All unit and integration tests passed across all workspace crates (release)"
else
    fail "Workspace test suite reported test failures (release)"
fi

echo "[*] Running dedicated Phase 3 compositor integration test suite (release)..."
# `-p utim_core` because `compositor_test` is utim_core's integration target;
# a bare `cargo test --test compositor_test` at the workspace root matches no
# target at all and runs nothing.
if cargo test -p utim_core --release --test compositor_test; then
    pass "Dedicated compositor test suite passed (Milestones 3.1 - 3.6, release)"
else
    fail "Dedicated compositor test suite failed (release)"
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
header "1b. Release Optimisation Profile Assertion (rasteriser must not be at -Oz)"
# ------------------------------------------------------------------------------

# `opt-level = "z"` (below) switches LLVM's loop and SLP vectorizers OFF, which
# is the wrong trade for a software rasteriser: nearly all of this workspace's
# CPU time is scalar per-pixel loops, and only `slice::fill` /
# `copy_from_slice` autovectorise at "z". `utim_core` is the crate that
# rasterises, so it alone must be exempted with
# `[profile.release.package.utim_core] opt-level = 3`.
#
# Asserted here, and not left to a comment, because the failure mode is
# invisible: deleting the package override does not break the build, does not
# fail a single unit test, and only shows up as a launcher that missed the
# 8.33 ms vsync period on real hardware. A grep turns that into a build-time
# failure with a message that names the fix.
#
# The section body is matched up to the next `[` table header, so an
# `opt-level` belonging to some *other* profile section cannot satisfy this.
CARGO_TOML="${WORKSPACE_ROOT}/Cargo.toml"
PROFILE_BLOCK="$(awk '
    /^\[profile\.release\.package\.utim_core\][[:space:]]*$/ { inblock = 1; print; next }
    inblock && /^\[/ { exit }
    inblock { print }
' "${CARGO_TOML}")"

if [[ -z "${PROFILE_BLOCK}" ]]; then
    fail "Cargo.toml has no [profile.release.package.utim_core] section; the rasteriser is being built at opt-level=\"z\""
elif grep -qE '^[[:space:]]*opt-level[[:space:]]*=[[:space:]]*3[[:space:]]*$' <<< "${PROFILE_BLOCK}"; then
    pass "Cargo.toml pins [profile.release.package.utim_core] opt-level = 3 (rasteriser keeps its vectorizers)"
else
    fail "Cargo.toml [profile.release.package.utim_core] exists but does not set opt-level = 3; the renderer is compiled at the workspace default"
fi

# The budget guard must actually EXECUTE, not be skipped. It is
# `#[cfg_attr(debug_assertions, ignore)]`, so in a debug run it is reported as
# `ignored` and the frame numbers below are absent. This check fails loudly if
# somebody re-adds an unconditional `#[ignore]`, which would turn the frame
# budget into a test that cannot fail.
echo "[*] Confirming the release frame-budget guard actually executes (not ignored)..."
if BUDGET_RUN=$(cargo test --release -p utim_core --lib full_frame_stays_inside_the_vsync_budget -- --nocapture 2>&1); then
    if grep -q "test result: ok. 1 passed" <<< "${BUDGET_RUN}"; then
        pass "full_frame_stays_inside_the_vsync_budget executed and passed in release mode"
    else
        fail "full_frame_stays_inside_the_vsync_budget did not run (0 passed) - it is probably #[ignore]d"
    fi
else
    fail "full_frame_stays_inside_the_vsync_budget FAILED in release mode"
fi

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
if PROTO_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --check-protocols --json); then :; else fail "utlc --check-protocols invocation failed"; PROTO_OUT=""; fi
if echo "${PROTO_OUT}" | grep -q '"all_supported":true'; then
    pass "All mobile Wayland protocols verified (xdg-shell, wlr-layer-shell, linux-dmabuf, presentation-time, wp_viewporter, ext-idle-notify, text-input-v3, zwp_tablet_manager_v2)"
else
    fail "Missing required mobile Wayland protocol extensions"
fi

echo "[*] Checking boot-to-launcher time and resident RAM footprint (RSS)..."
if BENCH_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --benchmark --json); then :; else fail "utlc --benchmark invocation failed"; BENCH_OUT=""; fi

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
header "4b. Release Frame Budget: 8.33 ms (120 Hz) / 16.67 ms (60 Hz)"
# ------------------------------------------------------------------------------

# Two numbers, both hard:
#
#   * 8.33 ms  -- one 120 Hz period. A STEADY state (what the screen shows
#                 when the user is not mid-gesture) that misses this drops
#                 frames on a 120 Hz panel.
#   * 16.67 ms -- one 60 Hz period. EVERY state, transient included, must meet
#                 this: the plan's requirement is zero frame drops across
#                 60/90/120/144 Hz, and 16.67 ms is the floor of that range.
#
# The timings come from `full_frame_stays_inside_the_vsync_budget`, which is
# the only place the real draw path is timed per state. `utlc --benchmark`
# reports boot / RSS / input / search latency but NOT per-state frame cost, so
# it cannot answer this question; the release test run is what can.
echo "[*] Collecting per-state release frame timings (utlc --benchmark has no frame field)..."
if BUDGET_OUT=$(cargo test --release -p utim_core --lib full_frame_stays_inside_the_vsync_budget -- --nocapture 2>&1); then
    pass "Release frame-budget measurement completed"
else
    fail "Release frame-budget measurement run failed; timings below are absent"
fi

# Parse `Duration`'s Debug output, which is a number plus a unit that can be
# any of ns / us / ms / s depending on magnitude. The budget is in
# microseconds, so everything is converted to us and compared as integers --
# a floating-point compare against 8333.0 would make a 8332.6 us frame read as
# over budget on a different host's rounding.
dur_to_us() {
    # $1 = e.g. "5.645773ms", "850us", "1.5s", "900ns"
    local d="$1" num unit
    num="${d%%[a-zµ]*}"
    unit="${d#"${num}"}"
    case "${unit}" in
        ns) awk -v n="${num}" 'BEGIN { printf "%d", n / 1000.0 }' ;;
        us|µs) awk -v n="${num}" 'BEGIN { printf "%d", n }' ;;
        ms) awk -v n="${num}" 'BEGIN { printf "%d", n * 1000.0 }' ;;
        s)  awk -v n="${num}" 'BEGIN { printf "%d", n * 1000000.0 }' ;;
        *)  echo "-1" ;;
    esac
}

FRAME_LINES=""
if [[ -n "${BUDGET_OUT:-}" ]]; then
    FRAME_LINES="$(grep -E '^[[:alnum:]_]+ frame: .*\(release build\)$' <<< "${BUDGET_OUT}" || true)"
fi

if [[ -z "${FRAME_LINES}" ]]; then
    fail "No per-state frame timings were reported - the budget guard did not execute in release mode"
else
    REPORTED=0
    OVER_60HZ=0
    OVER_120HZ=0
    SLOWEST_NAME=""
    SLOWEST_US=0
    while IFS= read -r line; do
        [[ -z "${line}" ]] && continue
        name="${line%% frame:*}"
        dur="${line#* frame: }"
        dur="${dur%% (*}"
        us="$(dur_to_us "${dur}")"
        if [[ "${us}" == "-1" ]]; then
            fail "Could not parse the frame duration for state '${name}' (raw: '${dur}')"
            continue
        fi
        REPORTED=$((REPORTED + 1))
        if (( us > SLOWEST_US )); then
            SLOWEST_US="${us}"
            SLOWEST_NAME="${name}"
        fi
        if (( us > 16667 )); then
            OVER_60HZ=$((OVER_60HZ + 1))
            echo -e "      ${YELLOW}${name}: ${us} us -- OVER the 16667 us (60 Hz) floor${NC}"
        elif (( us > 8333 )); then
            OVER_120HZ=$((OVER_120HZ + 1))
            echo -e "      ${YELLOW}${name}: ${us} us -- over the 8333 us (120 Hz) steady budget (transient states get two periods)${NC}"
        else
            echo -e "      ${name}: ${us} us"
        fi
    done <<< "${FRAME_LINES}"

    echo "[*] Slowest state: ${SLOWEST_NAME} at ${SLOWEST_US} us; ${REPORTED} states measured."
    if (( OVER_60HZ == 0 )); then
        pass "All ${REPORTED} states fit one 60 Hz period (< 16667 us); slowest was ${SLOWEST_NAME} at ${SLOWEST_US} us"
    else
        fail "${OVER_60HZ} of ${REPORTED} states exceed the 16667 us (60 Hz) floor; slowest was ${SLOWEST_NAME} at ${SLOWEST_US} us"
    fi
    if (( OVER_120HZ == 0 )); then
        pass "All ${REPORTED} states also fit one 120 Hz period (< 8333 us); no state needs the transient tier"
    else
        # Not a failure: the Rust guard classifies those states as transient
        # and grants them two 120 Hz periods by design. Reported so the count
        # is visible rather than silently absorbed.
        pass "${OVER_120HZ} of ${REPORTED} states are over 8333 us and are classified transient (two 120 Hz periods) by full_frame_stays_inside_the_vsync_budget"
    fi
fi

# The release binary's own benchmark, for the non-frame latencies.
echo "[*] Running release utlc --benchmark for boot / RSS / input / search latency..."
if REL_BENCH=$("${WORKSPACE_ROOT}/target/release/utlc" --benchmark --json 2>/dev/null); then
    if echo "${REL_BENCH}" | grep -q '"touch_target_met":true'; then
        pass "Release touch-input latency verified (< 8.0 ms guarantee)"
    else
        fail "Release touch-input latency exceeded 8.0 ms"
    fi
    if echo "${REL_BENCH}" | grep -q '"rss_target_met":true'; then
        pass "Release Resident RAM (RSS) verified (< 15 MB target achieved)"
    else
        fail "Release Resident RAM footprint exceeded 15 MB"
    fi
else
    fail "Release utlc --benchmark invocation failed (build target/release/utlc first)"
fi

# ------------------------------------------------------------------------------
header "5. Milestone 3.2: Android Home Screen, Hotseat Dock & Zero-Copy App Drawer"
# ------------------------------------------------------------------------------

echo "[*] Checking zero-allocation .desktop parser and real-time fuzzy search..."
if DESK_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-desktop --json); then :; else fail "utlc --test-desktop invocation failed"; DESK_OUT=""; fi
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
if GEST_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-gestures --json); then :; else fail "utlc --test-gestures invocation failed"; GEST_OUT=""; fi
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
if SYSUI_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-systemui --json); then :; else fail "utlc --test-systemui invocation failed"; SYSUI_OUT=""; fi
if echo "${SYSUI_OUT}" | grep -q '"all_passed":true'; then
    pass "SystemUI Status Bar, Quick Settings tiles (Torch, Wi-Fi), and Notification center verified"
else
    fail "SystemUI validation failed"
fi

# ------------------------------------------------------------------------------
header "8. Milestone 3.5: Lock Screen, Biometrics & Virtual Keyboard (IME)"
# ------------------------------------------------------------------------------

echo "[*] Checking ambient lock screen and Android Fingerprint HAL bridge (< 300ms)..."
if LOCK_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-lockscreen --json); then :; else fail "utlc --test-lockscreen invocation failed"; LOCK_OUT=""; fi
if echo "${LOCK_OUT}" | grep -q '"sub_300ms":true'; then
    pass "Lock screen and Fingerprint HAL bridge verified (sub-300ms biometric unlock)"
else
    fail "Fingerprint HAL bridge failed or latency exceeded 300ms"
fi

echo "[*] Checking integrated Gboard-style virtual keyboard (IME) and viewport push..."
if IME_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-ime --json); then :; else fail "utlc --test-ime invocation failed"; IME_OUT=""; fi
if echo "${IME_OUT}" | grep -q '"all_passed":true'; then
    pass "Virtual Keyboard (IME) text-input-v3 and window viewport push animation verified"
else
    fail "Virtual Keyboard IME validation failed"
fi

# ------------------------------------------------------------------------------
header "9. Milestone 3.6: UTIM Power & OOM Governance Synchronization"
# ------------------------------------------------------------------------------

echo "[*] Checking UTIM MPG cgroup v2 freezing and dynamic OOM hierarchy..."
if PWR_OUT=$("${WORKSPACE_ROOT}/target/debug/utlc" --test-power-sync --json); then :; else fail "utlc --test-power-sync invocation failed"; PWR_OUT=""; fi
if echo "${PWR_OUT}" | grep -q '"all_passed":true'; then
    pass "UTIM Power Governor display sleep freezing and dynamic OOM hierarchy verified"
else
    fail "UTIM power and OOM synchronization failed"
fi

echo "[*] Checking release-mode screenshot capture path..."
if SCREENSHOT_TEST=$(cargo test --release -p utim_core --lib screenshot -- --nocapture 2>&1); then :; else fail "Screenshot release test command failed"; SCREENSHOT_TEST=""; fi
if echo "${SCREENSHOT_TEST}" | grep -qE "test result: ok\. [1-9][0-9]* passed"; then
    pass "Screenshot capture path verified in release mode (utim_core)"
else
    fail "Screenshot release-mode test failed"
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
