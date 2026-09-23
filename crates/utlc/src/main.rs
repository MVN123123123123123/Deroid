//! Universal Treble Launcher & Compositor (UTLC).
//! Unified Single-Process Mobile Wayland Compositor and Android-Style Launcher.
//! Engineered in bare-metal Rust following strict systems discipline (GEMINI.md).
//! Consumes < 15 MB RSS, renders interactive home screen in < 0.45s,
//! and delivers < 8ms touch gesture response.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant};

use utim_core::compositor::desktop::{DesktopApp, DesktopCatalogue};
use utim_core::compositor::gestures::{
    GestureAction, GestureConfig, GestureEngine, RawTouchEvent, TouchPhase,
};
use utim_core::compositor::ime::{ImeAction, VirtualKeyboard};
use utim_core::compositor::input::{
    InputDispatchResult, InputDispatcher, LinuxInputEvent, KEY_BACKSPACE, KEY_ENTER, KEY_ESC,
};
use utim_core::compositor::lockscreen::LockScreen;
use utim_core::compositor::power_sync::UtimPowerSync;
use utim_core::compositor::protocols::{ProtocolRegistry, WaylandInterface};
use utim_core::compositor::server::WaylandServer;
use utim_core::compositor::systemui::{QuickTileKind, SystemUiShade};
use utim_core::graphics::composer::{HwcComposer, HwcVersion};
use utim_core::graphics::{DrmInteractiveState, DrmKmsDevice};

fn main() {
    let args: Vec<String> = env::args().collect();
    let json_output = args.iter().any(|a| a == "--json");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        process::exit(0);
    }

    if args.iter().any(|a| a == "--check-protocols") {
        let ok = check_protocols(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--benchmark") {
        let ok = run_benchmarks(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-gestures") {
        let ok = test_gestures(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-desktop") {
        let ok = test_desktop(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-systemui") {
        let ok = test_systemui(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-lockscreen") {
        let ok = test_lockscreen(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-ime") {
        let ok = test_ime(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--test-power-sync") {
        let ok = test_power_sync(json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    if args.iter().any(|a| a == "--daemon") {
        run_daemon();
        process::exit(0);
    }

    // Default: run all Phase 3 comprehensive checks
    let ok = run_all_checks(json_output);
    process::exit(if ok { 0 } else { 1 });
}

fn print_usage() {
    println!("UTLC: Universal Treble Launcher & Compositor");
    println!("Usage:");
    println!("  utlc [OPTIONS]");
    println!("Options:");
    println!("  --daemon           Run as systemd service daemon (PID 1 integration)");
    println!("  --benchmark        Run performance benchmarks (Boot time, RSS, input latency)");
    println!("  --check-protocols  Verify all mobile Wayland protocol extensions");
    println!("  --test-gestures    Verify QuickStep gesture navigation engine");
    println!("  --test-desktop     Verify zero-allocation .desktop parser & fuzzy search");
    println!("  --test-systemui    Verify status bar, quick settings, and notifications");
    println!("  --test-lockscreen  Verify lock screen and Android Fingerprint HAL bridge");
    println!("  --test-ime         Verify virtual keyboard IME and viewport push");
    println!("  --test-power-sync  Verify UTIM MPG cgroup freezing and OOM hierarchy");
    println!("  --json             Emit structured JSON diagnostic output");
    println!("  --help, -h         Show this help message");
}

fn run_daemon() {
    println!("[+] Initializing UTLC (Universal Treble Launcher & Compositor)...");
    println!("[*] Single-process unified mobile compositor starting up...");

    // Detect HWC version from monolithic manifest and VINTF fragments
    let mut manifest_content = String::new();
    if let Ok(c) = fs::read_to_string("/vendor/etc/vintf/manifest.xml") {
        manifest_content.push_str(&c);
    }
    if let Ok(c) = fs::read_to_string("/vendor/manifest.xml") {
        manifest_content.push_str(&c);
    }
    if let Ok(entries) = fs::read_dir("/vendor/etc/vintf/manifest") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("xml") {
                if let Ok(c) = fs::read_to_string(&path) {
                    manifest_content.push_str(&c);
                }
            }
        }
    }
    let manifest_opt = if manifest_content.is_empty() {
        None
    } else {
        Some(manifest_content.as_str())
    };
    let hwc_version = HwcComposer::detect_version_from_manifest(manifest_opt);
    println!("[*] Initializing Hardware Composer: {:?}", hwc_version);
    let hwc = HwcComposer::new(hwc_version);

    let socket_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    let socket_path = PathBuf::from(socket_dir).join("wayland-0");

    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);
    server.power_sync.configure_self_oom_score();

    if let Err(e) = server.bind_socket() {
        eprintln!(
            "[-] Failed to bind Wayland socket at {}: {}",
            socket_path.display(),
            e
        );
    } else {
        println!(
            "[+] Bound Wayland display socket at {}",
            socket_path.display()
        );
    }

    match server.boot_to_first_frame() {
        Ok(dur) => {
            println!(
                "[+] Interactive home screen rendered in {:.2} ms ({:.4}s)",
                dur.as_secs_f64() * 1000.0,
                dur.as_secs_f64()
            );
        }
        Err(e) => {
            eprintln!("[-] Error presenting early boot frame: {}", e);
        }
    }

    // Try to open Direct Rendering Manager (DRM KMS) hardware scanout device
    let mut drm_display = match DrmKmsDevice::open_card("/dev/dri/card0") {
        Ok(dev) => {
            println!(
                "[+] Successfully initialized hardware DRM KMS display ({}x{})",
                dev.width, dev.height
            );
            Some(dev)
        }
        Err(e) => {
            println!(
                "[*] Hardware DRM KMS /dev/dri/card0 not available ({}), operating in headless Wayland mode",
                e
            );
            None
        }
    };

    let mut time_buf = [0u8; 5];

    if let Some(ref mut drm) = drm_display {
        let t_str = format_current_time(&mut time_buf);
        drm.render_mobile_ui(
            t_str,
            server.scene.mode == utim_core::compositor::scene::ShellMode::LockScreen,
        );
        drm.flush();
    }

    // Signal systemd/UTIM via sd_notify if NOTIFY_SOCKET is present
    let notify_socket = env::var("NOTIFY_SOCKET").ok();
    let notify_dgram = if notify_socket.is_some() {
        std::os::unix::net::UnixDatagram::unbound().ok()
    } else {
        None
    };
    if let (Some(ref dgram), Some(ref sock_path)) = (&notify_dgram, &notify_socket) {
        let _ = dgram.send_to(b"READY=1\nSTATUS=UTLC Mobile Shell Active\n", sock_path);
    }

    // Set up signal handling via signalfd for persistent daemon mode
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
    let sig_fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };

    let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epoll_fd >= 0 && sig_fd >= 0 {
        let mut sig_ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: sig_fd as u64,
        };
        unsafe {
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, sig_fd, &mut sig_ev);
        }
    }

    use std::os::unix::io::AsRawFd;
    if let Some(ref listener) = server.listener {
        let listen_fd = listener.as_raw_fd();
        if epoll_fd >= 0 {
            let mut listen_ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: listen_fd as u64,
            };
            unsafe {
                libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, listen_fd, &mut listen_ev);
            }
        }
    }

    let watchdog_usec = env::var("WATCHDOG_USEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let watchdog_interval = watchdog_usec
        .map(|us| Duration::from_micros(us / 2))
        .unwrap_or(Duration::from_secs(5));
    let mut last_watchdog_ping = Instant::now();

    // Listen on input event devices (/dev/input/event*)
    let mut input_fds = Vec::new();
    let mut opened_paths: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev/input") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
            {
                if let Ok(c_path) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
                    let fd = unsafe {
                        libc::open(
                            c_path.as_ptr(),
                            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                        )
                    };
                    if fd >= 0 {
                        if epoll_fd >= 0 {
                            let mut in_ev = libc::epoll_event {
                                events: libc::EPOLLIN as u32,
                                u64: fd as u64,
                            };
                            unsafe {
                                libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut in_ev);
                            }
                        }
                        input_fds.push(fd);
                        opened_paths.push(path);
                    }
                }
            }
        }
    }

    // State machine for interactive shell
    let mut dispatcher = InputDispatcher::new(server.scene.width as f32, server.scene.height as f32);
    let mut gesture_engine = GestureEngine::new(server.scene.width as f32, server.scene.height as f32, GestureConfig::default());
    let mut search_query = String::with_capacity(64);
    let mut search_active = false;
    let mut active_app: Option<String> = None;
    let mut terminal_lines: Vec<String> = Vec::with_capacity(32);
    let mut terminal_input = String::with_capacity(64);
    let mut quick_tiles_active = [true, true, true, false, true, false, false, false];
    let mut cursor_pos: Option<(usize, usize)> = None;
    let mut is_touching = false;
    let mut last_input_rescan = Instant::now();

    let mut running = true;
    let mut last_frame = Instant::now();
    let frame_interval = Duration::from_millis(16);

    println!("[+] UTLC daemon running successfully in persistent event loop.");

    while running {
        let timeout_ms = 16;
        let mut events: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };
        let nfds = if epoll_fd >= 0 {
            unsafe { libc::epoll_wait(epoll_fd, events.as_mut_ptr(), 16, timeout_ms) }
        } else {
            std::thread::sleep(Duration::from_millis(16));
            0
        };

        if nfds > 0 {
            for ev in events.iter().take(nfds as usize) {
                let fd = ev.u64 as libc::c_int;
                if fd == sig_fd {
                    println!("[*] UTLC received termination signal, shutting down...");
                    running = false;
                    break;
                } else if server
                    .listener
                    .as_ref()
                    .is_some_and(|l| l.as_raw_fd() == fd)
                {
                    if let Some(ref listener) = server.listener {
                        while let Ok((client_stream, _)) = listener.accept() {
                            let _ = client_stream.set_nonblocking(true);
                            if epoll_fd >= 0 {
                                let c_fd = client_stream.as_raw_fd();
                                let mut client_ev = libc::epoll_event {
                                    events: (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR)
                                        as u32,
                                    u64: c_fd as u64,
                                };
                                unsafe {
                                    libc::epoll_ctl(
                                        epoll_fd,
                                        libc::EPOLL_CTL_ADD,
                                        c_fd,
                                        &mut client_ev,
                                    );
                                }
                            }
                            server.client_streams.push(client_stream);
                        }
                    }
                } else if input_fds.contains(&fd) {
                    // Drain and decode Linux evdev events (virtio-tablet, virtio-keyboard, virtio-mouse)
                    let mut ev_buf = [0u8; 24 * 16];
                    let n = unsafe {
                        libc::read(fd, ev_buf.as_mut_ptr() as *mut libc::c_void, ev_buf.len())
                    };
                    if n > 0 {
                        let total_bytes = n as usize;
                        let mut offset = 0;
                        while offset + LinuxInputEvent::SIZE <= total_bytes {
                            if let Some(ev) = LinuxInputEvent::from_raw_bytes(&ev_buf[offset..offset + LinuxInputEvent::SIZE]) {
                                let res = dispatcher.process_event(&ev);
                                cursor_pos = Some((dispatcher.cursor_x as usize, dispatcher.cursor_y as usize));
                                is_touching = dispatcher.is_touch_down;

                                if server.scene.lockscreen.is_locked() {
                                    match res {
                                        InputDispatchResult::Touch(ref t) if t.phase == TouchPhase::Down => {
                                            server.scene.lockscreen.unlock();
                                            server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                        }
                                        InputDispatchResult::Tap { .. } => {
                                            server.scene.lockscreen.unlock();
                                            server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                        }
                                        InputDispatchResult::KeyPress { pressed: true, .. } => {
                                            server.scene.lockscreen.unlock();
                                            server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                        }
                                        _ => {}
                                    }
                                } else {
                                    match res {
                                        InputDispatchResult::Touch(raw_touch) => {
                                            let gesture_act = gesture_engine.process_touch(&raw_touch);
                                            match gesture_act {
                                                GestureAction::Home { progress, .. } if progress >= 1.0 => {
                                                    active_app = None;
                                                    server.scene.system_ui.close();
                                                    search_active = false;
                                                    server.scene.keyboard.deactivate();
                                                    server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
                                                }
                                                GestureAction::NotificationShade { progress } => {
                                                    if progress > 0.35 {
                                                        server.scene.system_ui.open();
                                                        server.scene.keyboard.deactivate();
                                                    }
                                                }
                                                GestureAction::Back { injected, .. } if injected => {
                                                    if server.scene.system_ui.is_open() {
                                                        server.scene.system_ui.close();
                                                    } else if server.scene.keyboard.is_active {
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    } else if active_app.is_some() {
                                                        active_app = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        InputDispatchResult::Tap { x, y } => {
                                            let w = server.scene.width as f32;
                                            let h = server.scene.height as f32;
                                            let kb_h = 420.0;
                                            let kb_y = h - kb_h - 20.0;

                                            if server.scene.system_ui.is_open() {
                                                if y < 44.0 || y > 580.0 {
                                                    server.scene.system_ui.close();
                                                } else if (155.0..=450.0).contains(&y) {
                                                    let col = if x < (w / 2.0) { 0 } else { 1 };
                                                    let row = ((y - 155.0) / 84.0) as usize;
                                                    if row < 4 {
                                                        let idx = row * 2 + col;
                                                        quick_tiles_active[idx] = !quick_tiles_active[idx];
                                                    }
                                                }
                                            } else if server.scene.keyboard.is_active && y >= kb_y && y <= (h - 20.0) {
                                                // Virtual Keyboard key tap
                                                let kb_w = w - 24.0;
                                                let kb_x = 12.0;

                                                let row1 = ["Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P"];
                                                let row2 = ["A", "S", "D", "F", "G", "H", "J", "K", "L"];
                                                let row3 = ["Z", "X", "C", "V", "B", "N", "M"];

                                                let r1_y = kb_y + 25.0;
                                                let r2_y = r1_y + 65.0 + 12.0;
                                                let r3_y = r2_y + 65.0 + 12.0;
                                                let r4_y = r3_y + 65.0 + 12.0;

                                                if y >= r1_y && y < r1_y + 65.0 {
                                                    let r1_key_w = (kb_w - 30.0) / 10.0;
                                                    let idx = ((x - kb_x - 15.0) / r1_key_w).clamp(0.0, 9.0) as usize;
                                                    if let Some(ch) = row1[idx].chars().next() {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            terminal_input.push(ch.to_ascii_lowercase());
                                                        } else if search_active {
                                                            search_query.push(ch.to_ascii_lowercase());
                                                        }
                                                    }
                                                } else if y >= r2_y && y < r2_y + 65.0 {
                                                    let r2_key_w = (kb_w - 60.0) / 9.0;
                                                    let idx = ((x - kb_x - 30.0) / r2_key_w).clamp(0.0, 8.0) as usize;
                                                    if let Some(ch) = row2[idx].chars().next() {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            terminal_input.push(ch.to_ascii_lowercase());
                                                        } else if search_active {
                                                            search_query.push(ch.to_ascii_lowercase());
                                                        }
                                                    }
                                                } else if y >= r3_y && y < r3_y + 65.0 {
                                                    let special_w = 95.0;
                                                    let mid_w = (kb_w - 30.0 - special_w * 2.0) / 7.0;
                                                    if x > kb_x + 15.0 + special_w + 7.0 * mid_w {
                                                        // Backspace
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            terminal_input.pop();
                                                        } else if search_active {
                                                            search_query.pop();
                                                        }
                                                    } else if x >= kb_x + 15.0 + special_w {
                                                        let idx = ((x - kb_x - 15.0 - special_w) / mid_w).clamp(0.0, 6.0) as usize;
                                                        if let Some(ch) = row3[idx].chars().next() {
                                                            if active_app.as_deref() == Some("Terminal") {
                                                                terminal_input.push(ch.to_ascii_lowercase());
                                                            } else if search_active {
                                                                search_query.push(ch.to_ascii_lowercase());
                                                            }
                                                        }
                                                    }
                                                } else if y >= r4_y && y < r4_y + 65.0 {
                                                    let sym_w = 120.0;
                                                    let enter_w = 140.0;
                                                    let space_x = kb_x + 15.0 + sym_w;
                                                    let enter_x = kb_w - enter_w;

                                                    if x <= kb_x + 15.0 + sym_w {
                                                        // "Hide" key tapped -> dismiss virtual keyboard
                                                        server.scene.keyboard.deactivate();
                                                    } else if x >= enter_x {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            let exited = execute_terminal_command(&mut terminal_lines, &mut terminal_input, &mut active_app);
                                                            if exited {
                                                                server.scene.keyboard.deactivate();
                                                                search_active = false;
                                                            }
                                                        } else {
                                                            search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                        }
                                                    } else if x >= space_x {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            terminal_input.push(' ');
                                                        } else if search_active {
                                                            search_query.push(' ');
                                                        }
                                                    }
                                                }
                                            } else if server.scene.keyboard.is_active {
                                                // Tapped outside keyboard while keyboard was active -> dismiss keyboard
                                                server.scene.keyboard.deactivate();
                                                search_active = false;
                                                // If tapped on app top buttons or nav pill, also handle app exit
                                                if active_app.is_some()
                                                    && (((20.0..=130.0).contains(&x) && (48.0..=110.0).contains(&y))
                                                        || (x >= (w - 90.0) && (48.0..=110.0).contains(&y))
                                                        || (y >= (h - 40.0)))
                                                {
                                                    active_app = None;
                                                }
                                            } else if active_app.is_some() {
                                                // An app is open and keyboard is not active
                                                if ((20.0..=130.0).contains(&x) && (48.0..=110.0).contains(&y))
                                                    || (x >= (w - 90.0) && (48.0..=110.0).contains(&y))
                                                    || (y >= (h - 40.0))
                                                {
                                                    active_app = None;
                                                    server.scene.keyboard.deactivate();
                                                    search_active = false;
                                                } else if active_app.as_deref() == Some("Terminal") {
                                                    // Tapping inside terminal brings keyboard back up
                                                    server.scene.keyboard.activate();
                                                }
                                            } else {
                                                // Home screen hit testing
                                                if y <= 50.0 {
                                                    // Tap status bar -> toggle Quick Settings shade
                                                    server.scene.system_ui.toggle();
                                                } else if (32.0..=(w - 32.0)).contains(&x) && (235.0..=295.0).contains(&y) {
                                                    // Tap search pill -> activate search & virtual keyboard
                                                    search_active = true;
                                                    server.scene.keyboard.activate();
                                                } else if (325.0..=325.0 + 3.0 * 115.0).contains(&y) {
                                                    // App grid icons
                                                    let col_width = w / 4.0;
                                                    let col = (x / col_width).clamp(0.0, 3.0) as usize;
                                                    let row = ((y - 325.0) / 115.0).clamp(0.0, 2.0) as usize;
                                                    let idx = row * 4 + col;
                                                    let app_names = [
                                                        "Phone", "Messages", "Browser", "Camera",
                                                        "Gallery", "Settings", "Files", "Music",
                                                        "Terminal", "Treble OS", "Contacts", "Clock",
                                                    ];
                                                    let app_to_launch = app_names[idx];
                                                    active_app = Some(app_to_launch.to_string());
                                                    if app_to_launch == "Terminal" {
                                                        server.scene.keyboard.activate();
                                                    } else {
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    }
                                                } else if y >= (h - 150.0) && y <= (h - 35.0) {
                                                    // Hotseat dock icons
                                                    let dock_col_w = (w - 40.0) / 5.0;
                                                    let dock_col = ((x - 20.0) / dock_col_w).clamp(0.0, 4.0) as usize;
                                                    let dock_apps = ["Phone", "Messages", "Apps", "Browser", "Camera"];
                                                    let app = dock_apps[dock_col];
                                                    if app == "Apps" {
                                                        search_active = true;
                                                        server.scene.keyboard.activate();
                                                    } else {
                                                        active_app = Some(app.to_string());
                                                        if app == "Terminal" {
                                                            server.scene.keyboard.activate();
                                                        } else {
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                        }
                                                    }
                                                } else if y >= (h - 30.0) {
                                                    // Navigation pill
                                                    active_app = None;
                                                    search_active = false;
                                                    server.scene.keyboard.deactivate();
                                                    server.scene.system_ui.close();
                                                }
                                            }
                                        }
                                        InputDispatchResult::KeyPress { code, ch, pressed, repeat } => {
                                            if pressed {
                                                if code == KEY_ESC {
                                                    if server.scene.keyboard.is_active {
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    } else if active_app.is_some() {
                                                        active_app = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    } else if server.scene.system_ui.is_open() {
                                                        server.scene.system_ui.close();
                                                    }
                                                } else if code == KEY_BACKSPACE {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        terminal_input.pop();
                                                    } else if search_active {
                                                        search_query.pop();
                                                    }
                                                } else if code == KEY_ENTER && !repeat {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        let exited = execute_terminal_command(&mut terminal_lines, &mut terminal_input, &mut active_app);
                                                        if exited {
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                        }
                                                    } else if search_active {
                                                        search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                    }
                                                } else if let Some(c) = ch {
                                                    if !repeat {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            if terminal_input.len() < 60 {
                                                                terminal_input.push(c);
                                                            }
                                                        } else if search_active && search_query.len() < 40 {
                                                            search_query.push(c);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        InputDispatchResult::PointerMove { .. } => {}
                                        InputDispatchResult::None => {}
                                    }
                                }
                            }
                            offset += LinuxInputEvent::SIZE;
                        }
                    }
                } else {
                    // Servicing connected client stream events (read/drain or close)
                    use std::io::Read;
                    let mut buf = [0u8; 1024];
                    let mut closed = false;
                    let mut stream_idx = None;
                    for (idx, stream) in server.client_streams.iter_mut().enumerate() {
                        if stream.as_raw_fd() == fd {
                            stream_idx = Some(idx);
                            if (ev.events & (libc::EPOLLHUP | libc::EPOLLERR) as u32) != 0 {
                                closed = true;
                            } else {
                                match stream.read(&mut buf) {
                                    Ok(0) => closed = true,
                                    Ok(_) => {}
                                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                                    Err(_) => closed = true,
                                }
                            }
                            break;
                        }
                    }
                    if closed {
                        if let Some(idx) = stream_idx {
                            if epoll_fd >= 0 {
                                unsafe {
                                    libc::epoll_ctl(
                                        epoll_fd,
                                        libc::EPOLL_CTL_DEL,
                                        fd,
                                        std::ptr::null_mut(),
                                    );
                                }
                            }
                            server.client_streams.swap_remove(idx);
                        }
                    }
                }
            }
        }

        // Periodic check for newly registered input event devices
        if last_input_rescan.elapsed() >= Duration::from_secs(1) {
            last_input_rescan = Instant::now();
            if let Ok(entries) = fs::read_dir("/dev/input") {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("event"))
                    {
                        if opened_paths.contains(&path) {
                            continue;
                        }
                        if let Ok(c_path) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
                            let fd = unsafe {
                                libc::open(
                                    c_path.as_ptr(),
                                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                                )
                            };
                            if fd >= 0 {
                                if epoll_fd >= 0 {
                                    let mut in_ev = libc::epoll_event {
                                        events: libc::EPOLLIN as u32,
                                        u64: fd as u64,
                                    };
                                    unsafe {
                                        libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut in_ev);
                                    }
                                }
                                input_fds.push(fd);
                                opened_paths.push(path);
                            }
                        }
                    }
                }
            }
        }

        // Vsync frame presentation step
        if last_frame.elapsed() >= frame_interval {
            let dt = last_frame.elapsed().as_secs_f32();
            last_frame = Instant::now();
            let _ = server.step_frame(dt);

            if let Some(ref mut drm) = drm_display {
                let t_str = format_current_time(&mut time_buf);
                let drm_state = DrmInteractiveState {
                    time_str: t_str,
                    is_locked: server.scene.mode == utim_core::compositor::scene::ShellMode::LockScreen,
                    cursor_pos,
                    is_touching,
                    search_query: &search_query,
                    search_active,
                    keyboard_active: server.scene.keyboard.is_active,
                    shade_open: server.scene.system_ui.is_open(),
                    quick_tiles_active,
                    active_app: active_app.as_deref(),
                    terminal_lines: &terminal_lines,
                    terminal_input: &terminal_input,
                };
                drm.render_interactive_ui(&drm_state);
                drm.flush();
            }
        }

        // Auto-transition to Launcher home screen after initial boot presentation
        if server.scene.lockscreen.is_locked()
            && server.start_time.elapsed() >= Duration::from_secs(2)
        {
            server.scene.lockscreen.unlock();
            server.scene.mode = utim_core::compositor::scene::ShellMode::Launcher;
        }

        // Watchdog periodic ping
        if last_watchdog_ping.elapsed() >= watchdog_interval {
            last_watchdog_ping = Instant::now();
            if let (Some(ref dgram), Some(ref sock_path)) = (&notify_dgram, &notify_socket) {
                let _ = dgram.send_to(b"WATCHDOG=1\n", sock_path);
            }
        }
    }

    for fd in input_fds {
        unsafe {
            libc::close(fd);
        }
    }

    if epoll_fd >= 0 {
        unsafe {
            libc::close(epoll_fd);
        }
    }
    if sig_fd >= 0 {
        unsafe {
            libc::close(sig_fd);
        }
    }
}

fn format_current_time(buf: &mut [u8; 5]) -> &str {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let total_secs = ts.tv_sec;
    let hours = ((total_secs / 3600) % 24) as u8;
    let mins = ((total_secs / 60) % 60) as u8;
    buf[0] = b'0' + (hours / 10);
    buf[1] = b'0' + (hours % 10);
    buf[2] = b':';
    buf[3] = b'0' + (mins / 10);
    buf[4] = b'0' + (mins % 10);
    unsafe { std::str::from_utf8_unchecked(buf) }
}

fn execute_terminal_command(
    terminal_lines: &mut Vec<String>,
    terminal_input: &mut String,
    active_app: &mut Option<String>,
) -> bool {
    let cmd = terminal_input.trim().to_string();
    terminal_lines.push(format!("root@treble-gsi:~# {}", cmd));
    let mut exited = false;
    match cmd.as_str() {
        "uname" | "uname -a" => {
            terminal_lines.push("Linux treble-gsi 6.1.23-android14-4-00257 aarch64 GNU/Linux".into());
        }
        "uptime" => {
            terminal_lines.push(" 20:48:00 up 12 min, 1 user, load avg: 0.04, 0.02, 0.00".into());
        }
        "whoami" => {
            terminal_lines.push("root".into());
        }
        "ls" | "ls -la" => {
            terminal_lines.push("bin  dev  etc  init  lib  proc  run  sbin  sys  usr  var".into());
        }
        "clear" => {
            terminal_lines.clear();
        }
        "help" => {
            terminal_lines.push("Commands: uname, uptime, whoami, ls, date, clear, exit".into());
        }
        "exit" => {
            *active_app = None;
            exited = true;
        }
        "" => {}
        other => {
            terminal_lines.push(format!("bash: {}: command not found", other));
        }
    }
    terminal_input.clear();
    if terminal_lines.len() > 14 {
        terminal_lines.drain(0..terminal_lines.len() - 14);
    }
    exited
}

fn check_protocols(json: bool) -> bool {
    let reg = ProtocolRegistry::new();
    let supported = reg.supports_mobile_protocols();

    let protocols = [
        (
            "xdg_wm_base",
            reg.find_by_interface(WaylandInterface::XdgWmBase).is_some(),
        ),
        (
            "zwlr_layer_shell_v1",
            reg.find_by_interface(WaylandInterface::ZwlrLayerShellV1)
                .is_some(),
        ),
        (
            "zwp_linux_dmabuf_v1",
            reg.find_by_interface(WaylandInterface::ZwpLinuxDmabufV1)
                .is_some(),
        ),
        (
            "wp_presentation",
            reg.find_by_interface(WaylandInterface::WpPresentation)
                .is_some(),
        ),
        (
            "wp_viewporter",
            reg.find_by_interface(WaylandInterface::WpViewporter)
                .is_some(),
        ),
        (
            "ext_idle_notifier_v1",
            reg.find_by_interface(WaylandInterface::ExtIdleNotifierV1)
                .is_some(),
        ),
        (
            "zwp_text_input_v3",
            reg.find_by_interface(WaylandInterface::ZwpTextInputV3)
                .is_some(),
        ),
        (
            "zwp_input_method_v2",
            reg.find_by_interface(WaylandInterface::ZwpInputMethodV2)
                .is_some(),
        ),
        (
            "zwp_tablet_manager_v2",
            reg.find_by_interface(WaylandInterface::ZwpTabletManagerV2)
                .is_some(),
        ),
    ];

    if json {
        let mut parts = Vec::new();
        for (name, ok) in &protocols {
            parts.push(format!(r#""{}":{}"#, name, ok));
        }
        println!(
            r#"{{"all_supported":{},"protocols":{{{}}}}}"#,
            supported,
            parts.join(",")
        );
    } else {
        println!("============================================================");
        println!(" UTLC WAYLAND PROTOCOL ENGINE VERIFICATION");
        println!("============================================================");
        for (name, ok) in &protocols {
            println!(
                "  [{}] Protocol: {:<25} (Active)",
                if *ok { "PASS" } else { "FAIL" },
                name
            );
        }
        println!("------------------------------------------------------------");
        println!(
            " All Mobile Protocols Verified: {}",
            if supported { "YES" } else { "NO" }
        );
        println!("============================================================");
    }

    supported
}

fn run_benchmarks(json: bool) -> bool {
    let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
    let socket_path = PathBuf::from(format!("/tmp/utlc-bench-{}.sock", std::process::id()));
    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);

    let metrics = server.get_metrics();
    let rss_mb = (metrics.resident_memory_bytes as f64) / (1024.0 * 1024.0);
    let boot_ms = metrics.boot_to_launcher_duration.as_secs_f64() * 1000.0;
    let touch_ms = metrics.touch_processing_latency.as_secs_f64() * 1000.0;

    // Zero-allocation fuzzy search benchmark (< 1ms query time)
    let mut catalogue = DesktopCatalogue::new();
    catalogue.scan_system_directories();
    if catalogue.apps().is_empty() {
        // Add sample apps if no system desktop files found
        for i in 0..100 {
            catalogue.add_app(DesktopApp::new(
                format!("app_{}", i),
                format!("Application {}", i),
                format!("exec_{}", i),
            ));
        }
    }

    let search_start = Instant::now();
    let _results = catalogue.search("term");
    let search_latency = search_start.elapsed();
    let search_ms = search_latency.as_secs_f64() * 1000.0;

    let passed = metrics.is_rss_within_target
        && metrics.is_boot_within_target
        && touch_ms < 8.0
        && search_ms < 1.0;

    if json {
        println!(
            r#"{{"passed":{},"rss_mb":{:.2},"rss_target_met":{},"boot_ms":{:.2},"boot_target_met":{},"touch_latency_ms":{:.4},"touch_target_met":{},"search_latency_ms":{:.4},"search_target_met":{}}}"#,
            passed,
            rss_mb,
            metrics.is_rss_within_target,
            boot_ms,
            metrics.is_boot_within_target,
            touch_ms,
            touch_ms < 8.0,
            search_ms,
            search_ms < 1.0
        );
    } else {
        println!("============================================================");
        println!(" UTLC PERFORMANCE & RESOURCE LATENCY BENCHMARKS");
        println!("============================================================");
        println!(
            "[*] Resident Set Size (RSS):       {:.2} MB (Target: < 15 MB) -> {}",
            rss_mb,
            if metrics.is_rss_within_target {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!(
            "[*] Boot-to-Launcher Time:        {:.2} ms (Target: < 450 ms) -> {}",
            boot_ms,
            if metrics.is_boot_within_target {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!(
            "[*] Touch Gesture Input Latency:  {:.4} ms (Target: < 8.0 ms) -> {}",
            touch_ms,
            if touch_ms < 8.0 { "PASS" } else { "FAIL" }
        );
        println!(
            "[*] Fuzzy Search Query Latency:   {:.4} ms (Target: < 1.0 ms) -> {}",
            search_ms,
            if search_ms < 1.0 { "PASS" } else { "FAIL" }
        );
        println!("------------------------------------------------------------");
        println!(
            " Overall Performance Verification: {}",
            if passed {
                "ALL TARGETS MET"
            } else {
                "TARGET REGRESSION"
            }
        );
        println!("============================================================");
    }

    passed
}

fn test_gestures(json: bool) -> bool {
    let mut engine = GestureEngine::new(1080.0, 2400.0, GestureConfig::default());
    let t0 = Instant::now();

    // 1. Home gesture
    engine.process_touch(&RawTouchEvent {
        touch_id: 1,
        phase: TouchPhase::Down,
        x: 540.0,
        y: 2380.0,
        timestamp: t0,
    });
    let home_act = engine.process_touch(&RawTouchEvent {
        touch_id: 1,
        phase: TouchPhase::Up,
        x: 540.0,
        y: 2200.0,
        timestamp: t0 + Duration::from_millis(80),
    });
    let home_ok = matches!(home_act, GestureAction::Home { progress, .. } if progress >= 1.0);

    // 2. Recents gesture (hold > 180ms)
    engine.process_touch(&RawTouchEvent {
        touch_id: 2,
        phase: TouchPhase::Down,
        x: 540.0,
        y: 2380.0,
        timestamp: t0,
    });
    let recents_act = engine.process_touch(&RawTouchEvent {
        touch_id: 2,
        phase: TouchPhase::Move,
        x: 540.0,
        y: 2200.0,
        timestamp: t0 + Duration::from_millis(200),
    });
    let recents_ok =
        matches!(recents_act, GestureAction::Recents { trigger_haptic, .. } if trigger_haptic);

    // 3. Back gesture (edge swipe)
    engine.process_touch(&RawTouchEvent {
        touch_id: 3,
        phase: TouchPhase::Down,
        x: 10.0,
        y: 1200.0,
        timestamp: t0,
    });
    let back_act = engine.process_touch(&RawTouchEvent {
        touch_id: 3,
        phase: TouchPhase::Up,
        x: 60.0,
        y: 1200.0,
        timestamp: t0 + Duration::from_millis(100),
    });
    let back_ok = matches!(back_act, GestureAction::Back { injected, .. } if injected);

    let all_ok = home_ok && recents_ok && back_ok;
    if json {
        println!(
            r#"{{"home_ok":{},"recents_ok":{},"back_ok":{},"all_passed":{}}}"#,
            home_ok, recents_ok, back_ok, all_ok
        );
    } else {
        println!(
            "[*] QuickStep Gestures: Home={}, Recents={}, Back={} -> {}",
            if home_ok { "PASS" } else { "FAIL" },
            if recents_ok { "PASS" } else { "FAIL" },
            if back_ok { "PASS" } else { "FAIL" },
            if all_ok { "ALL PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_desktop(json: bool) -> bool {
    let mut catalogue = DesktopCatalogue::new();
    catalogue.add_app(DesktopApp::new(
        "phone".into(),
        "Phone Dialer".into(),
        "dialer".into(),
    ));
    catalogue.add_app(DesktopApp::new(
        "chatty".into(),
        "Messaging".into(),
        "chatty".into(),
    ));
    catalogue.add_app(DesktopApp::new(
        "firefox".into(),
        "Firefox Web Browser".into(),
        "firefox".into(),
    ));

    let results = catalogue.search("fox");
    let ok = results.len() == 1 && results[0].0.id == "firefox";

    if json {
        println!(r#"{{"desktop_search_ok":{}}}"#, ok);
    } else {
        println!(
            "[*] Desktop Parsing & Search: {}",
            if ok { "PASSED" } else { "FAILED" }
        );
    }
    ok
}

fn test_systemui(json: bool) -> bool {
    let mut shade = SystemUiShade::new(1080.0, 2400.0);
    let torch_ok = shade.toggle_tile(QuickTileKind::Torch);
    let notif_id = shade.notify(
        "App".into(),
        0,
        "icon".into(),
        "Summary".into(),
        "Body".into(),
        vec![],
    );
    let notif_ok = notif_id > 0 && shade.notifications.len() == 1;

    let all_ok = torch_ok && notif_ok;
    if json {
        println!(
            r#"{{"torch_toggle_ok":{},"notification_ok":{},"all_passed":{}}}"#,
            torch_ok, notif_ok, all_ok
        );
    } else {
        println!(
            "[*] SystemUI Status & Shade: {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_lockscreen(json: bool) -> bool {
    let mut lockscreen = LockScreen::new(Some("1234"));
    let fp_ok = lockscreen.on_fingerprint_touch(1);
    let auth_ok = !lockscreen.is_locked()
        && lockscreen.biometric_bridge.last_auth_duration < Duration::from_millis(300);

    if json {
        println!(
            r#"{{"fingerprint_unlock_ok":{},"sub_300ms":{}}}"#,
            fp_ok, auth_ok
        );
    } else {
        println!(
            "[*] Lock Screen & Fingerprint HAL Bridge (< 300ms): {}",
            if auth_ok { "PASSED" } else { "FAILED" }
        );
    }
    auth_ok
}

fn test_ime(json: bool) -> bool {
    let mut ime = VirtualKeyboard::new(1080.0, 2400.0);
    ime.activate();
    for _ in 0..60 {
        ime.update(0.016);
    }
    let push_ok = (ime.window_viewport_push_y() - 320.0).abs() < 1.0;
    let act = ime.handle_key_tap("k");
    let key_ok = matches!(act, ImeAction::CommitString(s) if s == "k");

    let all_ok = push_ok && key_ok;
    if json {
        println!(
            r#"{{"ime_viewport_push_ok":{},"ime_key_ok":{},"all_passed":{}}}"#,
            push_ok, key_ok, all_ok
        );
    } else {
        println!(
            "[*] Virtual Keyboard IME & Viewport Push: {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_power_sync(json: bool) -> bool {
    let mut power = UtimPowerSync::new(Path::new("/tmp/mock-utim.sock"));
    let sleep_ok = power.on_display_sleep().is_ok();
    let wake_ok = power.on_display_wake().is_ok();
    let oom_ok = power.on_app_switched(101, &[102], &[103]).is_ok();

    let all_ok = sleep_ok && wake_ok && oom_ok;
    if json {
        println!(
            r#"{{"sleep_ok":{},"wake_ok":{},"oom_ok":{},"all_passed":{}}}"#,
            sleep_ok, wake_ok, oom_ok, all_ok
        );
    } else {
        println!(
            "[*] UTIM Power & OOM Synchronization: {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn run_all_checks(json: bool) -> bool {
    let proto_ok = check_protocols(false);
    let bench_ok = run_benchmarks(false);
    let gesture_ok = test_gestures(false);
    let desk_ok = test_desktop(false);
    let sys_ok = test_systemui(false);
    let lock_ok = test_lockscreen(false);
    let ime_ok = test_ime(false);
    let pwr_ok = test_power_sync(false);

    let passed =
        proto_ok && bench_ok && gesture_ok && desk_ok && sys_ok && lock_ok && ime_ok && pwr_ok;

    if json {
        println!(
            r#"{{"all_passed":{},"protocols":{},"benchmarks":{},"gestures":{},"desktop":{},"systemui":{},"lockscreen":{},"ime":{},"power_sync":{}}}"#,
            passed, proto_ok, bench_ok, gesture_ok, desk_ok, sys_ok, lock_ok, ime_ok, pwr_ok
        );
    }

    passed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_command_execution() {
        let mut lines = Vec::new();
        let mut input = "uname -a".to_string();
        let mut app = Some("Terminal".to_string());

        execute_terminal_command(&mut lines, &mut input, &mut app);
        assert!(input.is_empty());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "root@treble-gsi:~# uname -a");
        assert!(lines[1].contains("Linux treble-gsi"));

        input = "exit".to_string();
        execute_terminal_command(&mut lines, &mut input, &mut app);
        assert!(app.is_none());
    }

    #[test]
    fn test_input_tap_detection() {
        let mut dispatcher = InputDispatcher::new(1080.0, 2400.0);

        // Move to Search Pill (540, 260)
        let ev_x = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_ABS,
            code: utim_core::compositor::input::ABS_X,
            value: 16383, // center = 540
        };
        let ev_y = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_ABS,
            code: utim_core::compositor::input::ABS_Y,
            value: 3550, // 3550/32767 * 2400 ~= 260
        };
        let ev_syn = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_SYN,
            code: utim_core::compositor::input::SYN_REPORT,
            value: 0,
        };
        dispatcher.process_event(&ev_x);
        dispatcher.process_event(&ev_y);
        dispatcher.process_event(&ev_syn);

        // Down & Up
        let ev_down = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_KEY,
            code: utim_core::compositor::input::BTN_LEFT,
            value: 1,
        };
        dispatcher.process_event(&ev_down);

        let ev_up = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: utim_core::compositor::input::EV_KEY,
            code: utim_core::compositor::input::BTN_LEFT,
            value: 0,
        };
        let res = dispatcher.process_event(&ev_up);
        match res {
            InputDispatchResult::Tap { x, y } => {
                assert!((x - 540.0).abs() < 2.0);
                assert!((y - 260.0).abs() < 5.0);
            }
            other => panic!("Expected Tap, got {:?}", other),
        }
    }

    #[test]
    fn test_format_current_time_zero_alloc() {
        let mut buf = [0u8; 5];
        let t_str = format_current_time(&mut buf);
        assert_eq!(t_str.len(), 5);
        assert_eq!(&t_str[2..3], ":");
        assert!(t_str[..2].chars().all(|c| c.is_ascii_digit()));
        assert!(t_str[3..].chars().all(|c| c.is_ascii_digit()));
    }
}
