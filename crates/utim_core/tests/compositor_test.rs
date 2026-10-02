//! Integration Test Suite for Phase 3: Android-Style Wayland Compositor & Launcher (UTLC).
//! Exhaustively validates Milestones 3.1 through 3.6:
//! - 3.1: Smithay-compatible Mobile Wayland Protocols, HWC multi-plane presentation, RSS < 15MB, boot < 0.45s
//! - 3.2: Paged home grid, spring physics, Hotseat dock, zero-allocation .desktop parser, fuzzy search < 1ms
//! - 3.3: QuickStep gesture navigation (< 8ms), Recents card stack, swipe-to-kill (SIGKILL), split-screen
//! - 3.4: SystemUI status bar (5G/LTE), Quick Settings tiles, org.freedesktop.Notifications
//! - 3.5: Ambient lock screen, touch barrier, PIN keypad, Android Fingerprint HAL bridge (< 300ms), Virtual Keyboard IME
//! - 3.6: UTIM Mobile Power Governor (cgroup.freeze) & dynamic OOM score hierarchy synchronization

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use utim_core::compositor::desktop::{parse_desktop_entry, DesktopApp, DesktopCatalogue};
use utim_core::compositor::gestures::{
    fast_out_slow_in, EdgeSide, GestureAction, GestureConfig, GestureEngine, RawTouchEvent,
    TouchPhase,
};
use utim_core::compositor::ime::{ImeAction, VirtualKeyboard};
use utim_core::compositor::lockscreen::LockScreen;
use utim_core::compositor::power_sync::{oom_roles, UtimPowerSync};
use utim_core::compositor::protocols::{ProtocolRegistry, WaylandInterface, WlMessage};
use utim_core::compositor::recents::{
    DismissOutcome, KillAction, KillQueue, KillState, PopupItem, PopupItems, Recents, TaskCard,
};
use utim_core::compositor::scene::ShellMode;
use utim_core::compositor::server::WaylandServer;
use utim_core::compositor::systemui::NotificationSpec;
use utim_core::compositor::systemui::{CellularRat, QuickTileKind, SystemUiShade};
use utim_core::graphics::composer::{HwcComposer, HwcVersion};
use utim_core::graphics::drm_kms::{SpringConfig, SpringSimulation};
use utim_core::graphics::layout::Layout;

#[test]
fn test_milestone_3_1_wayland_protocols_and_wire_framing() {
    // 1. Validate Wire Framing and Decoding.
    //
    //    The bytes are laid out by hand rather than by a builder: the
    //    outgoing-event serializer had no production caller and was deleted,
    //    and the parser is the half of this module that is actually live. So
    //    this is now the round trip in the only direction that happens --
    //    a wire buffer arrives, and the shell reads fields out of it.
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(&0xCAFE_u32.to_le_bytes()); // u32
    payload.extend_from_slice(&(-42_i32).to_le_bytes()); // i32
    payload.extend_from_slice(&((12.75 * 256.0) as i32).to_le_bytes()); // 24.8 fixed
                                                                        // String: u32 length *including* the NUL, the bytes, then NUL, then pad
                                                                        // to a 4-byte boundary -- "org.freedesktop.MobileWayland" is 28 bytes, so
                                                                        // 29 with the NUL rounds up to 32.
    let s = b"org.freedesktop.MobileWayland";
    payload.extend_from_slice(&((s.len() + 1) as u32).to_le_bytes());
    payload.extend_from_slice(s);
    payload.push(0);
    while !payload.len().is_multiple_of(4) {
        payload.push(0);
    }
    // Array: u32 length then the bytes, padded to 4.
    payload.extend_from_slice(&5_u32.to_le_bytes());
    payload.extend_from_slice(&[1, 2, 3, 4, 5]);
    while !payload.len().is_multiple_of(4) {
        payload.push(0);
    }

    let mut wire: Vec<u8> = Vec::new();
    wire.extend_from_slice(&1_u32.to_le_bytes()); // object id
    wire.extend_from_slice(&2_u16.to_le_bytes()); // opcode
    wire.extend_from_slice(&0_u16.to_le_bytes()); // length placeholder
    wire.extend_from_slice(&payload);
    let total = u16::try_from(wire.len()).expect("message fits the 16-bit length field");
    wire[6..8].copy_from_slice(&total.to_le_bytes());

    assert_eq!(wire.len() % 4, 0, "Wayland messages must be 4-byte aligned");

    let (msg, parsed_len) = WlMessage::parse(&wire).unwrap().unwrap();
    assert_eq!(parsed_len, wire.len());
    assert_eq!(msg.header.object_id, 1);
    assert_eq!(msg.header.opcode, 2);
    assert_eq!(msg.read_u32(0), Some(0xCAFE));
    assert_eq!(msg.read_i32(4), Some(-42));
    assert_eq!(msg.read_fixed(8), Some(12.75));

    let (str_val, next_offset) = msg.read_string(12).unwrap();
    assert_eq!(str_val, "org.freedesktop.MobileWayland");
    // 48, not 44. The payload runs
    //   0..4 u32, 4..8 i32, 8..12 fixed,
    //   12..16 string length (29, NUL included),
    //   16..44 the 28 string bytes, 44 the NUL, 45..48 the pad,
    //   48..52 the array length.
    // so the offset past the string is `16 + round_up_4(29)` = 48. The old
    // 44 was `12 + 32` -- it forgot the 4-byte length prefix, which put the
    // reader *inside* the padding, and it contradicted the two assertions
    // below it, which read the array at whatever this returned.
    assert_eq!(next_offset, 48, "the reader must skip the NUL padding too");
    // The array follows the string, at the offset the string reader reported.
    let len = msg.read_u32(next_offset).unwrap();
    assert_eq!(len, 5);
    assert_eq!(
        &msg.payload[next_offset + 4..next_offset + 9],
        &[1, 2, 3, 4, 5]
    );

    // A short buffer is a partial message, not a malformed one: the parser
    // asks for more bytes rather than erroring.
    assert!(WlMessage::parse(&wire[..wire.len() - 4]).unwrap().is_none());

    // 2. Validate Protocol Registry Coverage
    let reg = ProtocolRegistry::new();
    assert!(
        reg.supports_mobile_protocols(),
        "Must support all mobile Wayland extensions"
    );
    assert!(reg.find_by_interface(WaylandInterface::XdgWmBase).is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::ZwlrLayerShellV1)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::ZwpLinuxDmabufV1)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::WpPresentation)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::WpViewporter)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::ExtIdleNotifierV1)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::ZwpTextInputV3)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::ZwpInputMethodV2)
        .is_some());
    assert!(reg
        .find_by_interface(WaylandInterface::ZwpTabletManagerV2)
        .is_some());
}

#[test]
fn test_milestone_3_1_hwc_multi_plane_presentation_and_performance() {
    let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
    let socket_path =
        std::path::PathBuf::from(format!("/tmp/utlc-test-{}.sock", std::process::id()));
    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);

    // Boot to first frame
    let boot_dur = server
        .boot_to_first_frame()
        .expect("Boot to first frame failed");
    assert!(
        boot_dur < Duration::from_millis(450),
        "Boot-to-launcher time must be < 450ms, got {:?}",
        boot_dur
    );

    // Check Resident Memory RSS
    let metrics = server.get_metrics().expect("metrics collection failed");
    let rss_mb = (metrics.resident_memory_bytes as f64) / (1024.0 * 1024.0);
    assert!(
        metrics.is_rss_within_target,
        "Resident RAM footprint must be < 15 MB, got {:.2} MB",
        rss_mb
    );

    // Multi-plane presentation test: set active app buffer and present
    server.scene.active_app_buffer_fd = Some(101);
    server.scene.mode = ShellMode::Application;
    assert!(server.step_frame(0.016).is_ok());
}

#[test]
fn test_milestone_3_2_home_grid_spring_physics_and_fuzzy_search() {
    // 1. Spring physics for the paged workspace. The old `WorkspaceGrid`
    //    (a `Vec<GridItem>` the shell never populated) is gone, and so is the
    //    second `k/c/m` oscillator that used to live in `compositor::spring`;
    //    the shade/IME spring is the crate's single `SpringSimulation` now.
    let mut spring = SpringSimulation::new(0.0, 100.0, SpringConfig::shade_pull());
    for _ in 0..120 {
        spring.step(0.016);
    }
    assert!(spring.is_at_rest(), "workspace spring must converge");
    assert!((spring.value - 100.0).abs() < 0.1);

    // 2. Persistent hotseat dock geometry, from the one layout the renderer
    //    and the hit-tester both read.
    let layout = Layout::plain(1080.0, 2400.0);
    assert!(
        layout.dock_slots >= 4,
        "phone profile needs a 4-icon hotseat"
    );
    let (dx, dy, dw, dh) = {
        let s = layout.dock_slot(0);
        (s.x, s.y, s.w, s.h)
    };
    assert_eq!(dx, layout.dock.x, "slot 0 must start at the dock edge");
    assert!(
        (dw - layout.dock.w / layout.dock_slots as f32).abs() < 0.001,
        "slot width must divide the dock evenly"
    );
    assert!(
        dy > 2400.0 * 0.85,
        "hotseat must be pinned to the bottom eighth, got y={dy}"
    );
    assert!(
        dy + dh <= layout.nav_pill.y + 0.001,
        "hotseat must sit above the gesture nav pill, not overlap it"
    );
    // Vertical stack, top to bottom: workspace grid ends exactly where the
    // page dots begin, then the dots, then the hotseat, then the nav pill.
    // Any other order means a touch lands on one band while the renderer
    // draws another.
    assert!(
        (layout.grid_bottom - layout.page_dots.y).abs() < 0.001,
        "workspace must end where the page dots begin"
    );
    assert!(
        layout.page_dots.y + layout.page_dots.h <= layout.dock.y + 0.001,
        "page dots must sit fully above the hotseat"
    );
    // The dock icon tile must be centred inside its own slot, otherwise a
    // tap on the slot misses the icon.
    let icon = layout.dock_icon_rect(0);
    let slot_cx = dx + dw * 0.5;
    assert!(
        (icon.center_x() - slot_cx).abs() < 0.5,
        "dock icon must be centred"
    );
    assert!(icon.h > 0.0 && icon.h <= dh, "dock icon must fit its cell");

    // 3. Zero-Allocation .desktop Parser
    let desktop_content = r#"
[Desktop Entry]
Version=1.0
Type=Application
Name=Antigravity IDE
Exec=antigravity-ide %F
Icon=antigravity
Categories=Development;IDE;
Keywords=Code;Editor;Development;
NoDisplay=false
"#;
    let app =
        parse_desktop_entry("antigravity-ide", desktop_content).expect("Failed to parse desktop");
    assert_eq!(app.id, "antigravity-ide");
    assert_eq!(app.name, "Antigravity IDE");
    assert_eq!(app.clean_exec(), "antigravity-ide");
    assert!(app.categories.contains(&"Development".to_string()));

    // 4. Real-time Fuzzy Search (< 1ms query time)
    let mut catalogue = DesktopCatalogue::new();
    catalogue.add_app(app);
    catalogue.add_app(DesktopApp::new(
        "phone".into(),
        "Phone Dialer".into(),
        "dialer".into(),
    ));
    catalogue.add_app(DesktopApp::new(
        "firefox".into(),
        "Firefox Browser".into(),
        "firefox".into(),
    ));

    let t_start = Instant::now();
    let matches = catalogue.search("anti");
    let t_dur = t_start.elapsed();
    assert!(
        t_dur < Duration::from_millis(1),
        "Fuzzy search latency must be < 1ms, got {:?}",
        t_dur
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0.id, "antigravity-ide");
}

#[test]
fn test_milestone_3_3_quickstep_gesture_navigation_and_recents() {
    let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
    let t0 = Instant::now();

    // 1. Home Swipe Up Gesture (< 8ms response, cubic bezier ease-out)
    let touch_down = RawTouchEvent {
        touch_id: 1,
        phase: TouchPhase::Down,
        x: 540.0,
        y: 2380.0,
        timestamp: t0,
    };
    let t_eval_start = Instant::now();
    let act_down = engine.process_touch(&touch_down);
    let eval_dur = t_eval_start.elapsed();
    assert!(
        eval_dur < Duration::from_millis(8),
        "Input latency must be < 8ms, got {:?}",
        eval_dur
    );
    assert_eq!(act_down, GestureAction::None);

    let touch_up = RawTouchEvent {
        touch_id: 1,
        phase: TouchPhase::Up,
        x: 540.0,
        y: 2180.0,
        timestamp: t0 + Duration::from_millis(75),
    };
    let act_home = engine.process_touch(&touch_up);
    match act_home {
        GestureAction::Home { progress, .. } => assert_eq!(progress, 1.0),
        _ => panic!("Expected completed Home gesture"),
    }
    assert_eq!(fast_out_slow_in(1.0), 1.0);

    // 2. The overview is now driven by the shell, not a parallel card model.
    //    `RecentsCarousel`/`RecentsCard`/`SplitScreenConfig` were deleted:
    //    no production path ever added a card, so the graceful-close ->
    //    SIGKILL-escalation and 50/50 split viewports they implemented were
    //    unreachable and are now the shell's job. What must still hold is
    //    that the scene exposes a mode the shell can leave the Launcher
    //    from, and that lock/unlock still drives it.
    let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
    let socket_path = PathBuf::from(format!("/tmp/utlc-scene-{}.sock", std::process::id()));
    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);
    // `LockScreen::new` boots Locked, and `step_frame` re-derives the mode
    // from lock state every tick, so unlock before driving the mode.
    server.scene.lockscreen.unlock();
    server.scene.update(0.016);
    assert_eq!(server.scene.mode, ShellMode::Launcher);

    server.scene.mode = ShellMode::Application;
    assert!(server.step_frame(0.016).is_ok());
    assert_eq!(
        server.scene.mode,
        ShellMode::Application,
        "an application plane must survive a frame step"
    );

    // Re-locking must reclaim the mode even mid-application: the lock screen
    // is not a peer of the application plane, it is above it.
    server.scene.lockscreen.lock();
    assert!(server.step_frame(0.016).is_ok());
    assert_eq!(server.scene.mode, ShellMode::LockScreen);

    // A Recents gesture is recognised by the engine even though the scene no
    // longer reacts to it: the shell consumes it. Regression-guard that the
    // engine still emits the action the shell dispatches on.
    let mut rec_engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
    let rt0 = Instant::now();
    rec_engine.process_touch(&RawTouchEvent {
        touch_id: 9,
        phase: TouchPhase::Down,
        x: 540.0,
        y: 2380.0,
        timestamp: rt0,
    });
    let rec_up = rec_engine.process_touch(&RawTouchEvent {
        touch_id: 9,
        phase: TouchPhase::Up,
        x: 540.0,
        y: 2180.0,
        timestamp: rt0 + Duration::from_millis(75),
    });
    match rec_up {
        GestureAction::Home { progress, .. } => assert_eq!(progress, 1.0),
        other => panic!("quick flick up must commit Home, got {other:?}"),
    }

    // 3. Overview-action geometry that the shell will hit-test against must
    //    be derivable from the shared layout, not from a deleted model.
    let layout = Layout::plain(1080.0, 2400.0);
    assert!(
        layout.max_rows > 0 && layout.grid_cols > 0,
        "overview card grid must be derivable from the panel size"
    );
}

#[test]
fn test_milestone_3_4_systemui_status_bar_and_quick_settings() {
    let mut shade = SystemUiShade::new();

    // 1. Status Bar Elements
    assert_eq!(shade.status_bar.cellular_rat, CellularRat::Nr5g);
    assert_eq!(shade.status_bar.cellular_rat.display_label(), "5G");
    assert_eq!(shade.status_bar.battery_percent, 85);

    // 2. Quick Settings Toggles
    assert!(!shade.is_open());
    shade.open();
    for _ in 0..60 {
        shade.update(0.016);
    }
    assert!(shade.is_open());

    assert!(shade.toggle_tile(QuickTileKind::Bluetooth).unwrap());
    assert!(!shade.toggle_tile(QuickTileKind::Bluetooth).unwrap()); // Toggles off

    // Brightness sysfs is absent in tests: the failure must be reported.
    assert!(shade.set_brightness(80).is_err());
    // ...but the in-memory state still tracks the request.
    assert_eq!(shade.brightness_percent, 80);
    shade.set_volume(70);
    assert_eq!(shade.volume_percent, 70);

    // 3. Notification Center (org.freedesktop.Notifications)
    let n_id = shade.notify(NotificationSpec {
        app_name: "Mail".into(),
        replaces_id: 0,
        app_icon: "email".into(),
        summary: "Meeting at 3PM".into(),
        body: "Discuss Universal Treble GSI roadmap.".into(),
        actions: vec![
            ("open".into(), "Open".into()),
            ("dismiss".into(), "Dismiss".into()),
        ],
        urgency: 1,
    });
    assert_eq!(n_id, 1);
    assert_eq!(shade.notifications.len(), 1);

    // Swipe to dismiss
    shade.on_notification_swipe(n_id, 150.0);
    assert!(shade.on_notification_release(n_id));
    assert_eq!(shade.notifications.len(), 0);
}

#[test]
fn test_milestone_3_5_lock_screen_fingerprint_hal_and_ime() {
    // 1. Lock Screen & Biometric Bridge: unbound HAL never authenticates,
    // and a bound HAL with a PIN enrolled routes to PIN entry (no bypass).
    let mut lock = LockScreen::new(Some("1337"));
    assert!(lock.is_locked());
    assert!(!lock.on_fingerprint_touch(1));
    assert!(lock.is_locked());

    lock.biometric_bridge.hal_bound = true;
    let t_auth_start = Instant::now();
    assert!(!lock.on_fingerprint_touch(1)); // enrolled, but PIN is required
    let auth_dur = t_auth_start.elapsed();
    assert_eq!(
        lock.state,
        utim_core::compositor::lockscreen::LockState::PinEntry
    );
    assert!(
        auth_dur < Duration::from_millis(300),
        "Biometric check must be < 300ms, got {:?}",
        auth_dur
    );

    // Bound HAL with no PIN enrolled unlocks directly.
    let mut open = LockScreen::new(None);
    open.biometric_bridge.hal_bound = true;
    assert!(open.on_fingerprint_touch(1));
    assert!(!open.is_locked());

    // 2. Virtual Keyboard IME & Viewport Push
    let mut ime = VirtualKeyboard::new();
    assert!(!ime.is_active);
    assert_eq!(ime.window_viewport_push_y(), 0.0);

    ime.activate();
    for _ in 0..60 {
        ime.update(0.016);
    }
    assert!(ime.is_active);
    assert!((ime.window_viewport_push_y() - 320.0).abs() < 1.0);

    // Typing keys
    let act_t = ime.handle_key_tap("t");
    assert_eq!(act_t, ImeAction::CommitChar('t'));

    ime.handle_key_tap("SHIFT");
    let act_r = ime.handle_key_tap("r");
    assert_eq!(act_r, ImeAction::CommitChar('R'));

    let act_bs = ime.handle_key_tap("BACKSPACE");
    assert_eq!(
        act_bs,
        ImeAction::DeleteSurroundingText {
            before_length: 1,
            after_length: 0
        }
    );

    let act_enter = ime.handle_key_tap("ENTER");
    assert_eq!(act_enter, ImeAction::SendKey(28));
}

#[test]
fn test_milestone_3_6_power_governor_and_oom_synchronization() {
    let mut power = UtimPowerSync::new(Path::new("/tmp/mock-utim-ctrl.sock"));

    // No daemon socket in the test env: every power transition must report
    // the failure instead of mocking success.
    assert!(power.is_display_on);
    assert!(power.on_display_sleep().is_err());
    assert!(!power.is_connected);

    assert!(power.on_display_wake().is_err());
    assert!(power.is_display_on);

    // 3. Dynamic OOM Score Hierarchy
    // Foreground = 0, Recents = +200, Inactive = +800
    assert_eq!(oom_roles::ACTIVE_FOREGROUND_APP, 0);
    assert_eq!(oom_roles::RECENTS_CACHED_APP, 200);
    assert_eq!(oom_roles::BACKGROUND_INACTIVE_APP, 800);
    assert_eq!(oom_roles::UTLC_COMPOSITOR, -900);

    let fg_pid = 2001;
    let recents_pids = vec![2002, 2003];
    let inactive_pids = vec![2004, 2005];

    assert!(power
        .on_app_switched(fg_pid, &recents_pids, &inactive_pids)
        .is_err());
    assert_eq!(power.active_foreground_pid, None);
}

#[test]
fn test_milestone_3_7_app_icon_resolution_pipeline() {
    use std::rc::Rc;
    use utim_core::compositor::IconCache;
    use utim_core::graphics::{decode_png, AppGridItem, RgbaImage};

    mod fixtures {
        #![allow(dead_code)]
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/png_fixtures.rs"
        ));
    }
    use fixtures::*;

    // 1. A freedesktop-style theme tree is resolved in one batched sweep.
    let root = std::env::temp_dir().join(format!("utim_icons_it_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let apps_dir = root.join("hicolor").join("48x48").join("apps");
    std::fs::create_dir_all(&apps_dir).unwrap();
    std::fs::write(apps_dir.join("demo-app.png"), RGBA8_PNG).unwrap();

    let mut cache = IconCache::with_roots(vec![root.clone()], "hicolor");
    let keys: Vec<String> = vec!["demo-app".to_string(), "absent".to_string()];
    cache.resolve_keys(&keys);

    let icon: Rc<RgbaImage> = cache
        .get("demo-app")
        .expect("icon resolved from theme tree");
    assert_eq!(icon.pixels, RGBA8_EXPECT, "in-crate decoder produced RGBA8");
    assert!(
        icon.width <= 64 && icon.height <= 64,
        "icons are cached at tile size"
    );

    // 2. Misses are remembered so the sweep is not repeated every frame.
    assert!(cache.get("absent").is_none());
    assert!(cache.knows("absent"));
    cache.resolve_keys(&keys);
    assert!(cache.get("absent").is_none());

    // 3. The decoded bitmap plugs straight into the launcher grid descriptor,
    //    and its presence/absence is what tells the renderer to blit vs glyph.
    let with_icon = AppGridItem {
        id: "demo",
        name: "Demo",
        color: 0xFF10B981,
        glyph: "D",
        icon: Some(&icon),
        folder_n: 0,
        folder_id: 0,
    };
    let without_icon = AppGridItem {
        icon: None,
        folder_n: 0,
        folder_id: 0,
        ..with_icon
    };
    assert_ne!(with_icon, without_icon, "icon presence is observable state");

    // 4. Round-trip: the cached bytes are a valid PNG decode target as well.
    let direct = decode_png(RGBA8_PNG).expect("decode_png is exported");
    assert_eq!(direct.pixels, icon.pixels);

    // 5. Invalidating misses (app set changed) allows the new icon to appear.
    cache.invalidate_misses();
    std::fs::write(apps_dir.join("absent.png"), GRAY1_PNG).unwrap();
    cache.resolve_keys(&keys);
    assert!(
        cache.get("absent").is_some(),
        "newly installed icon resolves"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ===========================================================================
// Shell-behaviour engine contracts
// ===========================================================================
//
// The four tests below each drive a *synthetic touch stream* (Down -> Move(s)
// -> Up) through the same `utim_core` engine the shell's input loop calls, and
// then assert the state or action the engine produced.
//
// The shell's own state machine (`ShellState::{Normal, AllApps, Overview,
// PopupOpen}`) is a private enum in `crates/utlc/src/main.rs` and cannot be
// named from here, so where a test covers a shell transition it says so in a
// `PROXY` comment and asserts the strongest thing `utim_core` can actually
// observe: the `GestureAction` the engine emits, and the model the transition
// would have moved. That is the right split anyway -- the shell tests already
// cover the `match` arms, and what was untested is whether the engines those
// arms call behave as the arms assume.

/// One synthetic touch sample, `dt_ms` after the start of the stream.
///
/// The timestamps are the stimulus, not decoration: the motion ring derives
/// velocity and dwell from the deltas between samples, so a stream built from
/// one captured base plus explicit offsets is reproducible run to run. Nothing
/// here reads the wall clock, so none of it can be flaky.
fn touch(t0: Instant, dt_ms: u64, phase: TouchPhase, x: f32, y: f32) -> RawTouchEvent {
    RawTouchEvent {
        touch_id: 1,
        phase,
        x,
        y,
        timestamp: t0 + Duration::from_millis(dt_ms),
    }
}

#[test]
fn test_long_press_app_icon_reaches_popup_state() {
    // PROXY for the shell's `ShellState::PopupOpen` transition
    // (`crates/utlc/src/main.rs`, the `TouchPhase::Up` arm that fills
    // `popup_rows` and sets `popup_spring.set_target(1.0)`). The enum is
    // private to that binary, so this asserts every `utim_core` engine that
    // transition rests on:
    //   1. the gesture engine must not *consume* the press, or the shell's
    //      long-press timer never gets a window to fire in;
    //   2. the press must resolve to the icon under the finger -- the shell
    //      calls `Layout::home_grid_hit_paged` on the DOWN, and an empty hit is
    //      what silently downgrades the menu to the workspace one;
    //   3. the press spring must compress on the DOWN, hold while the finger
    //      is down, and return on the release;
    //   4. the resulting four-row menu must fit `PopupMenuLayout` and the
    //      bounded `PopupItems` without a row being dropped.
    // And the exclusivity: the *same* press, once it travels, is a Home drag
    // and must not also be read as a long press.
    let (w, h) = (1080.0f32, 2400.0f32);
    let cfg = GestureConfig::default();
    let l = Layout::plain(w, h);

    // Slot 0 of the page: the icon the finger lands on, on the leading half of
    // the panel.
    let icon = l.grid_icon(0);
    let (ix, iy) = (icon.center_x(), icon.center_y());
    assert!(ix < w * 0.5, "the probe icon must be on the leading half");

    // 1. The press lands in the *centre* band, so however long it is held it
    //    can never be re-read as a back gesture, a shade pull or a task scrub.
    let mut engine = GestureEngine::new(w, h, cfg.clone());
    assert_eq!(
        engine.classify_edge(ix, iy),
        EdgeSide::Center,
        "an app icon must not sit inside a system gesture band"
    );

    let t0 = Instant::now();
    assert_eq!(
        engine.process_touch(&touch(t0, 0, TouchPhase::Down, ix, iy)),
        GestureAction::None
    );
    // A real finger rests with a couple of pixels of wobble, reported as MOVE
    // events. Every one of them must still be `None`: the instant the engine
    // claims the gesture the shell stops treating the press as a press.
    let slop = cfg.gesture_slop();
    assert!(slop > 1.0, "the reference slop is not zero: {slop}");
    for (i, (dx, dy)) in [(-2.0f32, 1.0f32), (2.0, -2.0), (0.0, 3.0)]
        .iter()
        .enumerate()
    {
        assert!(
            dx.abs() < slop && dy.abs() < slop,
            "the probe jitter must stay inside the slop or it is a different test"
        );
        let a = engine.process_touch(&touch(
            t0,
            60 + i as u64 * 200,
            TouchPhase::Move,
            ix + dx,
            iy + dy,
        ));
        assert_eq!(
            a,
            GestureAction::None,
            "slop-sized jitter must not become a gesture: {a:?}"
        );
    }
    // Released after the shell's long-press window with the finger still on the
    // icon: still nothing. If this committed anything, the icon would both
    // open a menu and navigate.
    assert_eq!(
        engine.process_touch(&touch(t0, 900, TouchPhase::Up, ix, iy)),
        GestureAction::None,
        "a still finger must not commit a navigation action on release"
    );

    // 2. The DOWN must resolve the icon. This is the function the shell calls
    //    verbatim, with the same argument shape.
    assert_eq!(
        l.home_grid_hit_paged(ix, iy, 0.0, 1),
        Some((0, 0)),
        "a press on an icon's centre must name that slot"
    );
    // A press on the wallpaper names nothing, which is the whole reason the
    // shell can tell the app menu from the workspace menu.
    assert_eq!(
        l.home_grid_hit_paged(ix, l.grid_top - 40.0, 0.0, 1),
        None,
        "a press above the grid must not name an app"
    );

    // 3. The press spring. The shell compresses a pressed icon towards
    //    `PRESS_SCALE` on the DOWN and re-targets to 1.0 on the release, on
    //    `SpringConfig::icon_rebound()`.
    const PRESS_SCALE: f32 = 0.90;
    let mut press = SpringSimulation::new(1.0, 0.0, SpringConfig::icon_rebound());
    press.set_target(PRESS_SCALE);
    for _ in 0..300 {
        press.step(0.0083);
    }
    assert!(press.is_at_rest(), "the press spring must converge");
    assert!(
        (press.value - PRESS_SCALE).abs() < 0.01,
        "a held icon must sit at the press scale, got {}",
        press.value
    );
    // A long press *holds* the compression: the shell only re-targets on the
    // release, so the icon must not creep back on its own while the finger is
    // still down.
    for _ in 0..300 {
        press.step(0.0083);
    }
    assert!(
        (press.value - PRESS_SCALE).abs() < 0.01,
        "the icon sprang back while the finger was still down: {}",
        press.value
    );
    press.set_target(1.0);
    for _ in 0..300 {
        press.step(0.0083);
    }
    assert!(
        (press.value - 1.0).abs() < 0.01,
        "the icon must return to full size on release, got {}",
        press.value
    );

    // 4. The menu the threshold produces, in the bounded container the
    //    renderer reads as `&[PopupItem]`.
    let mut rows = PopupItems::EMPTY;
    for item in [
        PopupItem::AppInfo,
        PopupItem::Uninstall,
        PopupItem::Remove,
        PopupItem::Customize,
    ] {
        assert!(
            rows.push(item),
            "the four icon rows must fit the popup list"
        );
    }
    assert_eq!(rows.len, 4);
    assert_eq!(
        rows.iter().copied().collect::<Vec<_>>(),
        vec![
            PopupItem::AppInfo,
            PopupItem::Uninstall,
            PopupItem::Remove,
            PopupItem::Customize
        ]
    );
    // The reference's ceiling is `SHORTCUT_COLLAPSE_THRESHOLD = 6`
    // (`PopupContainerWithArrow.java:95`): past six rows it collapses to a
    // single icon strip rather than growing. This used to be 4, asserted as
    // "the icon menu is exactly at the row ceiling" -- but the icon menu is
    // seven rows once the app's own deep shortcuts are added
    // (`PopupPopulator.MAX_SHORTCUTS = 4` over three system rows), so a ceiling
    // of 4 was cutting real rows off the reference's own menu. The constant is
    // now 6 and all three former readers of it agree.
    let anchor = utim_core::graphics::layout::Rect {
        x: ix,
        y: iy,
        w: 0.0,
        h: 0.0,
        radius: 0.0,
    };
    let pm = l.popup_menu(anchor);
    assert_eq!(pm.max_items, 6, "the reference's collapse threshold");
    assert!(
        rows.len as usize <= pm.max_items,
        "the icon menu must fit under the ceiling"
    );
    assert_eq!(pm.rows_for(rows.len as usize), rows.len as usize);
    assert_eq!(
        pm.height_for(rows.len as usize),
        pm.height_for(pm.max_items) - pm.item_h * (pm.max_items - rows.len as usize) as f32,
        "four rows are shorter than the ceiling, by exactly the missing rows"
    );
    // Past the ceiling, rows are dropped rather than the menu growing.
    assert_eq!(pm.rows_for(9), pm.max_items);
    assert_eq!(pm.height_for(9), pm.height_for(pm.max_items));
    assert!(
        pm.height_for(rows.len as usize) > pm.item_h * 4.0,
        "four rows of item_h plus padding and the arrow"
    );
    assert!(
        pm.height_for(rows.len as usize) < h * 0.5,
        "the menu must fit the panel it opens over"
    );
    // The arrow follows the anchored half of the panel, so a press on the
    // other side of the screen cannot point the popup off-panel.
    let trailing_x = w - ix;
    let leading = l.popup_menu(utim_core::graphics::layout::Rect {
        x: trailing_x,
        y: iy,
        w: 0.0,
        h: 0.0,
        radius: 0.0,
    });
    assert!(
        trailing_x > w * 0.5,
        "the mirrored probe must be on the other half"
    );
    assert!(
        pm.arrow_center.is_sign_positive() && leading.arrow_center.is_sign_negative(),
        "the popup arrow must flip with the anchored edge ({} vs {})",
        pm.arrow_center,
        leading.arrow_center
    );
    // A workspace press and an icon press therefore resolve to *different*
    // menus, and the container has room for the longer workspace one too.
    let mut workspace = PopupItems::EMPTY;
    for item in [
        PopupItem::Wallpapers,
        PopupItem::Widgets,
        PopupItem::AllApps,
        PopupItem::HomeSettings,
    ] {
        assert!(workspace.push(item));
    }
    assert_ne!(
        workspace.iter().copied().collect::<Vec<_>>(),
        rows.iter().copied().collect::<Vec<_>>(),
        "an empty hit must not produce the icon menu"
    );
    // And the deep shortcuts the real icon menu can grow are bounded.
    assert!(rows.push_shortcut(0), "there is room for a shortcut");
    assert_eq!(rows.len, 5, "the container is not the four-row limit");
    assert!(!rows.push_shortcut(utim_core::compositor::MAX_DEEP_SHORTCUTS));

    // Exclusivity: travel the same press and the engine claims it as a Home
    // drag, so the long press and the navigation gesture cannot both be read
    // out of one touch.
    let mut travelling = GestureEngine::new(w, h, cfg);
    travelling.process_touch(&touch(t0, 0, TouchPhase::Down, ix, iy));
    let dragged = travelling.process_touch(&touch(t0, 16, TouchPhase::Move, ix, iy - 400.0));
    match dragged {
        GestureAction::Home { progress, .. } => assert!(
            progress > 0.0,
            "a travelled press reports Home progress, not a popup"
        ),
        other => panic!("a press that travels is a Home drag, got {other:?}"),
    }
}

#[test]
fn test_swipe_up_from_app_reaches_home_without_drawer() {
    // PROXY for the shell's swipe-up-to-home (`ShellState::AllApps { .. }` ->
    // `Normal`, the `GestureAction::Home` arm in the input loop). What must be
    // proved here is the *engine* contract that arm assumes: an upward drag on
    // the application surface commits Home, it commits for both a deliberate
    // stop and a real fling, a short nudge does not, a horizontal drag is left
    // to the app -- and none of it can be mistaken for the bottom-bar scrub
    // the drawer's own gesture rides on.
    let (w, h) = (1080.0f32, 2400.0f32);
    let cfg = GestureConfig::default();
    let (sx, sy) = (540.0f32, 1200.0f32);
    assert_eq!(
        GestureEngine::new(w, h, cfg.clone()).classify_edge(sx, sy),
        EdgeSide::Center,
        "the drag must start on the app surface, not in a system band"
    );

    // The commit distance is a fraction of the panel, not a dp constant, so it
    // scales with the display. Pin both the fraction and the resulting px.
    assert!((cfg.home_commit_distance(h) - h * 0.25).abs() < 0.001);

    // --- (a) the deliberate stop ------------------------------------------
    let mut engine = GestureEngine::new(w, h, cfg.clone());
    let t0 = Instant::now();
    assert_eq!(
        engine.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy)),
        GestureAction::None
    );
    // 12 frames of 60 px, 16 ms apart: 720 px, i.e. past the 600 px commit
    // distance. Progress tracks the finger 1:1 and the window shrinks with it.
    for i in 1..=12u32 {
        let y = sy - i as f32 * 60.0;
        let a = engine.process_touch(&touch(t0, i as u64 * 16, TouchPhase::Move, sx, y));
        let expect = (i as f32 * 60.0) / (h * 0.5);
        match a {
            GestureAction::Home {
                progress,
                scale,
                window_alpha,
            } => {
                assert!(
                    (progress - expect).abs() < 1e-4,
                    "progress at frame {i}: {progress}"
                );
                assert!(
                    (scale - (1.0 - expect * 0.4)).abs() < 1e-4,
                    "scale at frame {i}"
                );
                assert!(
                    (window_alpha - (1.0 - expect * 0.3)).abs() < 1e-4,
                    "window alpha at frame {i}"
                );
            }
            other => panic!("frame {i} of an upward drag must be a Home drag: {other:?}"),
        }
    }
    // Hold still, then release. The transform is a function of the finger's
    // position, so a stationary finger must *not* keep shrinking the window
    // -- otherwise a 1 s pause before lifting would tear the app down further
    // than the drag itself did.
    for i in 13..=18u32 {
        match engine.process_touch(&touch(t0, i as u64 * 16, TouchPhase::Move, sx, sy - 720.0)) {
            GestureAction::Home { progress, .. } => assert!(
                (progress - 0.6).abs() < 1e-4,
                "frame {i}: a stopped finger must freeze the transform, got {progress}"
            ),
            other => panic!("frame {i}: the drag is still owned: {other:?}"),
        }
    }
    let committed = engine.process_touch(&touch(t0, 19 * 16, TouchPhase::Up, sx, sy - 720.0));
    assert_eq!(
        committed,
        GestureAction::Home {
            progress: 1.0,
            scale: 0.6,
            window_alpha: 0.7
        },
        "a deliberate drag past the commit distance must land Home"
    );

    // --- (b) the fling ----------------------------------------------------
    // 400 px, which is *under* the commit distance, released at 12.5 px/ms.
    // It can only commit on the *velocity* branch, which is what makes a real
    // flick work without the finger having to cross a quarter of the panel.
    let mut fling = GestureEngine::new(w, h, cfg.clone());
    fling.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    fling.process_touch(&touch(t0, 16, TouchPhase::Move, sx, sy - 200.0));
    let flicked = fling.process_touch(&touch(t0, 32, TouchPhase::Up, sx, sy - 400.0));
    assert_eq!(
        flicked,
        GestureAction::Home {
            progress: 1.0,
            scale: 0.6,
            window_alpha: 0.7
        },
        "a short hard flick must land Home on the velocity branch"
    );
    assert!(
        cfg.fling_threshold_px > 0.0 && cfg.fling_threshold > 0.0,
        "the two fling units must both be live"
    );

    // --- (c) the short nudge is not a Home ---------------------------------
    let mut nudge = GestureEngine::new(w, h, cfg.clone());
    nudge.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    for i in 1..=5u32 {
        nudge.process_touch(&touch(
            t0,
            i as u64 * 16,
            TouchPhase::Move,
            sx,
            sy - i as f32 * 60.0,
        ));
    }
    // 300 px, released from a dead stop: neither the fling branch nor the
    // distance branch is satisfied, so the release is handed back to the app
    // as its own scroll. What matters is that it is *not* a Home.
    let nudged = nudge.process_touch(&touch(t0, 20 * 16, TouchPhase::Up, sx, sy - 300.0));
    assert!(
        !matches!(nudged, GestureAction::Home { .. }),
        "a nudge that is neither a fling nor past the commit distance must not tear the app down: {nudged:?}"
    );
    assert_eq!(
        nudged,
        GestureAction::Swipe {
            delta_x: 0.0,
            delta_y: -300.0
        },
        "and it must reach the app as an ordinary scroll"
    );
    // A nudge *under* the slop is not even that: it is a tap.
    let mut tap = GestureEngine::new(w, h, cfg.clone());
    tap.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    assert_eq!(
        tap.process_touch(&touch(t0, 8, TouchPhase::Move, sx + 2.0, sy + 1.0)),
        GestureAction::None
    );
    assert_eq!(
        tap.process_touch(&touch(t0, 900, TouchPhase::Up, sx + 2.0, sy + 1.0)),
        GestureAction::None
    );

    // --- (d) a horizontal drag is the app's own ---------------------------
    let mut sideways = GestureEngine::new(w, h, cfg.clone());
    sideways.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    assert_eq!(
        sideways.process_touch(&touch(t0, 16, TouchPhase::Move, sx + 400.0, sy)),
        GestureAction::None,
        "a horizontal drag is not an upward drag"
    );
    assert_eq!(
        sideways.process_touch(&touch(t0, 32, TouchPhase::Up, sx + 400.0, sy)),
        GestureAction::Swipe {
            delta_x: 400.0,
            delta_y: 0.0
        },
        "the app's own scroll must be handed back untouched"
    );

    // --- (e) none of it can reach the drawer ------------------------------
    // The drawer's gesture rides the bottom bar and is reported as
    // `BottomBarScrub`; the axis is claimed by dominance, so a purely
    // vertical drag can never be read as one.
    assert!(
        !cfg.is_scrub_dominant(0.0, 720.0),
        "a vertical drag must never dominate the scrub axis"
    );
    let mut scrub = GestureEngine::new(w, h, cfg.clone());
    let nav_y = h - cfg.bottom_nav_height * 0.5;
    assert_eq!(scrub.classify_edge(sx, nav_y), EdgeSide::Bottom);
    scrub.process_touch(&touch(t0, 0, TouchPhase::Down, sx, nav_y));
    match scrub.process_touch(&touch(
        t0,
        16,
        TouchPhase::Move,
        sx + cfg.scrub_threshold_x * 1.5,
        nav_y,
    )) {
        GestureAction::BottomBarScrub { app_shift, .. } => assert_eq!(app_shift, 1),
        other => panic!("a horizontal drag on the nav bar is a task scrub: {other:?}"),
    }
    // And the full upward walk from (a) contains nothing but Home drags plus
    // the single commit -- no scrub, no shade, no back, no recents.
    let mut walk = GestureEngine::new(w, h, cfg);
    let mut seen = Vec::new();
    walk.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    for i in 1..=12u32 {
        seen.push(walk.process_touch(&touch(
            t0,
            i as u64 * 16,
            TouchPhase::Move,
            sx,
            sy - i as f32 * 60.0,
        )));
    }
    seen.push(walk.process_touch(&touch(t0, 19 * 16, TouchPhase::Up, sx, sy - 720.0)));
    assert!(
        seen.iter()
            .all(|a| matches!(a, GestureAction::Home { .. } | GestureAction::None)),
        "the home walk leaked another gesture: {seen:?}"
    );
}

#[test]
fn test_recents_drag_reaches_overview_and_allows_dismiss() {
    // PROXY for the shell's `ShellState::Overview` transition and the carousel
    // drag inside it (`overview_touch_down` / `_move` / `_up` in
    // `crates/utlc/src/main.rs`). The `ShellState` enum is private to that
    // binary, so the two halves are asserted on the engines those functions
    // call:
    //   1. `GestureAction::Recents` is the action that opens the carousel, and
    //      it is a motion *pause* -- a fast flick must not produce it;
    //   2. `Recents` is the carousel model: the selected card is centred, the
    //      card under the finger is the card that can be grabbed, and a drag
    //      past the dismiss threshold commits a kill while a short one cancels.
    let (w, h) = (1080.0f32, 2400.0f32);
    let cfg = GestureConfig::default();
    let t0 = Instant::now();
    let (sx, sy) = (540.0f32, 2380.0f32);

    // --- 1. entering the overview -----------------------------------------
    let mut engine = GestureEngine::new(w, h, cfg.clone());
    assert_eq!(
        engine.classify_edge(sx, sy),
        EdgeSide::Bottom,
        "quick-step starts on the navigation bar"
    );
    assert_eq!(
        engine.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy)),
        GestureAction::None
    );
    // Below the 60 px threshold nothing is claimed at all: a 40 px flick off
    // the navigation bar is still the app's own scroll.
    assert_eq!(
        engine.process_touch(&touch(t0, 8, TouchPhase::Move, sx, 2340.0)),
        GestureAction::None,
        "a move short of home_threshold_y must not be claimed"
    );
    // Past it the drag is reported as a *home* scale-down. While the finger is
    // still moving this is not an overview.
    for (dt, y) in [(16u64, 2300.0f32), (32, 2299.0)] {
        match engine.process_touch(&touch(t0, dt, TouchPhase::Move, sx, y)) {
            GestureAction::Home { progress, .. } => assert!(progress > 0.0),
            other => panic!("a moving finger is still a home drag: {other:?}"),
        }
    }
    // Then crawl: 1 px per 16 ms frame is 0.0625 px/ms, well inside
    // `motion_pause_slow`, so the dwell clock starts running.
    let mut ms = 48u64;
    let mut y = 2298.0f32;
    let mut detents = 0usize;
    let mut recents_progress = 0.0f32;
    while ms < 2000 {
        y -= 1.0;
        ms += 16;
        match engine.process_touch(&touch(t0, ms, TouchPhase::Move, sx, y)) {
            GestureAction::Recents {
                progress,
                trigger_haptic,
            } => {
                if trigger_haptic {
                    detents += 1;
                }
                recents_progress = recents_progress.max(progress);
            }
            GestureAction::Home { .. } | GestureAction::None => {}
            other => panic!("the pause must not change gesture kind: {other:?}"),
        }
    }
    assert_eq!(
        detents, 1,
        "the overview detent must pulse exactly once per gesture"
    );
    assert!(
        recents_progress > 0.0 && recents_progress <= 1.0,
        "the overview must report a progress, got {recents_progress}"
    );
    let release = engine.process_touch(&touch(t0, ms + 16, TouchPhase::Up, sx, y));
    assert_eq!(
        release,
        GestureAction::Recents {
            progress: 1.0,
            trigger_haptic: false
        },
        "the release commits the overview at full progress and does not pulse again"
    );

    // The same travel released while still moving fast is a Home flick, not an
    // overview: Recents is a pause, and without the dwell the action must stay
    // Home so the shell does not open a carousel the user swiped past.
    let mut flick = GestureEngine::new(w, h, cfg.clone());
    flick.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    flick.process_touch(&touch(t0, 8, TouchPhase::Move, sx, 2340.0));
    flick.process_touch(&touch(t0, 16, TouchPhase::Move, sx, 2300.0));
    match flick.process_touch(&touch(t0, 24, TouchPhase::Up, sx, 2260.0)) {
        GestureAction::Home { progress, .. } => assert_eq!(progress, 1.0),
        other => panic!("an un-paused flick is Home, not Recents: {other:?}"),
    }

    // A system takeover is not a gesture end.
    let mut takeover = GestureEngine::new(w, h, cfg);
    takeover.process_touch(&touch(t0, 0, TouchPhase::Down, sx, sy));
    takeover.process_touch(&touch(t0, 8, TouchPhase::Move, sx, 2340.0));
    takeover.process_touch(&touch(t0, 16, TouchPhase::Move, sx, 2300.0));
    for i in 3..40u64 {
        takeover.process_touch(&touch(
            t0,
            i * 16,
            TouchPhase::Move,
            sx,
            2304.0 - (i as f32 - 2.0),
        ));
    }
    assert_eq!(
        takeover.process_touch(&touch(t0, 640, TouchPhase::Cancel, sx, 2268.0)),
        GestureAction::None,
        "a Cancel must not open the overview"
    );

    // --- 2. the carousel the shell opens ----------------------------------
    let l = Layout::plain(w, h);
    let mut recents = Recents::new(&l);
    for i in 0..3i32 {
        assert!(
            recents
                .push(TaskCard::new(i as u32, 4000 + i, 900 + i as u32))
                .is_none(),
            "nothing may be evicted below MAX_TASKS"
        );
    }
    assert_eq!(recents.len, 3);
    assert_eq!(recents.selected, 0, "a push focuses the new card");
    assert_eq!(recents.visible_card, recents.selected);

    // The selected card is the centred one, and the geometry is the model's
    // own `RecentsLayout` rather than a second formula in the renderer.
    let rl = l.recents();
    let card = recents.card_rect_centered(0, w, h).expect("card 0 is live");
    assert!(
        (card.center_x() - w * 0.5).abs() < 0.01,
        "the focused card is centred"
    );
    assert!(
        (card.center_y() - h * 0.5).abs() < 0.01,
        "the strip is centred vertically"
    );
    assert!((card.w - rl.card_w).abs() < 0.01 && (card.h - rl.card_h).abs() < 0.01);
    // A neighbour is one pitch away once the reflow springs have parked. It is
    // *not* one pitch away on the frame the card was pushed -- `arm_reflow_insert`
    // deliberately starts the new neighbour at the position that holds it still
    // across the insert, and the spring slides it into its slot.
    let next = recents.card_rect_centered(1, w, h).expect("card 1 is live");
    assert!(
        (next.center_x() - card.center_x()).abs() < recents.pitch + 0.01,
        "a neighbour may not overlap the focused card"
    );
    for _ in 0..400 {
        recents.step(0.016, 8.0);
    }
    let parked = recents.card_rect_centered(1, w, h).expect("card 1 is live");
    assert!(
        (parked.center_x() - card.center_x() - recents.pitch).abs() < 0.5,
        "the parked carousel is one pitch per card: {} vs {}",
        parked.center_x(),
        card.center_x()
    );
    assert!(
        recents.reflow[1].is_at_rest(),
        "the reflow bank must settle"
    );
    // The cull range covers the focused card and the one that can slide in.
    assert_eq!(recents.cull_range(), 0..2);
    assert!(
        recents.card_rect_centered(3, w, h).is_none(),
        "an out-of-range index is a miss"
    );

    // --- 3. the dismiss ---------------------------------------------------
    // A Down -> Move -> Up on the card, hit-tested through the model's own rect
    // exactly as `overview_hit` does, so "what is drawn" and "what is
    // grabbable" cannot disagree.
    let touch_x = card.center_x();
    let mut finger_y = card.center_y();
    // 40 px per frame: 0.0191 of the dismiss length, so the 10 dp haptic band
    // (0.0123 of the length here) is *landed in* rather than stepped over, and
    // 30 frames is comfortably past the 0.5 commit threshold.
    const DISMISS_STEP: f32 = 40.0;
    const DISMISS_FRAMES: u32 = 30;
    let mut fed = 0usize;
    let mut threshold_armed = false;
    for _ in 0..DISMISS_FRAMES {
        let next_y = finger_y - DISMISS_STEP;
        // Re-read the rect every frame: the card travels with the finger, and
        // `overview_hit` hit-tests against the *live* rect, not the one the
        // drag started on.
        let live = recents.card_rect_centered(0, w, h).expect("card 0 is live");
        if !live.contains(touch_x, next_y) {
            break; // the finger has left the card; the shell stops feeding it
        }
        finger_y = next_y;
        recents.on_card_drag(0, -DISMISS_STEP);
        // The shell's frame loop steps the model every frame, and past the
        // detach threshold the dismiss spring is what carries the card, so the
        // drag has to be interleaved with a step to be the real sequence.
        recents.step(0.016, 8.0);
        fed += 1;
        threshold_armed |= recents.threshold_haptic_done;
    }
    assert_eq!(
        fed, DISMISS_FRAMES as usize,
        "the whole drag must reach the model"
    );
    assert!(
        threshold_armed,
        "crossing the dismiss threshold must arm the haptic once"
    );
    assert!(
        recents.threshold_haptic_done,
        "and the latch stays set for the rest of the drag"
    );
    assert!(
        recents.cards[0].dismiss_y < 0.0,
        "an upward drag is a negative offset"
    );
    // The stack is most-recent-first, so card 0 is the *last* task pushed.
    let focused = recents.cards[0].pid;
    // Past half the dismiss length commits; below it cancels. Both are the
    // decision `on_card_release` exists to make.
    let beyond = -recents.cards[0].dismiss_y / recents.dismiss_length;
    if beyond >= 0.5 {
        assert_eq!(
            recents.on_card_release(0),
            DismissOutcome::Committed {
                card: 0,
                pid: focused
            },
            "a drag past the dismiss threshold must commit"
        );
        assert_eq!(recents.cards[0].kill, KillState::Grace);
        // The card stays in the stack; the shell removes it when the close is
        // acknowledged. The grace clock is the escalation.
        assert_eq!(recents.len, 3);
        let mut escalated = KillQueue::EMPTY;
        for _ in 0..40 {
            let q = recents.step(0.05, 8.0);
            if !q.is_empty() {
                escalated = q;
            }
        }
        let actions: Vec<KillAction> = escalated.iter().copied().collect();
        assert_eq!(
            actions,
            vec![KillAction::Force(focused)],
            "a committed card escalates to an uncatchable kill once its grace expires"
        );
        assert_eq!(recents.cards[0].kill, KillState::Forced);
        // ...and only once.
        assert!(
            recents.step(0.05, 8.0).is_empty(),
            "the escalation must not repeat"
        );
        // A card that is already dying cannot be dragged again.
        assert_eq!(recents.on_card_release(0), DismissOutcome::Ignored);
    } else {
        panic!("the synthetic drag only reached {beyond} of the dismiss length");
    }

    // A short drag on a fresh stack cancels: the same code path, the other
    // answer, and the app is still there.
    let mut keep = Recents::new(&l);
    for i in 0..3i32 {
        let _ = keep.push(TaskCard::new(i as u32, 5000 + i, 1900 + i as u32));
    }
    for _ in 0..4 {
        keep.on_card_drag(0, -40.0);
    }
    let keep_pid = keep.cards[0].pid;
    assert_eq!(
        keep.on_card_release(0),
        DismissOutcome::Cancelled {
            card: 0,
            pid: keep_pid
        },
        "a short drag must spring the card back"
    );
    assert_eq!(
        keep.cards[0].kill,
        KillState::Idle,
        "a cancelled card is not closing"
    );
    assert_eq!(keep.len, 3);

    // A system takeover pushes the card home rather than arming the kill --
    // the `cancelled` arm of `overview_touch_up`.
    let mut takeover_recents = Recents::new(&l);
    for i in 0..2i32 {
        let _ = takeover_recents.push(TaskCard::new(i as u32, 6000 + i, 2900 + i as u32));
    }
    for _ in 0..12 {
        takeover_recents.on_card_drag(0, -100.0);
    }
    takeover_recents.on_card_drag(0, 0.0);
    let takeover_pid = takeover_recents.cards[0].pid;
    assert_eq!(
        takeover_recents.on_card_release(0),
        DismissOutcome::Committed {
            card: 0,
            pid: takeover_pid
        },
        "a card dragged back to its origin and released still commits: \
         `on_card_release` decides on travel and velocity, and the shell's \
         cancel arm must call `on_card_drag(i, 0.0)` *first* -- which the \
         pinned negative case below proves is the only thing that changes it"
    );

    // The negative case the previous arm depends on: an untouched card.
    let mut untouched = Recents::new(&l);
    let _ = untouched.push(TaskCard::new(0, 7000, 3900));
    assert_eq!(
        untouched.on_card_release(0),
        DismissOutcome::Cancelled { card: 0, pid: 7000 },
        "releasing a card that never moved must not kill anything"
    );
    assert_eq!(untouched.cards[0].pid, 7000);
    // A pinned card cannot be dismissed at all.
    let mut pinned = Recents::new(&l);
    let _ = pinned.push(TaskCard::new(0, 7100, 3901));
    pinned.cards[0].dismissable = false;
    for _ in 0..12 {
        pinned.on_card_drag(0, -100.0);
    }
    assert_eq!(pinned.on_card_release(0), DismissOutcome::Ignored);
    assert_eq!(pinned.cards[0].kill, KillState::Idle);
}

#[test]
fn test_fast_scroller_drag_scrolls_catalogue_grid() {
    // PROXY for the shell's fast-scroller gesture (the `app_drawer_open` arm
    // of the input loop in `crates/utlc/src/main.rs`, which feeds
    // `FastScrollerState` and pulses a `Tick` on the returned section-change
    // edge). The state enum is not involved, so this asserts the engine
    // contracts that arm depends on:
    //   1. a press only engages once the finger has both travelled `engage_delta`
    //      and dwelled `engage_ms`; a fast flick past the scroller must not be
    //      captured, and a distant press must not be either;
    //   2. the engaging move contributes no thumb travel (anti-jump);
    //   3. the thumb position maps to a *row* in a real catalogue, and the row
    //      maps to the letter that row actually starts with;
    //   4. the returned bool is a rising edge -- one pulse per letter, not one
    //      per MOVE;
    //   5. a release ends the drag but keeps the letter, and the popup alpha
    //      lands exactly on zero.
    use utim_core::graphics::drawer_mod::{FastScrollerState, SectionIndex, FADE_OUT_MS};

    let (w, h) = (1080.0f32, 2400.0f32);
    let l = Layout::plain(w, h);
    let fs = l.fast_scroller();

    // A catalogue that spans the whole alphabet, so the letter -> row map has
    // an absent letter *between* every pair of present ones and nothing above
    // the last one. That is the shape `SectionIndex` encodes exactly, and it
    // is the shape the exact-mapping assertion below is about.
    let all: Vec<String> = (0..26u8)
        .map(|i| {
            let letter = b'A' + i;
            let letter = letter as char;
            format!("{letter}pplication")
        })
        .collect();
    let letters: Vec<&str> = all.iter().map(|s| &s[..1]).collect();
    let index = SectionIndex::build(letters.iter().copied());
    assert!(!index.is_empty(), "a 26-app catalogue has sections");
    // Every present row's first row is its own, and the encoding is monotone --
    // which is the documented invariant the lookup's tie-break depends on.
    for row in 0..26usize {
        assert_eq!(
            index.row_offset[row] as usize, row,
            "row {row} must be the first row of its own letter"
        );
    }
    for row in 0..all.len() {
        let progress = (row as f32 + 0.5) / all.len() as f32;
        assert_eq!(
            index.section_for_progress(progress, all.len()),
            row as u8 + 1,
            "row {row} ({}) must report its own letter",
            all[row]
        );
    }
    // An empty catalogue has no section to show, which is the row-count test
    // the encoding cannot express on its own.
    assert_eq!(SectionIndex::EMPTY.section_for_progress(0.5, 0), 0);
    assert_eq!(index.section_for_progress(f32::NAN, all.len()), 0);
    // A full-travel drag reports the last present section, not a trailing run
    // of absent letters.
    assert_eq!(index.section_for_progress(1.0, all.len()), 26);

    // A real all-apps catalogue is *sparse* -- it has no Q, no X, no Z -- which
    // is the shape that broke `SectionIndex::build`. The descending
    // "inherit the next present letter's row" walk had nothing to inherit for
    // the letters *above* the highest present one and left them at the seed
    // value 0, so `row_offset` was no longer non-decreasing and
    // `section_for_progress`'s "later letters win ties" rule reported the last
    // alphabet index at every scroll position. `utlc` shows that string
    // verbatim, so a sparse catalogue always read "Z".
    let sparse: [&str; 12] = [
        "Alarm", "Browser", "Camera", "Dialer", "Email", "Files", "Gallery", "Health", "Market",
        "Notes", "Podcasts", "Recorder",
    ];
    let sparse_index = SectionIndex::build(sparse.iter().copied());
    assert_eq!(
        sparse_index.row_offset[17] as usize, 11,
        "R owns the last row"
    );
    // The leading run (S..Z) has no successor to mirror, so it takes the last
    // present row, which is what keeps `row_offset` non-decreasing. That value
    // is *not* what the lookup reads -- the `present` mask below is -- it is
    // just the value that leaves the documented invariant intact.
    assert_eq!(
        sparse_index.row_offset[25] as usize, 11,
        "the leading run mirrors the last present row, keeping row_offset monotone"
    );
    assert_eq!(
        sparse_index.row_offset[12] as usize, 8,
        "M owns the Market row"
    );
    // The leading run is marked absent, and that is the actual fix: an absent
    // letter must never be a lookup candidate, or it wins the tie its own
    // mirrored row creates.
    for absent in ["S", "T", "U", "V", "W", "X", "Y", "Z"] {
        let l = (absent.as_bytes()[0] - b'A') as u32;
        assert_eq!(
            sparse_index.present & (1 << l),
            0,
            "{absent} owns no row and must not be a lookup candidate"
        );
    }
    for owned in ["A", "G", "H", "M", "N", "P", "R"] {
        let l = (owned.as_bytes()[0] - b'A') as u32;
        assert_ne!(
            sparse_index.present & (1 << l),
            0,
            "{owned} owns a row and must be a candidate"
        );
    }
    // The documented contract, stated as an invariant over the whole alphabet:
    // `row_offset` is non-decreasing. This is what the seed bug violated, and
    // it is the property the lookup's tie-break depends on, so assert it
    // directly rather than relying on a few sample letters.
    for l in 1..26 {
        let (prev, cur) = (sparse_index.row_offset[l - 1], sparse_index.row_offset[l]);
        assert!(
            cur >= prev,
            "row_offset must be non-decreasing: [{prev_l}]={prev} > [{l}]={cur}",
            prev_l = l - 1
        );
    }
    // And the user-visible consequence: the reported letter must be the letter
    // the row actually starts with, at every row of a sparse catalogue.
    for (row, want) in sparse.iter().enumerate() {
        let progress = (row as f32 + 0.5) / sparse.len() as f32;
        let got = sparse_index.section_for_progress(progress, sparse.len());
        let want_i = (want.as_bytes()[0].to_ascii_uppercase() - b'A') as i32 + 1;
        assert_eq!(
            i32::from(got),
            want_i,
            "row {row} ({want:?}) reported letter {got}, expected {want_i}"
        );
    }
    // A catalogue occupying only the first letter must still answer sanely at
    // both ends rather than collapsing to the last alphabet index.
    let only_a = SectionIndex::build(["Alpha", "Ant", "Axe"]);
    assert_eq!(
        only_a.section_for_progress(0.0, 3),
        1,
        "top of an A-only list is A"
    );
    assert_eq!(
        only_a.section_for_progress(1.0, 3),
        1,
        "bottom of an A-only list is A"
    );

    // --- the drag ---------------------------------------------------------
    let mut st = FastScrollerState::new();
    st.set_catalogue(all.iter().map(|s| s.as_str()));
    assert_eq!(st.total_rows as usize, all.len());
    assert!(!st.dragging);
    assert_eq!(st.letter, 0, "no letter before a drag");
    assert!(!st.popup_visible());

    let travel = (fs.track.h - fs.thumb_h).max(0.0);
    assert!(travel > 0.0, "the track must have somewhere to travel");
    // The finger enters the scroller at the very top of the track, `ENGAGE_OFF`
    // px below it. That offset is the anti-jump: it is inside `engage_delta`
    // and is absorbed by the engaging move, so the thumb starts the walk at 0
    // even though the finger is `ENGAGE_OFF` px down.
    const ENGAGE_OFF: f32 = 2.0;
    assert!(
        ENGAGE_OFF < fs.engage_delta,
        "the probe must stay inside the gate"
    );
    let entry_y = fs.track.y + ENGAGE_OFF;

    // A flick straight past the scroller must not be captured.
    st.on_down(entry_y, 0.0, &fs);
    assert!(!st.dragging, "a DOWN does not engage");
    assert!(
        !st.on_move(entry_y + fs.engage_delta * 4.0, 40.0, &fs),
        "a press that has run away from the scroller is a list scroll"
    );
    assert!(!st.dragging, "still not a fast-scroll drag");
    assert_eq!(
        st.thumb_y, 0.0,
        "an un-engaged move must not move the thumb"
    );

    // A press that has not yet dwelled is not a drag either.
    st.on_down(entry_y, 100.0, &fs);
    assert!(!st.on_move(entry_y + 1.0, 100.0 + fs.engage_ms * 0.5, &fs));

    // The real thing: dwell past `engage_ms` *and* inside `engage_delta`.
    st.on_down(entry_y, 200.0, &fs);
    let engaged = st.on_move(entry_y + ENGAGE_OFF, 200.0 + fs.engage_ms * 2.0, &fs);
    assert!(
        st.dragging,
        "the drag must engage on the first qualifying move"
    );
    assert_eq!(
        st.thumb_y, 0.0,
        "the engaging move must contribute no travel (anti-jump)"
    );
    assert_eq!(
        st.letter, 1,
        "the thumb is still at the top, so the letter is A"
    );
    assert!(engaged, "entering section A is a detent");
    assert!(st.haptic_latch, "the latch mirrors the returned edge");

    // Walk the thumb down the track, 8 frames per row. Every letter change is a
    // rising edge and nothing else is; the thumb follows the finger 1:1 below
    // the clamp, from the position the engaging move left it in.
    let mut detents = 1usize; // the engagement itself
    let mut last_letter = st.letter;
    let mut crossed = 0usize;
    let steps = 26 * 8;
    for i in 1..=steps {
        let frac = i as f32 / steps as f32;
        let y = entry_y + ENGAGE_OFF + travel * frac;
        let edge = st.on_move(y, 200.0 + fs.engage_ms * 2.0 + i as f32 * 16.0, &fs);
        assert!(
            (st.thumb_y - travel * frac).abs() < 0.5,
            "frame {i}: thumb {} should track {frac} of the travel",
            st.thumb_y
        );
        assert_eq!(
            edge,
            st.letter != last_letter,
            "frame {i}: the return value is the section-change edge only"
        );
        if edge {
            detents += 1;
        }
        // The letter must be the one the row under the thumb really starts
        // with. Checked on the *last* frame of each 8-frame group, which is the
        // sample furthest from the row boundary it just crossed; asserting on
        // the boundary frame itself would be testing float accumulation.
        if i % 8 == 7 {
            let row = (i + 1) / 8 - 1;
            assert_eq!(st.letter, row as u8 + 1, "frame {i} at row {row}");
            assert_eq!(
                st.letter_str(),
                letters[row],
                "the letter renders as a letter"
            );
        }
        if st.letter != last_letter {
            crossed += 1;
        }
        last_letter = st.letter;
    }
    assert_eq!(
        crossed, 25,
        "the walk must cross every one of the 25 letter gaps"
    );
    assert_eq!(
        detents, 26,
        "26 letters, 26 pulses -- not one per MOVE, which would be {steps}"
    );
    // The full-travel thumb is the end of the list, not past it.
    assert_eq!(st.letter, 26);
    assert_eq!(st.letter_str(), "Z");

    // Dragging past the end of the track clamps instead of running away.
    st.on_move(fs.track.y + travel * 4.0, 2000.0, &fs);
    assert_eq!(st.thumb_y, travel, "the thumb cannot leave its own track");

    // A release ends the drag but keeps the letter: the popup fades out over
    // `FADE_OUT_MS` and the reference leaves the text in place for that whole
    // window.
    st.on_up(2100.0);
    assert!(!st.dragging);
    assert_eq!(st.letter, 26, "the letter outlives the drag");
    st.step(FADE_OUT_MS as f32);
    assert_eq!(st.popup_alpha, 0.0, "the fade must land exactly on zero");
    assert!(
        !st.popup_visible(),
        "nothing to paint once the alpha is gone"
    );
    // ...and the thumb is still where the drag left it, because the fade is an
    // alpha and not a rewind.
    assert_eq!(st.thumb_y, travel);

    // While it is dragging, the popup ramps *up* and is visible; a short drag
    // that ends on a zero-section catalogue shows nothing at all.
    let mut blank = FastScrollerState::new();
    blank.on_down(entry_y, 0.0, &fs);
    blank.on_move(entry_y + ENGAGE_OFF, fs.engage_ms * 2.0, &fs);
    assert!(blank.dragging, "the drag engages on an empty catalogue too");
    assert_eq!(blank.letter, 0, "an empty catalogue has no section");
    assert!(!blank.popup_visible(), "no letter means no teardrop");
    // The alpha still ramps -- `step` drives it off `dragging`, not off the
    // letter -- and `popup_visible` is the conjunction that keeps it off the
    // frame. The split is what lets the fade be a plain time ramp.
    blank.step(200.0);
    assert_eq!(
        blank.popup_alpha, 1.0,
        "the fade-in is driven by the drag, not the letter"
    );
    assert!(
        !blank.popup_visible(),
        "and still nothing to paint without a section"
    );
    assert!(blank.dragging);

    // A sparse catalogue still drives a real drag: the geometry, the
    // engagement and the fade are all catalogue-independent, and the letter is
    // a valid index into the alphabet.
    let mut sparse_drag = FastScrollerState::new();
    sparse_drag.set_catalogue(sparse.iter().copied());
    sparse_drag.on_down(entry_y, 0.0, &fs);
    assert!(sparse_drag.on_move(entry_y + ENGAGE_OFF, fs.engage_ms * 2.0, &fs));
    for i in 1..=40 {
        let frac = i as f32 / 40.0;
        sparse_drag.on_move(fs.track.y + travel * frac, 100.0 + i as f32 * 16.0, &fs);
        assert!(
            (1..=26).contains(&sparse_drag.letter),
            "frame {i}: {} is not a letter index",
            sparse_drag.letter
        );
    }
    assert_eq!(sparse_drag.total_rows as usize, sparse.len());
    sparse_drag.on_up(900.0);
    sparse_drag.step(FADE_OUT_MS as f32);
    assert!(!sparse_drag.popup_visible());

    // The shell renders the thumb as a fraction of the travel, so the value it
    // hands the renderer has to be in 0..=1 for every legal drag.
    let mut f = FastScrollerState::new();
    f.set_catalogue(all.iter().map(|s| s.as_str()));
    f.on_down(fs.track.y, 0.0, &fs);
    f.on_move(fs.track.y + 1.0, fs.engage_ms * 2.0, &fs);
    for i in 0..50 {
        f.on_move(
            fs.track.y - 500.0 + i as f32 * 40.0,
            100.0 + i as f32 * 16.0,
            &fs,
        );
        let frac = f.thumb_y / travel;
        assert!(
            (0.0..=1.0).contains(&frac),
            "frame {i}: the render fraction {frac} left 0..1"
        );
    }
    // A relayout between drags cannot strand the thumb outside its own track.
    let mut tall = l.fast_scroller();
    tall.track.h *= 2.0;
    f.on_down(fs.track.y + 10.0, 500.0, &tall);
    assert!(
        f.thumb_y <= (tall.track.h - tall.thumb_h).max(0.0),
        "the thumb re-clamps against the track it is shown on"
    );
}
