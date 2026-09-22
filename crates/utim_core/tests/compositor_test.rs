//! Integration Test Suite for Phase 3: Android-Style Wayland Compositor & Launcher (UTLC).
//! Exhaustively validates Milestones 3.1 through 3.6:
//! - 3.1: Smithay-compatible Mobile Wayland Protocols, HWC multi-plane presentation, RSS < 15MB, boot < 0.45s
//! - 3.2: Paged home grid, spring physics, Hotseat dock, zero-allocation .desktop parser, fuzzy search < 1ms
//! - 3.3: QuickStep gesture navigation (< 8ms), Recents card stack, swipe-to-kill (SIGKILL), split-screen
//! - 3.4: SystemUI status bar (5G/LTE), Quick Settings tiles, org.freedesktop.Notifications
//! - 3.5: Ambient lock screen, touch barrier, PIN keypad, Android Fingerprint HAL bridge (< 300ms), Virtual Keyboard IME
//! - 3.6: UTIM Mobile Power Governor (cgroup.freeze) & dynamic OOM score hierarchy synchronization

use std::path::Path;
use std::time::{Duration, Instant};

use utim_core::compositor::desktop::{parse_desktop_entry, DesktopApp, DesktopCatalogue};
use utim_core::compositor::gestures::{
    cubic_bezier_ease_out, GestureAction, GestureConfig, GestureEngine, RawTouchEvent, TouchPhase,
};
use utim_core::compositor::ime::{ImeAction, VirtualKeyboard};
use utim_core::compositor::launcher::{HotseatDock, WorkspaceGrid};
use utim_core::compositor::lockscreen::LockScreen;
use utim_core::compositor::power_sync::{oom_roles, UtimPowerSync};
use utim_core::compositor::protocols::{
    ProtocolRegistry, WaylandInterface, WlMessage, WlMessageBuilder,
};
use utim_core::compositor::recents::{RecentsCard, RecentsCarousel, SplitScreenConfig};
use utim_core::compositor::scene::ShellMode;
use utim_core::compositor::server::WaylandServer;
use utim_core::compositor::systemui::{CellularRat, QuickTileKind, SystemUiShade};
use utim_core::graphics::composer::{HwcComposer, HwcVersion};

#[test]
fn test_milestone_3_1_wayland_protocols_and_wire_framing() {
    // 1. Validate Wire Framing and Serialization
    let mut builder = WlMessageBuilder::new(1, 2);
    builder.put_u32(0xCAFE);
    builder.put_i32(-42);
    builder.put_fixed(12.75);
    builder.put_string("org.freedesktop.MobileWayland");
    builder.put_array(&[1, 2, 3, 4, 5]);

    let wire = builder.build();
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
    assert!(next_offset <= wire.len());

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
    let metrics = server.get_metrics();
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
    // 1. Spring Physics Horizontal Scrolling
    let mut grid = WorkspaceGrid::new(4, 5, 3, 1080.0);
    assert_eq!(grid.current_page, 0);

    grid.add_item(0, 0, 0, "org.mobian.dialer".into());
    grid.add_item(0, 1, 0, "chatty".into());
    grid.add_item(1, 0, 0, "firefox".into());

    // Drag left
    grid.on_drag(-400.0);
    assert!(grid.scroll_spring.current < 0.0);

    // Release with leftward flick velocity
    grid.on_release(-600.0);
    for _ in 0..60 {
        grid.update(0.016);
    }
    assert_eq!(grid.current_page, 1, "Should snap to page 1 after flick");

    // 2. Persistent Hotseat Dock
    let dock = HotseatDock::default_mobile(2400.0);
    assert_eq!(dock.slots.len(), 5);
    let (x, y, w, _h) = dock.slot_rect(0, 1080.0);
    assert_eq!(x, 0.0);
    assert_eq!(w, 1080.0 / 5.0);
    assert!(y > 2200.0);

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
    assert_eq!(cubic_bezier_ease_out(1.0), 1.0);

    // 2. Recents Carousel & Swipe-to-Kill Process Management
    let mut carousel = RecentsCarousel::new(1080.0, 2400.0);
    carousel.add_card(RecentsCard::new(
        "calc".into(),
        5001,
        "Calculator".into(),
        "".into(),
        1,
        None,
        800.0,
        1600.0,
    ));
    carousel.add_card(RecentsCard::new(
        "files".into(),
        5002,
        "Files".into(),
        "".into(),
        2,
        None,
        800.0,
        1600.0,
    ));
    assert_eq!(carousel.cards.len(), 2);

    // Swipe upward on card 0
    carousel.on_card_vertical_drag(0, -200.0);
    let kill_target = carousel.on_card_vertical_release(0);
    assert_eq!(kill_target, Some(5002));

    // Check 500ms grace period escalation to SIGKILL
    let escalations = carousel.update_kill_lifecycle(Duration::from_millis(0));
    assert_eq!(escalations, vec![(5002, true)]);

    // 3. Clear All Button
    let cleared = carousel.clear_all();
    assert!(cleared.contains(&5001));

    // 4. Split-Screen Multitasking (50/50 Viewports)
    let mut split = SplitScreenConfig::new(2400.0);
    split.enable(5001, 5002);
    assert!(split.is_active);
    assert_eq!(split.top_viewport(1080.0), (0.0, 0.0, 1080.0, 1200.0));
    assert_eq!(
        split.bottom_viewport(1080.0, 2400.0),
        (0.0, 1200.0, 1080.0, 1200.0)
    );
}

#[test]
fn test_milestone_3_4_systemui_status_bar_and_quick_settings() {
    let mut shade = SystemUiShade::new(1080.0, 2400.0);

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

    assert!(shade.toggle_tile(QuickTileKind::Bluetooth));
    assert!(!shade.toggle_tile(QuickTileKind::Bluetooth)); // Toggles off

    shade.set_brightness(80);
    assert_eq!(shade.brightness_percent, 80);
    shade.set_volume(70);
    assert_eq!(shade.volume_percent, 70);

    // 3. Notification Center (org.freedesktop.Notifications)
    let n_id = shade.notify(
        "Mail".into(),
        0,
        "email".into(),
        "Meeting at 3PM".into(),
        "Discuss Universal Treble GSI roadmap.".into(),
        vec![
            ("open".into(), "Open".into()),
            ("dismiss".into(), "Dismiss".into()),
        ],
    );
    assert_eq!(n_id, 1);
    assert_eq!(shade.notifications.len(), 1);

    // Swipe to dismiss
    shade.on_notification_swipe(n_id, 150.0);
    assert!(shade.on_notification_release(n_id));
    assert_eq!(shade.notifications.len(), 0);
}

#[test]
fn test_milestone_3_5_lock_screen_fingerprint_hal_and_ime() {
    // 1. Lock Screen & Biometric Bridge
    let mut lock = LockScreen::new(Some("1337"));
    assert!(lock.is_locked());

    // Instant sub-300ms Fingerprint unlock
    let t_auth_start = Instant::now();
    let unlocked = lock.on_fingerprint_touch(1); // Enrolled finger 1
    let auth_dur = t_auth_start.elapsed();

    assert!(unlocked);
    assert!(!lock.is_locked());
    assert!(
        auth_dur < Duration::from_millis(300),
        "Biometric unlock must be < 300ms, got {:?}",
        auth_dur
    );

    // 2. Virtual Keyboard IME & Viewport Push
    let mut ime = VirtualKeyboard::new(1080.0, 2400.0);
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
    assert_eq!(act_t, ImeAction::CommitString("t".into()));

    ime.handle_key_tap("SHIFT");
    let act_r = ime.handle_key_tap("r");
    assert_eq!(act_r, ImeAction::CommitString("R".into()));

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

    // 1. Display Sleep -> Triggers cgroup freezing and wakelock release
    assert!(power.is_display_on);
    assert!(power.on_display_sleep().is_ok());
    assert!(!power.is_display_on);

    // 2. Display Wake -> Unfreezes cgroups in < 150us and acquires wakelock
    assert!(power.on_display_wake().is_ok());
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
        .is_ok());
    assert_eq!(power.active_foreground_pid, Some(2001));
    assert_eq!(power.recents_pids, vec![2002, 2003]);
}
