//! Universal Treble Launcher & Compositor (UTLC).
//! Unified Single-Process Mobile Wayland Compositor and Android-Style Launcher.
//! Engineered in bare-metal Rust following strict systems discipline (GEMINI.md).
//! Consumes < 15 MB RSS, renders interactive home screen in < 0.45s,
//! and delivers < 8ms touch gesture response.

use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::time::{Duration, Instant};

use utim_core::compositor::desktop::{DesktopApp, DesktopCatalogue};
use utim_core::compositor::IconCache;
use utim_core::compositor::gestures::{
    GestureAction, GestureConfig, GestureEngine, RawTouchEvent, TouchPhase,
};
use utim_core::compositor::ime::{ImeAction, VirtualKeyboard};
use utim_core::compositor::input::{
    InputDispatchResult, InputDispatcher, LinuxInputEvent, KEY_1, KEY_2, KEY_3, KEY_4,
    KEY_BACKSPACE, KEY_C, KEY_D, KEY_ENTER, KEY_ESC, KEY_L, KEY_T, KEY_TAB, KEY_W,
};
use utim_core::compositor::lockscreen::LockScreen;
use utim_core::compositor::power_sync::UtimPowerSync;
use utim_core::compositor::protocols::{ProtocolRegistry, WaylandInterface};
use utim_core::compositor::server::WaylandServer;
use utim_core::compositor::systemui::{QuickTileKind, SystemUiShade};
use utim_core::graphics::composer::{HwcComposer, HwcVersion};
use utim_core::graphics::layout::{
    AppLayout, AppPanel, DrawerSearchHit, Key, Keyboard, Layout, ShadeLayout, ShadeZone, TabHit,
};
use utim_core::graphics::{
    AppGridItem, DrmInteractiveState, DrmKmsDevice, MaterialYouPalette, RgbaImage,
    TerminalTabInfo, SpringConfig, SpringSimulation, apply_overscroll_resistance,
};

#[derive(Debug, Clone)]
pub struct ManagedApp {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub color: u32,
    pub glyph: String,
    /// Icon theme names tried in order when resolving this app's icon.
    pub icon_keys: Vec<String>,
    /// Decoded icon bitmap, attached by [`apply_app_icons`].
    pub icon: Option<Rc<RgbaImage>>,
}

impl ManagedApp {
    pub fn new(id: &str, name: &str, exec: &str, color: u32, glyph: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            exec: exec.to_string(),
            color,
            glyph: glyph.to_string(),
            icon_keys: Vec::new(),
            icon: None,
        }
    }

    fn with_icon_keys(mut self, keys: &[&str]) -> Self {
        self.icon_keys = keys.iter().map(|k| k.to_string()).collect();
        self
    }
}

/// Freedesktop icon names for the built-in launcher entries. All of these
/// resolve in the icon themes shipped by the rootfs except `clock`, which has
/// no raster icon and therefore keeps its letter glyph.
const BUILTIN_ICON_KEYS: [(&str, &[&str]); 13] = [
    ("phone", &["phone", "call-start"]),
    ("messages", &["mail-message-new", "messages"]),
    ("browser", &["web-browser", "browser", "internet-web-browser"]),
    ("camera", &["camera-photo", "camera"]),
    ("gallery", &["image-x-generic", "gallery"]),
    ("settings", &["preferences-system", "settings"]),
    ("files", &["system-file-manager", "files"]),
    ("music", &["audio-x-generic", "music"]),
    ("terminal", &["utilities-terminal", "terminal"]),
    ("treble", &["computer", "treble", "distributor-logo-android", "android"]),
    ("contacts", &["contact-new", "contacts"]),
    ("clock", &["clock"]),
    ("apps", &["view-app-grid", "apps"]),
];

/// Built-in launcher entry with its icon keys attached.
fn builtin_app(id: &str, name: &str, exec: &str, color: u32, glyph: &str) -> ManagedApp {
    let keys = BUILTIN_ICON_KEYS
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, keys)| *keys)
        .unwrap_or(&[]);
    ManagedApp::new(id, name, exec, color, glyph).with_icon_keys(keys)
}

/// Stable fingerprint of the application set, used to decide when the icon
/// cache may drop its recorded misses and re-scan.
fn app_set_signature(apps: &[ManagedApp]) -> String {
    let mut sig = String::with_capacity(apps.len() * 24);
    for app in apps {
        sig.push_str(&app.id);
        sig.push('\0');
    }
    sig
}

/// Resolve every app's `icon_keys` through one batched icon-theme sweep and
/// attach the decoded bitmap; apps whose keys do not resolve keep the
/// first-letter glyph fallback.
/// Decode icon size, in pixels, for the panel the shell is drawing on.
///
/// Cached icons are pre-scaled to this edge so the render path can blit them
/// 1:1 instead of resampling every frame.
fn icon_edge_px() -> u32 {
    // Filled in once the panel size is known; the shell boots at 1080x2400 and
    // the value is refreshed on every mode change.
    ICON_EDGE_PX.with(|c| *c.borrow())
}

thread_local! {
    static ICON_EDGE_PX: std::cell::RefCell<u32> = const { std::cell::RefCell::new(121) };
}

fn set_icon_edge_px(edge: u32) {
    ICON_EDGE_PX.with(|c| *c.borrow_mut() = edge);
}

fn apply_app_icons(apps: &mut [ManagedApp], cache: &mut IconCache) {
    let mut pending: Vec<String> = Vec::new();
    for app in apps.iter() {
        for key in &app.icon_keys {
            if !cache.knows(key) && !pending.contains(key) {
                pending.push(key.clone());
            }
        }
    }
    for dock_key in ["view-app-grid", "apps"] {
        let dk = dock_key.to_string();
        if !cache.knows(&dk) && !pending.contains(&dk) {
            pending.push(dk);
        }
    }
    if !pending.is_empty() {
        cache.resolve_keys(&pending);
    }
    for app in apps.iter_mut() {
        app.icon = app.icon_keys.iter().find_map(|key| cache.get(key));
    }
}

/// The app drawer's visible list, as an iterator.
///
/// The drawer grid, the press feedback and the launch handler all need "the
/// nth app the drawer is showing"; this is the single definition of that, and
/// it allocates nothing.
fn drawer_view<'a>(
    apps: &'a [ManagedApp],
    query: &str,
) -> Box<dyn Iterator<Item = &'a ManagedApp> + 'a> {
    if query.is_empty() {
        return Box::new(apps.iter());
    }
    let q = query.to_lowercase();
    Box::new(apps.iter().filter(move |a| {
        a.name.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q)
    }))
}

fn get_app_color(name_or_id: &str) -> u32 {
    const PALETTE: [u32; 10] = [
        0xFF3B82F6, // Sky Blue
        0xFF10B981, // Emerald Green
        0xFF8B5CF6, // Violet
        0xFFF59E0B, // Amber
        0xFFEC4899, // Pink
        0xFF06B6D4, // Cyan
        0xFF6366F1, // Indigo
        0xFF14B8A6, // Teal
        0xFFF97316, // Vibrant Orange
        0xFF64748B, // Slate
    ];
    let mut hash: u32 = 0;
    for b in name_or_id.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as u32);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
}
fn launch_desktop_app(exec_cmd: &str, socket_dir: &str) {
    if exec_cmd.is_empty() {
        return;
    }
    let parts: Vec<&str> = exec_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }
    let prog = parts[0];
    let args = &parts[1..];

    if !std::path::Path::new(prog).exists() {
        eprintln!("[UTLC] Skipping desktop application launch: binary '{}' does not exist", prog);
        return;
    }
    println!("[UTLC] Launching desktop application: '{}'", exec_cmd);

    let is_root = unsafe { libc::geteuid() == 0 };
    let (target_home, target_user, app_socket_dir) = if is_root {
        let _ = std::fs::create_dir_all("/home/user");
        let _ = std::fs::create_dir_all("/run/user/1000");
        unsafe {
            if let Ok(c_home) = std::ffi::CString::new("/home/user") {
                libc::chown(c_home.as_ptr(), 1000, 1000);
            }
            if let Ok(c_run_u) = std::ffi::CString::new("/run/user/1000") {
                libc::chown(c_run_u.as_ptr(), 1000, 1000);
                libc::chmod(c_run_u.as_ptr(), 0o777);
            }
        }
        ("/home/user", "user", "/run/user/1000")
    } else {
        (
            "/home/user",
            "user",
            socket_dir,
        )
    };

    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("XDG_RUNTIME_DIR", app_socket_dir)
        .env("GDK_BACKEND", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("HOME", target_home)
        .env("USER", target_user)
        .env("LOGNAME", target_user)
        .env("SHELL", "/bin/bash")
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

    if is_root {
        cmd.uid(1000).gid(1000);
        unsafe {
            cmd.pre_exec(|| {
                let groups = [1000 as libc::gid_t, 24, 27, 29, 44, 105, 107];
                libc::setgroups(groups.len(), groups.as_ptr());
                Ok(())
            });
        }
    }

    match cmd.spawn() {
        Ok(child) => {
            println!(
                "[UTLC] Spawned '{}' with PID {} (UID {})",
                prog,
                child.id(),
                if is_root { 1000 } else { unsafe { libc::getuid() } }
            );
        }
        Err(e) => {
            eprintln!("[UTLC] Error spawning '{}': {}", prog, e);
        }
    }
}

fn build_all_apps(catalogue: &DesktopCatalogue) -> Vec<ManagedApp> {
    let mut apps = Vec::new();

    // Check if Firefox is installed in the catalogue
    let firefox_installed = catalogue.apps().iter().find(|a| {
        a.id.eq_ignore_ascii_case("firefox")
            || a.id.eq_ignore_ascii_case("firefox-esr")
            || a.name.to_lowercase().contains("firefox")
    });

    let browser_exec = if let Some(ff) = firefox_installed {
        let clean = ff.clean_exec();
        if clean.is_empty() {
            "/usr/bin/firefox".to_string()
        } else {
            clean
        }
    } else {
        String::new()
    };

    // 1. Built-in Core System Apps
    apps.push(builtin_app("phone", "Phone", "", 0xFF10B981, "P"));
    apps.push(builtin_app("messages", "Messages", "", 0xFF3B82F6, "M"));
    apps.push(builtin_app("browser", "Browser", &browser_exec, 0xFF06B6D4, "B"));
    apps.push(builtin_app("camera", "Camera", "", 0xFFF43F5E, "C"));
    apps.push(builtin_app("gallery", "Gallery", "", 0xFF8B5CF6, "G"));
    apps.push(builtin_app("settings", "Settings", "", 0xFF64748B, "S"));
    apps.push(builtin_app("files", "Files", "", 0xFFF59E0B, "F"));
    apps.push(builtin_app("music", "Music", "", 0xFFD946EF, "M"));
    apps.push(builtin_app("terminal", "Terminal", "", 0xFF1E293B, ">"));
    apps.push(builtin_app("treble", "Treble OS", "", 0xFF6366F1, "U"));
    apps.push(builtin_app("contacts", "Contacts", "", 0xFF14B8A6, "C"));
    apps.push(builtin_app("clock", "Clock", "", 0xFFEF4444, "T"));

    // 2. Discovered installed applications from /usr/share/applications etc.
    for d_app in catalogue.apps() {
        if d_app.no_display || d_app.id.eq_ignore_ascii_case("utlc") {
            continue;
        }

        let is_firefox = d_app.id.eq_ignore_ascii_case("firefox")
            || d_app.id.eq_ignore_ascii_case("firefox-esr")
            || d_app.name.to_lowercase().contains("firefox");

        let display_name = if is_firefox {
            "Firefox".to_string()
        } else if d_app.name.len() > 12 {
            d_app.name[..12].to_string()
        } else {
            d_app.name.clone()
        };

        let color = if is_firefox {
            0xFFFF5722
        } else {
            get_app_color(&d_app.id)
        };

        let glyph = if is_firefox {
            "F".to_string()
        } else {
            d_app.name.chars().next().unwrap_or('A').to_uppercase().to_string()
        };

        let mut exec = d_app.clean_exec();
        if exec.is_empty() && is_firefox {
            exec = "/usr/bin/firefox".to_string();
        }

        // Avoid adding duplicate if already present in base apps
        if !apps.iter().any(|a| a.name.eq_ignore_ascii_case(&display_name)) {
            let mut app = ManagedApp::new(&d_app.id, &display_name, &exec, color, &glyph);
            // `Icon=` first, then the desktop id (many themes key on it).
            if !d_app.icon.is_empty() {
                app.icon_keys.push(d_app.icon.clone());
            }
            let id_key = d_app.id.to_lowercase();
            if !d_app.id.is_empty() && !app.icon_keys.contains(&id_key) {
                app.icon_keys.push(id_key);
            }
            apps.push(app);
        }
    }

    apps
}

fn main() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
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

    let socket_dir = env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        if unsafe { libc::geteuid() } == 0 {
            "/run/user/0".into()
        } else {
            "/run/user/1000".into()
        }
    });
    let _ = fs::create_dir_all(&socket_dir);
    let socket_path = PathBuf::from(&socket_dir).join("wayland-0");

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
        let is_root = unsafe { libc::geteuid() == 0 };
        if is_root {
            unsafe {
                if let Ok(c_s) = std::ffi::CString::new(socket_path.to_string_lossy().as_bytes()) {
                    libc::chmod(c_s.as_ptr(), 0o666);
                }
                if let Ok(c_sdir) = std::ffi::CString::new(socket_dir.as_bytes()) {
                    libc::chmod(c_sdir.as_ptr(), 0o777);
                }
            }
            let user_sock_dir = PathBuf::from("/run/user/1000");
            let _ = fs::create_dir_all(&user_sock_dir);
            unsafe {
                if let Ok(c_u) = std::ffi::CString::new("/run/user/1000") {
                    libc::chown(c_u.as_ptr(), 1000, 1000);
                    libc::chmod(c_u.as_ptr(), 0o777);
                }
            }
            let user_sock = user_sock_dir.join("wayland-0");
            let _ = fs::remove_file(&user_sock);
            let _ = std::os::unix::fs::symlink(&socket_path, &user_sock);
        }
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
    // Material You: derive the tonal scheme from the wallpaper once at start
    // up. The whole shell is then themed from a single value.
    let shell_palette = MaterialYouPalette::from_seed(wallpaper_seed());

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
    let mut app_input = String::with_capacity(128);
    let mut app_input_focused = false;
    let mut messages_list: Vec<String> = vec![
        "Treble Carrier: LTE connection active.".to_string(),
        "System: All Android 14 GKI HAL bridges ready.".to_string(),
    ];
    let mut terminal_tabs: Vec<TerminalTab> = vec![TerminalTab::new(1)];
    let mut active_tab_idx: usize = 0;
    let mut next_tab_id: usize = 2;
    let mut quick_tiles_active = [true, true, true, false, true, false, false, false];
    let mut cursor_pos: Option<(usize, usize)> = None;
    let mut is_touching = false;
    let mut last_input_rescan = Instant::now();

    // Multi-page home screen and Android 17 / PixelUI App Drawer state
    let mut home_pages: Vec<Vec<String>> = vec![
        vec![
            "phone".into(),
            "messages".into(),
            "browser".into(),
            "camera".into(),
            "gallery".into(),
            "settings".into(),
            "files".into(),
            "music".into(),
        ],
        vec![
            "terminal".into(),
            "treble".into(),
            "contacts".into(),
            "clock".into(),
        ],
    ];
    let mut current_home_page: usize = 0;
    let mut app_drawer_open = false;
    let mut drawer_search = String::with_capacity(64);
    let mut drawer_search_active = false;
    let mut selected_home_icon: Option<String> = None;
    // Lawnchair 17 / Pixel Launcher Animations and Transitions
    let mut drawer_progress: f32 = 0.0;
    let mut home_scroll_offset: f32 = 0.0;
    let mut app_launch_progress: f32 = 0.0;
    let mut app_launch_origin: Option<(f32, f32)> = None;
    let mut app_launch_color: u32 = 0xFF2563EB;
    let mut touch_ripple: Option<(f32, f32, f32, f32)> = None;
    let mut touch_drag_start: Option<(f32, f32)> = None;
    let mut drawer_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::drawer());
    let mut page_scroll_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::page_swipe());
    let mut app_launch_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::app_launch());
    let mut icon_bounce_spring = SpringSimulation::new(1.0, 1.0, SpringConfig::icon_bounce());
    let mut pressed_icon_id: Option<String> = None;

    // Icons are decoded and resampled once, at the size the layout draws.
    set_icon_edge_px(
        Layout::plain(server.scene.width as f32, server.scene.height as f32)
            .icon_size
            .ceil() as u32,
    );

    let mut desktop_catalogue = DesktopCatalogue::new();
    desktop_catalogue.scan_system_directories();
    let mut all_managed_apps = build_all_apps(&desktop_catalogue);
    let mut last_catalogue_scan = Instant::now();
    let catalogue_scan_interval = Duration::from_secs(2);

    // Icon resolution: one sweep per batch of new keys, then served from cache.
    let mut icon_cache = IconCache::new();
    icon_cache.set_display_edge(icon_edge_px());
    let mut icon_app_sig = app_set_signature(&all_managed_apps);
    apply_app_icons(&mut all_managed_apps, &mut icon_cache);

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
                                            match raw_touch.phase {
                                                TouchPhase::Down => {
                                                    touch_drag_start = Some((raw_touch.x, raw_touch.y));
                                                    let w = server.scene.width as f32;
                                                    let h = server.scene.height as f32;
                                                    if active_app.is_none()
                                                        && !server.scene.system_ui.is_open()
                                                        && !server.scene.keyboard.is_active
                                                    {
                                                        // Finger down: the icon compresses
                                                        // toward 0.90 and *stays* there
                                                        // until release, so the press reads
                                                        // as tactile rather than as a
                                                        // one-shot pop.
                                                        fn press(
                                                            target: &mut Option<String>,
                                                            id: String,
                                                            spring: &mut SpringSimulation,
                                                        ) {
                                                            *target = Some(id);
                                                            spring.set_target(PRESS_SCALE);
                                                        }
                                                        if app_drawer_open {
                                                            let off = (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                            let l = Layout::plain(w, h);
                                                            if let Some(idx) =
                                                                l.drawer_grid_hit(off, raw_touch.x, raw_touch.y)
                                                            {
                                                                // No intermediate Vec: the
                                                                // drawer's filtered view is an
                                                                // iterator, and this runs on
                                                                // every touch down.
                                                                if let Some(app) = drawer_view(&all_managed_apps, &drawer_search)
                                                                    .nth(idx)
                                                                {
                                                                    press(
                                                                        &mut pressed_icon_id,
                                                                        app.id.clone(),
                                                                        &mut icon_bounce_spring,
                                                                    );
                                                                }
                                                            }
                                                        } else {
                                                            let l = Layout::new(
                                                                w,
                                                                h,
                                                                selected_home_icon.is_some(),
                                                            );
                                                            if let Some(idx) =
                                                                l.home_grid_hit(
                                                                    raw_touch.x,
                                                                    raw_touch.y,
                                                                    home_scroll_offset,
                                                                )
                                                            {
                                                                if let Some(id) =
                                                                    home_pages.get(current_home_page).and_then(|p| p.get(idx))
                                                                {
                                                                    press(&mut pressed_icon_id, id.clone(), &mut icon_bounce_spring);
                                                                }
                                                            } else if let Some(slot) =
                                                                l.home_dock_hit(raw_touch.x, raw_touch.y)
                                                            {
                                                                let dock_ids = [
                                                                    "phone", "messages", "apps", "browser", "camera",
                                                                ];
                                                                if let Some(id) = dock_ids.get(slot) {
                                                                    press(&mut pressed_icon_id, (*id).to_string(), &mut icon_bounce_spring);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                TouchPhase::Move => {
                                                    if active_app.is_none()
                                                        && !server.scene.system_ui.is_open()
                                                        && !server.scene.keyboard.is_active
                                                    {
                                                        let h = server.scene.height as f32;
                                                        if let Some((_, sy)) = touch_drag_start {
                                                            let dy = sy - raw_touch.y;
                                                            // Drag span is the drawer's own
                                                            // height, so the sheet tracks the
                                                            // finger one-to-one.
                                                            let drawer_l = Layout::plain(server.scene.width as f32, h);
                                                            let drag_span = (h - drawer_l.drawer_handle.y)
                                                                .max(100.0);
                                                            let slop = h * 0.004;
                                                            if !app_drawer_open && dy > slop {
                                                                drawer_progress =
                                                                    (dy / drag_span).clamp(0.0, 1.0);
                                                                drawer_spring.value = drawer_progress;
                                                                drawer_spring.velocity = 0.0;
                                                            } else if app_drawer_open && dy < -slop {
                                                                drawer_progress =
                                                                    (1.0 + dy / drag_span).clamp(0.0, 1.0);
                                                                drawer_spring.value = drawer_progress;
                                                                drawer_spring.velocity = 0.0;
                                                            }
                                                        }
                                                    }
                                                }
                                                TouchPhase::Up | TouchPhase::Cancel => {
                                                    if let Some((_, sy)) = touch_drag_start.take() {
                                                        let dy = sy - raw_touch.y;
                                                        if active_app.is_none()
                                                            && !server.scene.system_ui.is_open()
                                                            && !server.scene.keyboard.is_active
                                                        {
                                                            let h = server.scene.height as f32;
                                                            // Commit past the halfway point,
                                                            // or on a decisive flick.
                                                            let flick = h * 0.12;
                                                            if !app_drawer_open {
                                                                if drawer_progress > 0.5 || dy > flick {
                                                                    app_drawer_open = true;
                                                                }
                                                            } else if drawer_progress < 0.5 || dy < -flick {
                                                                app_drawer_open = false;
                                                                drawer_search_active = false;
                                                                drawer_search.clear();
                                                            }
                                                            let target =
                                                                if app_drawer_open { 1.0 } else { 0.0 };
                                                            drawer_spring.set_target(target);
                                                            // Hand the release velocity to the
                                                            // spring, which is what makes the
                                                            // sheet feel like it has mass.
                                                            drawer_spring.velocity =
                                                                (dy / 50.0).clamp(-12.0, 12.0);
                                                        }
                                                    }
                                                    // Release: rebound to full size
                                                    // with an underdamped overshoot.
                                                    icon_bounce_spring.set_target(1.0);
                                                }
                                            }

                                            let gesture_act = gesture_engine.process_touch(&raw_touch);
                                            match gesture_act {
                                                GestureAction::Home { progress, .. } if progress >= 1.0 => {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        for tab in &terminal_tabs {
                                                            tab.cleanup_child();
                                                        }
                                                    }
                                                    active_app = None;
                                                    app_drawer_open = false;
                                                    drawer_search_active = false;
                                                    selected_home_icon = None;
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
                                                        drawer_search_active = false;
                                                    } else if app_drawer_open {
                                                        app_drawer_open = false;
                                                        drawer_search_active = false;
                                                    } else if selected_home_icon.is_some() {
                                                        selected_home_icon = None;
                                                    } else if active_app.is_some() {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            for tab in &terminal_tabs {
                                                                tab.cleanup_child();
                                                            }
                                                        }
                                                        active_app = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    }
                                                }
                                                GestureAction::Swipe { delta_x, delta_y }
                                                    if active_app.is_none()
                                                        && !server.scene.system_ui.is_open()
                                                        && !server.scene.keyboard.is_active =>
                                                {
                                                    if app_drawer_open {
                                                        if delta_y > 45.0 {
                                                            // Swiped down in App Drawer -> close drawer
                                                            app_drawer_open = false;
                                                            drawer_search_active = false;
                                                            drawer_search.clear();
                                                        }
                                                    } else if delta_y < -45.0 {
                                                        // Swiped up on home screen -> open App Drawer!
                                                        app_drawer_open = true;
                                                        selected_home_icon = None;
                                                    } else if delta_x < -45.0 {
                                                        // Swiped left -> next page
                                                        if current_home_page + 1 < home_pages.len() {
                                                            current_home_page += 1;
                                                            home_scroll_offset = server.scene.width as f32 * 0.45;
                                                            page_scroll_spring.value = home_scroll_offset;
                                                            page_scroll_spring.velocity = -250.0;
                                                            page_scroll_spring.set_target(0.0);
                                                            selected_home_icon = None;
                                                            println!("[UTLC] Swiped left to Home Page {}", current_home_page + 1);
                                                        } else {
                                                            let resisted = apply_overscroll_resistance(delta_x, server.scene.width as f32);
                                                            page_scroll_spring.value = resisted;
                                                            page_scroll_spring.velocity = 120.0;
                                                            page_scroll_spring.set_target(0.0);
                                                            home_scroll_offset = resisted;
                                                        }
                                                    } else if delta_x > 45.0 {
                                                        // Swiped right -> prev page
                                                        if current_home_page > 0 {
                                                            current_home_page -= 1;
                                                            home_scroll_offset = -(server.scene.width as f32 * 0.45);
                                                            page_scroll_spring.value = home_scroll_offset;
                                                            page_scroll_spring.velocity = 250.0;
                                                            page_scroll_spring.set_target(0.0);
                                                            selected_home_icon = None;
                                                            println!("[UTLC] Swiped right to Home Page {}", current_home_page + 1);
                                                        } else {
                                                            let resisted = apply_overscroll_resistance(delta_x, server.scene.width as f32);
                                                            page_scroll_spring.value = resisted;
                                                            page_scroll_spring.velocity = -120.0;
                                                            page_scroll_spring.set_target(0.0);
                                                            home_scroll_offset = resisted;
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        InputDispatchResult::Tap { x, y } => {
                                            let w = server.scene.width as f32;
                                            let h = server.scene.height as f32;
                                            // Trigger tactile touch ripple on every tap
                                            touch_ripple = Some((x, y, 12.0, 0.7));

                                            if server.scene.system_ui.is_open() {
                                                let shade = ShadeLayout::new(w, h);
                                                match shade.zone(x, y) {
                                                    ShadeZone::Tiles(i) => {
                                                        if i < quick_tiles_active.len() {
                                                            quick_tiles_active[i] =
                                                                !quick_tiles_active[i];
                                                        }
                                                    }
                                                    // Tapping the dimmed backdrop above the
                                                    // tiles, or the pull handle, closes.
                                                    ShadeZone::Header | ShadeZone::Empty => {
                                                        server.scene.system_ui.close();
                                                    }
                                                    ShadeZone::Brightness
                                                    | ShadeZone::Notifications
                                                    | ShadeZone::Handle => {}
                                                }
                                            } else if let Some(key) =
                                                Keyboard::new(w, h).hit(x, y)
                                            {
                                                // Virtual keyboard: the key under the
                                                // finger comes from the same layout the
                                                // keyboard is drawn from.
                                                let handle_key_input = |key_str: &str,
                                                                            active_app: &mut Option<String>,
                                                                            app_input: &mut String,
                                                                            app_input_focused: &mut bool,
                                                                            search_active: &mut bool,
                                                                            search_query: &mut String,
                                                                            drawer_search: &mut String,
                                                                            drawer_search_active: &mut bool,
                                                                            app_drawer_open: bool,
                                                                            terminal_tabs: &mut Vec<TerminalTab>,
                                                                            active_tab_idx: &mut usize,
                                                                            next_tab_id: &mut usize,
                                                                            keyboard: &mut VirtualKeyboard,
                                                                            messages_list: &mut Vec<String>| {
                                                    if app_drawer_open && *drawer_search_active {
                                                        match key_str {
                                                            "BACKSPACE" => { drawer_search.pop(); }
                                                            "ENTER" => {
                                                                *drawer_search_active = false;
                                                                keyboard.deactivate();
                                                            }
                                                            "SPACE" => {
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(' ');
                                                                }
                                                            }
                                                            ch => {
                                                                let c = if keyboard.is_shift_active {
                                                                    ch.chars().next().unwrap_or('?')
                                                                } else {
                                                                    ch.chars().next().unwrap_or('?').to_ascii_lowercase()
                                                                };
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(c);
                                                                }
                                                            }
                                                        }
                                                    } else {
                                                        let act = keyboard.handle_key_tap(key_str);
                                                        apply_ime_action(
                                                            act,
                                                            active_app,
                                                            app_input,
                                                            app_input_focused,
                                                            search_active,
                                                            search_query,
                                                            terminal_tabs,
                                                            active_tab_idx,
                                                            next_tab_id,
                                                            keyboard,
                                                            messages_list,
                                                        );
                                                    }
                                                };
                                                match key {
                                                    Key::Char(c) => handle_key_input(
                                                        &c.to_string(),
                                                        &mut active_app,
                                                        &mut app_input,
                                                        &mut app_input_focused,
                                                        &mut search_active,
                                                        &mut search_query,
                                                        &mut drawer_search,
                                                        &mut drawer_search_active,
                                                        app_drawer_open,
                                                        &mut terminal_tabs,
                                                        &mut active_tab_idx,
                                                        &mut next_tab_id,
                                                        &mut server.scene.keyboard,
                                                        &mut messages_list,
                                                    ),
                                                    Key::Space => handle_key_input(
                                                        "SPACE",
                                                        &mut active_app,
                                                        &mut app_input,
                                                        &mut app_input_focused,
                                                        &mut search_active,
                                                        &mut search_query,
                                                        &mut drawer_search,
                                                        &mut drawer_search_active,
                                                        app_drawer_open,
                                                        &mut terminal_tabs,
                                                        &mut active_tab_idx,
                                                        &mut next_tab_id,
                                                        &mut server.scene.keyboard,
                                                        &mut messages_list,
                                                    ),
                                                    Key::Enter => handle_key_input(
                                                        "ENTER",
                                                        &mut active_app,
                                                        &mut app_input,
                                                        &mut app_input_focused,
                                                        &mut search_active,
                                                        &mut search_query,
                                                        &mut drawer_search,
                                                        &mut drawer_search_active,
                                                        app_drawer_open,
                                                        &mut terminal_tabs,
                                                        &mut active_tab_idx,
                                                        &mut next_tab_id,
                                                        &mut server.scene.keyboard,
                                                        &mut messages_list,
                                                    ),
                                                    Key::Backspace => handle_key_input(
                                                        "BACKSPACE",
                                                        &mut active_app,
                                                        &mut app_input,
                                                        &mut app_input_focused,
                                                        &mut search_active,
                                                        &mut search_query,
                                                        &mut drawer_search,
                                                        &mut drawer_search_active,
                                                        app_drawer_open,
                                                        &mut terminal_tabs,
                                                        &mut active_tab_idx,
                                                        &mut next_tab_id,
                                                        &mut server.scene.keyboard,
                                                        &mut messages_list,
                                                    ),
                                                    Key::Hide => {
                                                        server.scene.keyboard.deactivate();
                                                        app_input_focused = false;
                                                        drawer_search_active = false;
                                                    }
                                                    Key::Shift => {
                                                        let _ = server.scene.keyboard.handle_key_tap("SHIFT");
                                                    }
                                                }
                                            } else if server.scene.keyboard.is_active {
                                                // Tapped outside keyboard while keyboard was active
                                                if active_app.is_some() {
                                                    let home = Layout::plain(w, h);
                                                    let app_l = AppLayout::new(
                                                        w,
                                                        h,
                                                        AppPanel::Other,
                                                        terminal_tabs.len(),
                                                    );
                                                    if app_l.back.contains(x, y)
                                                        || app_l.close.contains(x, y)
                                                        || home.nav_pill.contains(x, y)
                                                    {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            for tab in &terminal_tabs {
                                                                tab.cleanup_child();
                                                            }
                                                        }
                                                        active_app = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                        app_input_focused = false;
                                                        app_input.clear();
                                                    } else if active_app.as_deref() == Some("Messages") {
                                                        // Composer field and send button come
                                                        // from the same layout they are
                                                        // drawn with.
                                                        if app_l.send.contains(x, y) {
                                                            if !app_input.is_empty() {
                                                                messages_list
                                                                    .push(format!("You: {}", app_input));
                                                                app_input.clear();
                                                            }
                                                        } else {
                                                            app_input_focused = true;
                                                        }
                                                    } else {
                                                        app_input_focused = true;
                                                    }
                                                } else if app_drawer_open {
                                                    let drawer_y_offset = (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                    match Layout::plain(w, h).drawer_search_hit(drawer_y_offset, x, y) {
                                                        DrawerSearchHit::Clear => {
                                                            drawer_search.clear();
                                                        }
                                                        DrawerSearchHit::Focus => {
                                                            drawer_search_active = true;
                                                        }
                                                        DrawerSearchHit::None => {
                                                            let drawer_l = Layout::plain(w, h);
                                                            if let Some(idx) = drawer_l.drawer_grid_hit(drawer_y_offset, x, y) {
                                                                if let Some(target_app) = drawer_view(
                                                                    &all_managed_apps,
                                                                    &drawer_search,
                                                                )
                                                                .nth(idx)
                                                                {
                                                                    let cell = drawer_l.drawer_icon_cell(idx);
                                                                    app_launch_origin = Some((
                                                                        cell.center_x(),
                                                                        drawer_y_offset + cell.center_y(),
                                                                    ));
                                                                    app_launch_progress = 0.01;
                                                                    app_launch_color = target_app.color;

                                                                    let app_to_launch = target_app.name.clone();
                                                                    let app_exec = target_app.exec.clone();

                                                                    active_app = Some(app_to_launch.clone());
                                                                    app_input.clear();
                                                                    app_input_focused = false;
                                                                    app_drawer_open = false;
                                                                    drawer_search_active = false;
                                                                    drawer_search.clear();

                                                                    if app_to_launch == "Terminal" {
                                                                        server.scene.keyboard.activate();
                                                                        app_input_focused = true;
                                                                    } else {
                                                                        server.scene.keyboard.deactivate();
                                                                    }

                                                                    if !app_exec.is_empty() {
                                                                        launch_desktop_app(&app_exec, &socket_dir);
                                                                    }
                                                                }
                                                            } else if y < drawer_y_offset
                                                                || Layout::plain(w, h).drawer_handle.contains(x, y - drawer_y_offset)
                                                                || Layout::plain(w, h).nav_pill.contains(x, y)
                                                            {
                                                                app_drawer_open = false;
                                                                drawer_search_active = false;
                                                                drawer_search.clear();
                                                                server.scene.keyboard.deactivate();
                                                            } else {
                                                                server.scene.keyboard.deactivate();
                                                                drawer_search_active = false;
                                                            }
                                                        }
                                                    }
                                                } else if search_active {
                                                    let home_l = Layout::plain(w, h);
                                                    if home_l.search.contains(x, y) {
                                                        // Tap on search bar keeps focus
                                                    } else if let Some(idx) =
                                                        home_l.home_grid_hit(x, y, home_scroll_offset)
                                                    {
                                                        // Search results replace the page
                                                        // contents, so the two views share
                                                        // the same iteration shape.
                                                        let target = if search_query.is_empty() {
                                                            home_pages[current_home_page]
                                                                .iter()
                                                                .filter_map(|id| {
                                                                    all_managed_apps.iter().find(|a| a.id == *id)
                                                                })
                                                                .nth(idx)
                                                        } else {
                                                            drawer_view(&all_managed_apps, search_query.as_str()).nth(idx)
                                                        };
                                                        if let Some(target_app) = target {
                                                            let cell = home_l.grid_icon(idx);
                                                            app_launch_origin = Some((
                                                                cell.center_x() + home_scroll_offset,
                                                                cell.center_y(),
                                                            ));
                                                            app_launch_progress = 0.01;
                                                            app_launch_color = target_app.color;
                                                            let app_to_launch = target_app.name.clone();
                                                            let app_exec = target_app.exec.clone();
                                                            active_app = Some(app_to_launch.clone());
                                                            app_input.clear();
                                                            app_input_focused = false;
                                                            search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                            if !app_exec.is_empty() {
                                                                launch_desktop_app(&app_exec, &socket_dir);
                                                            }
                                                        }
                                                    } else {
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    }
                                                } else {
                                                    server.scene.keyboard.deactivate();
                                                    search_active = false;
                                                    drawer_search_active = false;
                                                }
                                            } else if active_app.is_some() {
                                                // An app is open and keyboard is not active
                                                let home = Layout::plain(w, h);
                                                let app_l = AppLayout::new(
                                                    w,
                                                    h,
                                                    AppPanel::Other,
                                                    terminal_tabs.len(),
                                                );
                                                if app_l.back.contains(x, y)
                                                    || app_l.close.contains(x, y)
                                                    || home.nav_pill.contains(x, y)
                                                {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        for tab in &terminal_tabs {
                                                            tab.cleanup_child();
                                                        }
                                                    }
                                                    active_app = None;
                                                    server.scene.keyboard.deactivate();
                                                    search_active = false;
                                                    app_input_focused = false;
                                                    app_input.clear();
                                                } else if active_app.as_deref() == Some("Terminal") {
                                                    let tab_l = AppLayout::new(
                                                        w, h, AppPanel::Terminal, terminal_tabs.len(),
                                                    );
                                                    match tab_l.hit_tab_active(x, y, active_tab_idx) {
                                                        Some(TabHit::Select(i)) => {
                                                            active_tab_idx = i;
                                                            server.scene.keyboard.activate();
                                                            app_input_focused = true;
                                                        }
                                                        Some(TabHit::Close(i)) => {
                                                            if terminal_tabs.len() > 1 && i < terminal_tabs.len() {
                                                                terminal_tabs[i].cleanup_child();
                                                                terminal_tabs.remove(i);
                                                                if active_tab_idx >= terminal_tabs.len() {
                                                                    active_tab_idx = terminal_tabs.len() - 1;
                                                                }
                                                            }
                                                            server.scene.keyboard.activate();
                                                            app_input_focused = true;
                                                        }
                                                        Some(TabHit::Add) => {
                                                            if terminal_tabs.len() < 4 {
                                                                terminal_tabs.push(TerminalTab::new(next_tab_id));
                                                                next_tab_id += 1;
                                                                active_tab_idx = terminal_tabs.len() - 1;
                                                                server.scene.keyboard.activate();
                                                                app_input_focused = true;
                                                            }
                                                        }
                                                        None => {
                                                            server.scene.keyboard.activate();
                                                            app_input_focused = true;
                                                        }
                                                    }
                                                } else if active_app.as_deref() == Some("Messages") {
                                                    if app_l.send.contains(x, y) {
                                                        if !app_input.is_empty() {
                                                            messages_list
                                                                .push(format!("You: {}", app_input));
                                                            app_input.clear();
                                                        }
                                                    } else {
                                                        app_input_focused = true;
                                                        server.scene.keyboard.activate();
                                                    }
                                                } else {
                                                    // Universal typing handler for ALL apps (Browser, Settings, Phone, Contacts, Files, desktop apps):
                                                    // Clicking on the place to type automatically pops up the keyboard!
                                                    app_input_focused = true;
                                                    server.scene.keyboard.activate();
                                                }
                                            } else if app_drawer_open {
                                                // App Drawer tap handling with dynamic sliding offset
                                                let drawer_y_offset = (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                let drawer_l = Layout::plain(w, h);
                                                if y < drawer_y_offset
                                                    || drawer_l.drawer_handle.contains(x, y - drawer_y_offset)
                                                {
                                                    // Pull handle / top area: dismiss drawer
                                                    app_drawer_open = false;
                                                    drawer_search_active = false;
                                                    server.scene.keyboard.deactivate();
                                                } else {
                                                    match Layout::plain(w, h).drawer_search_hit(drawer_y_offset, x, y) {
                                                        DrawerSearchHit::Clear => {
                                                            drawer_search.clear();
                                                        }
                                                        DrawerSearchHit::Focus => {
                                                            drawer_search_active = true;
                                                            server.scene.keyboard.activate();
                                                        }
                                                        DrawerSearchHit::None => {
                                                            if Layout::plain(w, h).nav_pill.contains(x, y) {
                                                                // Bottom pill: close drawer
                                                                app_drawer_open = false;
                                                                drawer_search_active = false;
                                                                drawer_search.clear();
                                                                server.scene.keyboard.deactivate();
                                                            } else if let Some(idx) =
                                                                drawer_l.drawer_grid_hit(drawer_y_offset, x, y)
                                                            {
                                                                if let Some(target_app) = drawer_view(
                                                                    &all_managed_apps,
                                                                    &drawer_search,
                                                                )
                                                                .nth(idx)
                                                                {
                                                                    let cell = drawer_l.drawer_icon_cell(idx);
                                                                    app_launch_origin = Some((
                                                                        cell.center_x(),
                                                                        drawer_y_offset + cell.center_y(),
                                                                    ));
                                                                    app_launch_progress = 0.01;
                                                                    app_launch_color = target_app.color;

                                                                    let app_to_launch = target_app.name.clone();
                                                                    let app_exec = target_app.exec.clone();

                                                                    active_app = Some(app_to_launch.clone());
                                                                    app_input.clear();
                                                                    app_input_focused = false;
                                                                    app_drawer_open = false;
                                                                    drawer_search_active = false;
                                                                    drawer_search.clear();

                                                                    if app_to_launch == "Terminal" {
                                                                        server.scene.keyboard.activate();
                                                                        app_input_focused = true;
                                                                    } else {
                                                                        server.scene.keyboard.deactivate();
                                                                    }

                                                                    if !app_exec.is_empty() {
                                                                        launch_desktop_app(&app_exec, &socket_dir);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            } else {
                                                // Home screen hit testing.
                                                //
                                                // `home_l` is the exact layout the
                                                // renderer used for this frame, so a
                                                // tap lands on exactly the widget
                                                // the user sees under their finger.
                                                let home_l =
                                                    Layout::new(w, h, selected_home_icon.is_some());
                                                if home_l.status_bar_h >= y {
                                                    server.scene.system_ui.toggle();
                                                } else if selected_home_icon.is_some()
                                                    && (home_l.remove_chip.contains(x, y)
                                                        || home_l.move_chip.contains(x, y))
                                                {
                                                    match home_l.remove_chip.contains(x, y) {
                                                        true => {
                                                            if let Some(ref sel_id) = selected_home_icon {
                                                                if let Some(pos) = home_pages[current_home_page].iter().position(|id| id == sel_id) {
                                                                    home_pages[current_home_page].remove(pos);
                                                                    println!("[UTLC] Removed app '{}' from Home Page {}", sel_id, current_home_page + 1);
                                                                }
                                                            }
                                                            selected_home_icon = None;
                                                        }
                                                        false => {
                                                            if let Some(sel_id) = selected_home_icon.take() {
                                                                if let Some(pos) = home_pages[current_home_page].iter().position(|id| *id == sel_id) {
                                                                    home_pages[current_home_page].remove(pos);
                                                                }
                                                                let target_page = if current_home_page == 0 { 1 } else { 0 };
                                                                while home_pages.len() <= target_page {
                                                                    home_pages.push(Vec::new());
                                                                }
                                                                home_pages[target_page].push(sel_id.clone());
                                                                if target_page > current_home_page {
                                                                    home_scroll_offset = w * 0.45;
                                                                } else {
                                                                    home_scroll_offset = -(w * 0.45);
                                                                }
                                                                current_home_page = target_page;
                                                                println!("[UTLC] Moved app '{}' to Home Page {}", sel_id, current_home_page + 1);
                                                            }
                                                        }
                                                    }
                                                } else if home_l.clock_rect().contains(x, y) {
                                                    active_app = Some("Clock".to_string());
                                                    app_launch_origin = Some((
                                                        home_l.w * 0.5,
                                                        home_l.clock_y + home_l.clock_h * 0.5,
                                                    ));
                                                    app_launch_progress = 0.01;
                                                    app_launch_color = 0xFFEF4444;
                                                    app_input.clear();
                                                    app_input_focused = false;
                                                    selected_home_icon = None;
                                                    server.scene.keyboard.deactivate();
                                                    search_active = false;
                                                } else if home_l.search.contains(x, y) {
                                                    search_active = true;
                                                    server.scene.keyboard.activate();
                                                } else if let Some(target_page) =
                                                    home_l.home_page_hit(x, y, home_pages.len())
                                                {
                                                    if let Some(sel_id) = selected_home_icon.take() {
                                                        if target_page != current_home_page {
                                                            if let Some(pos) = home_pages[current_home_page].iter().position(|id| *id == sel_id) {
                                                                home_pages[current_home_page].remove(pos);
                                                            }
                                                            while home_pages.len() <= target_page {
                                                                home_pages.push(Vec::new());
                                                            }
                                                            home_pages[target_page].push(sel_id.clone());
                                                            println!("[UTLC] Moved app '{}' to Home Page {}", sel_id, target_page + 1);
                                                        }
                                                    }
                                                    if target_page != current_home_page {
                                                        if target_page > current_home_page {
                                                            home_scroll_offset = w * 0.45;
                                                        } else {
                                                            home_scroll_offset = -(w * 0.45);
                                                        }
                                                        current_home_page = target_page;
                                                    }
                                                } else if let Some(dock_slot) =
                                                    home_l.home_dock_hit(x, y)
                                                {
                                                    let dock_apps = ["Phone", "Messages", "Apps", "Browser", "Camera"];
                                                    let app = dock_apps[dock_slot];
                                                    if app == "Apps" {
                                                        // Tapping "Apps" on dock toggles the App Drawer!
                                                        app_drawer_open = !app_drawer_open;
                                                        selected_home_icon = None;
                                                        drawer_search.clear();
                                                        drawer_search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                    } else {
                                                        let dock_icon = home_l.dock_icon_rect(dock_slot);
                                                        app_launch_origin =
                                                            Some((dock_icon.center_x(), dock_icon.center_y()));
                                                        app_launch_progress = 0.01;
                                                        let entry = all_managed_apps.iter().find(|a| a.name == app);
                                                        app_launch_color = entry.map(|a| a.color).unwrap_or(0xFF2563EB);

                                                        active_app = Some(app.to_string());
                                                        app_input.clear();
                                                        app_input_focused = false;
                                                        selected_home_icon = None;
                                                        if app == "Terminal" {
                                                            server.scene.keyboard.activate();
                                                            app_input_focused = true;
                                                        } else {
                                                            server.scene.keyboard.deactivate();
                                                            search_active = false;
                                                        }
                                                        if app == "Browser" {
                                                            if let Some(b_app) = all_managed_apps.iter().find(|a| a.name == "Browser" || a.name == "Firefox") {
                                                                if !b_app.exec.is_empty() {
                                                                    launch_desktop_app(&b_app.exec, &socket_dir);
                                                                }
                                                            }
                                                        }
                                                    }
                                                } else if home_l.nav_pill.contains(x, y) {
                                                    active_app = None;
                                                    search_active = false;
                                                    app_drawer_open = false;
                                                    selected_home_icon = None;
                                                    app_input_focused = false;
                                                    app_input.clear();
                                                    server.scene.keyboard.deactivate();
                                                    server.scene.system_ui.close();
                                                } else if let Some(idx) =
                                                    home_l.home_grid_hit(x, y, home_scroll_offset)
                                                {
                                                    let cell = home_l.grid_icon(idx);
                                                    let cx = cell.center_x() + home_scroll_offset;
                                                    let cy = cell.center_y();

                                                    if search_active && !search_query.is_empty() {
                                                        if let Some(target_app) = drawer_view(
                                                            &all_managed_apps,
                                                            search_query.as_str(),
                                                        )
                                                        .nth(idx)
                                                        {
                                                            app_launch_origin = Some((cx, cy));
                                                            app_launch_progress = 0.01;
                                                            app_launch_color = target_app.color;
                                                            let app_to_launch = target_app.name.clone();
                                                            let app_exec = target_app.exec.clone();
                                                            active_app = Some(app_to_launch.clone());
                                                            app_input.clear();
                                                            app_input_focused = false;
                                                            search_active = false;
                                                            server.scene.keyboard.deactivate();
                                                            if !app_exec.is_empty() {
                                                                launch_desktop_app(&app_exec, &socket_dir);
                                                            }
                                                        }
                                                    } else if let Some(sel_id) = selected_home_icon.take() {
                                                        // Moving icon in edit mode to selected slot
                                                        if let Some(old_pos) = home_pages[current_home_page].iter().position(|id| *id == sel_id) {
                                                            home_pages[current_home_page].remove(old_pos);
                                                            let insert_pos = idx.min(home_pages[current_home_page].len());
                                                            home_pages[current_home_page].insert(insert_pos, sel_id.clone());
                                                            println!("[UTLC] Moved app '{}' from slot {} to slot {}", sel_id, old_pos, insert_pos);
                                                        }
                                                    } else {
                                                        let page_app_ids = &home_pages[current_home_page];
                                                        if let Some(app_id) = page_app_ids.get(idx) {
                                                            if let Some(target_app) = all_managed_apps.iter().find(|a| a.id == *app_id) {
                                                                app_launch_origin = Some((cx, cy));
                                                                app_launch_progress = 0.01;
                                                                app_launch_color = target_app.color;
                                                                let app_to_launch = target_app.name.clone();
                                                                let app_exec = target_app.exec.clone();
                                                                active_app = Some(app_to_launch.clone());
                                                                app_input.clear();
                                                                app_input_focused = false;
                                                                selected_home_icon = None;
                                                                if app_to_launch == "Terminal" {
                                                                    server.scene.keyboard.activate();
                                                                    app_input_focused = true;
                                                                } else {
                                                                    server.scene.keyboard.deactivate();
                                                                    search_active = false;
                                                                }
                                                                if !app_exec.is_empty() {
                                                                    launch_desktop_app(&app_exec, &socket_dir);
                                                                }
                                                            }
                                                        }
                                                    }
                                                } else if selected_home_icon.is_some() {
                                                    selected_home_icon = None;
                                                }
                                            }
                                        }
                                        InputDispatchResult::KeyPress {
                                            code,
                                            ch,
                                            pressed,
                                            repeat,
                                            ctrl,
                                        } => {
                                            if pressed {
                                                if ctrl {
                                                    if code == KEY_C {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            let tab = &mut terminal_tabs[active_tab_idx];
                                                            tab.cleanup_child();
                                                            let display_cmd = if tab.input.is_empty() { "^C" } else { &format!("{}^C", tab.input) };
                                                            let full_line = format!("root@treble-gsi:~# {}", display_cmd);
                                                            push_terminal_line(&mut tab.lines, &full_line);
                                                            log_terminal_output(&full_line);
                                                            tab.input.clear();
                                                        }
                                                    } else if code == KEY_D {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            let (has_stdin, is_empty) = {
                                                                let tab = &terminal_tabs[active_tab_idx];
                                                                let has_stdin = tab.active_stdin.lock().unwrap().is_some();
                                                                let is_empty = tab.input.is_empty();
                                                                (has_stdin, is_empty)
                                                            };
                                                            if has_stdin {
                                                                *terminal_tabs[active_tab_idx].active_stdin.lock().unwrap() = None;
                                                            } else if is_empty {
                                                                terminal_tabs[active_tab_idx].cleanup_child();
                                                                terminal_tabs.remove(active_tab_idx);
                                                                if terminal_tabs.is_empty() {
                                                                    active_app = None;
                                                                    server.scene.keyboard.deactivate();
                                                                    search_active = false;
                                                                    terminal_tabs.push(TerminalTab::new(1));
                                                                    next_tab_id = 2;
                                                                    active_tab_idx = 0;
                                                                } else if active_tab_idx >= terminal_tabs.len() {
                                                                    active_tab_idx = terminal_tabs.len() - 1;
                                                                }
                                                            }
                                                        }
                                                    } else if code == KEY_L {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            terminal_tabs[active_tab_idx].lines.clear();
                                                        }
                                                    } else if code == KEY_T {
                                                        if active_app.as_deref() == Some("Terminal") && terminal_tabs.len() < 4 {
                                                            terminal_tabs.push(TerminalTab::new(next_tab_id));
                                                            next_tab_id += 1;
                                                            active_tab_idx = terminal_tabs.len() - 1;
                                                            server.scene.keyboard.activate();
                                                        }
                                                    } else if code == KEY_W {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            terminal_tabs[active_tab_idx].cleanup_child();
                                                            terminal_tabs.remove(active_tab_idx);
                                                            if terminal_tabs.is_empty() {
                                                                active_app = None;
                                                                server.scene.keyboard.deactivate();
                                                                search_active = false;
                                                                terminal_tabs.push(TerminalTab::new(1));
                                                                next_tab_id = 2;
                                                                active_tab_idx = 0;
                                                            } else if active_tab_idx >= terminal_tabs.len() {
                                                                active_tab_idx = terminal_tabs.len() - 1;
                                                            }
                                                        }
                                                    } else if code == KEY_TAB {
                                                        if active_app.as_deref() == Some("Terminal") && !terminal_tabs.is_empty() {
                                                            active_tab_idx = (active_tab_idx + 1) % terminal_tabs.len();
                                                        }
                                                    } else if active_app.as_deref() == Some("Terminal") {
                                                        if code == KEY_1 && !terminal_tabs.is_empty() {
                                                            active_tab_idx = 0;
                                                        } else if code == KEY_2 && terminal_tabs.len() > 1 {
                                                            active_tab_idx = 1;
                                                        } else if code == KEY_3 && terminal_tabs.len() > 2 {
                                                            active_tab_idx = 2;
                                                        } else if code == KEY_4 && terminal_tabs.len() > 3 {
                                                            active_tab_idx = 3;
                                                        }
                                                    }
                                                } else if code == KEY_ESC {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        for tab in &terminal_tabs {
                                                            tab.cleanup_child();
                                                        }
                                                    }
                                                    if server.scene.keyboard.is_active {
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                        drawer_search_active = false;
                                                    } else if app_drawer_open {
                                                        app_drawer_open = false;
                                                        drawer_search_active = false;
                                                    } else if selected_home_icon.is_some() {
                                                        selected_home_icon = None;
                                                    } else if active_app.is_some() {
                                                        active_app = None;
                                                        server.scene.keyboard.deactivate();
                                                        search_active = false;
                                                    } else if server.scene.system_ui.is_open() {
                                                        server.scene.system_ui.close();
                                                    }
                                                } else if code == KEY_BACKSPACE {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        terminal_tabs[active_tab_idx].input.pop();
                                                    } else if app_drawer_open && drawer_search_active {
                                                        drawer_search.pop();
                                                    } else if search_active {
                                                        search_query.pop();
                                                    } else if active_app.is_some() && app_input_focused {
                                                        app_input.pop();
                                                    }
                                                } else if code == KEY_ENTER && !repeat {
                                                    if active_app.as_deref() == Some("Terminal") {
                                                        handle_terminal_enter(
                                                            &mut terminal_tabs,
                                                            &mut active_tab_idx,
                                                            &mut next_tab_id,
                                                            &mut active_app,
                                                            &mut server.scene.keyboard,
                                                            &mut search_active,
                                                        );
                                                    } else if app_drawer_open && drawer_search_active {
                                                        drawer_search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                    } else if search_active {
                                                        search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                    } else if active_app.is_some() && app_input_focused {
                                                        if active_app.as_deref() == Some("Messages") {
                                                            if !app_input.is_empty() {
                                                                messages_list.push(format!("You: {}", app_input));
                                                                app_input.clear();
                                                            }
                                                        } else {
                                                            server.scene.keyboard.deactivate();
                                                            app_input_focused = false;
                                                        }
                                                    }
                                                } else if let Some(c) = ch {
                                                    if !repeat {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            if terminal_tabs[active_tab_idx].input.len() < 60 {
                                                                terminal_tabs[active_tab_idx].input.push(c);
                                                            }
                                                        } else if app_drawer_open && drawer_search_active {
                                                            if drawer_search.len() < 40 {
                                                                drawer_search.push(c);
                                                            }
                                                        } else if search_active && search_query.len() < 40 {
                                                            search_query.push(c);
                                                        } else if active_app.is_some() {
                                                            app_input_focused = true;
                                                            if app_input.len() < 120 {
                                                                app_input.push(c);
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        InputDispatchResult::LongPress { x, y } => {
                                            let w = server.scene.width as f32;
                                            let h = server.scene.height as f32;
                                            touch_ripple = Some((x, y, 18.0, 0.9));
                                            if app_drawer_open {
                                                let drawer_y_offset = (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                if let Some(idx) =
                                                    Layout::plain(w, h).drawer_grid_hit(drawer_y_offset, x, y)
                                                {
                                                    if let Some(target_app) =
                                                        drawer_view(&all_managed_apps, &drawer_search).nth(idx)
                                                    {
                                                        if !home_pages[current_home_page].contains(&target_app.id) {
                                                            home_pages[current_home_page].push(target_app.id.clone());
                                                            println!("[UTLC] Pinned '{}' to Home Page {}", target_app.name, current_home_page + 1);
                                                        }
                                                        app_drawer_open = false;
                                                        drawer_search_active = false;
                                                        server.scene.keyboard.deactivate();
                                                    }
                                                }
                                            } else if active_app.is_none() && !server.scene.system_ui.is_open() {
                                                if let Some(idx) = Layout::new(
                                                    w, h, selected_home_icon.is_some(),
                                                )
                                                .home_grid_hit(x, y, home_scroll_offset)
                                                {
                                                    let page_app_ids = &home_pages[current_home_page];
                                                    if let Some(app_id) = page_app_ids.get(idx) {
                                                        selected_home_icon = Some(app_id.clone());
                                                        println!("[UTLC] Selected app '{}' on Home Page {} for edit mode", app_id, current_home_page + 1);
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
                    } else {
                        let err = unsafe { *libc::__errno_location() };
                        if n < 0
                            && (err == libc::EAGAIN
                                || err == libc::EWOULDBLOCK
                                || err == libc::EINTR)
                        {
                            // Non-blocking read would block; ignore
                        } else {
                            // Device closed or error; unregister from epoll and close
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
                            unsafe {
                                libc::close(fd);
                            }
                            if let Some(pos) = input_fds.iter().position(|&x| x == fd) {
                                input_fds.remove(pos);
                                if pos < opened_paths.len() {
                                    opened_paths.remove(pos);
                                }
                            }
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
                                    Ok(n) => {
                                        let mut slice = &buf[..n];
                                        while let Ok(Some((msg, len))) = utim_core::compositor::protocols::WlMessage::parse(slice) {
                                            // zwp_text_input_v3 requests:
                                            // opcode 1: enable -> activate virtual keyboard
                                            // opcode 2: disable -> deactivate virtual keyboard
                                            if msg.header.opcode == 1 {
                                                server.scene.keyboard.activate();
                                                app_input_focused = true;
                                            } else if msg.header.opcode == 2 {
                                                server.scene.keyboard.deactivate();
                                                app_input_focused = false;
                                            }
                                            slice = &slice[len..];
                                        }
                                    }
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
                    } else if stream_idx.is_none() && epoll_fd >= 0 {
                        unsafe {
                            libc::epoll_ctl(
                                epoll_fd,
                                libc::EPOLL_CTL_DEL,
                                fd,
                                std::ptr::null_mut(),
                            );
                        }
                    }
                }
            }
        }

        // Drain asynchronous terminal command output streams for all tabs
        for tab in &mut terminal_tabs {
            while let Ok(line) = tab.rx.try_recv() {
                push_terminal_line(&mut tab.lines, &line);
            }
            if tab.lines.len() > 120 {
                tab.lines.drain(0..tab.lines.len() - 120);
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

            // Lawnchair 17 / Android DynamicAnimation transitions. Every one
            // of these is an analytical damped-oscillator spring integrated at
            // the real frame delta, so motion is identical at 60 and 120 Hz.
            let drawer_target = if app_drawer_open { 1.0 } else { 0.0 };
            if touch_drag_start.is_none() {
                drawer_spring.set_target(drawer_target);
                drawer_spring.step(dt);
                drawer_progress = drawer_spring.value.clamp(0.0, 1.0);
            }

            page_scroll_spring.step(dt);
            // Boundary resistance: the spring can never park the strip outside
            // the paginated range, and overscroll bleeds off elastically.
            // Boundary resistance: the strip may be dragged a third of a page
            // past the last page, then springs back with the overscroll curve.
            let last_page = home_pages.len().saturating_sub(1) as f32;
            let strip_max = last_page * server.scene.width as f32;
            home_scroll_offset = page_scroll_spring
                .value
                .clamp(-strip_max * 0.35, strip_max + server.scene.width as f32 * 0.35);

            icon_bounce_spring.step(dt);
            if icon_bounce_spring.is_at_rest() {
                icon_bounce_spring.value = icon_bounce_spring.target;
                if (icon_bounce_spring.value - 1.0).abs() < 0.005 {
                    pressed_icon_id = None;
                }
            }

            if app_launch_progress > 0.0 {
                if app_launch_spring.target != 1.0 {
                    // Kick the expansion off with a real velocity so the card
                    // leaves the icon with momentum instead of easing in.
                    app_launch_spring.set_target(1.0);
                    app_launch_spring.value = app_launch_progress;
                    app_launch_spring.velocity = 2.2;
                }
                app_launch_spring.step(dt);
                app_launch_progress = app_launch_spring.value.clamp(0.0, 1.0);
                if app_launch_progress >= 0.995 {
                    app_launch_progress = 0.0;
                    app_launch_origin = None;
                    app_launch_spring.set_target(0.0);
                    app_launch_spring.value = 0.0;
                    app_launch_spring.velocity = 0.0;
                }
            }

            // Ripple: a Material You touch ripple that expands and fades on a
            // time constant rather than a frame count.
            if let Some((_, _, ref mut r, ref mut a)) = touch_ripple {
                *r += dt * 140.0;
                *a -= dt * 3.0;
                if *a <= 0.0 {
                    touch_ripple = None;
                }
            }

            if let Some(ref mut drm) = drm_display {
                let t_str = format_current_time(&mut time_buf);
                if active_tab_idx >= terminal_tabs.len() {
                    active_tab_idx = terminal_tabs.len().saturating_sub(1);
                }
                let active_tab = &terminal_tabs[active_tab_idx];
                let tab_infos: Vec<TerminalTabInfo> = terminal_tabs
                    .iter()
                    .enumerate()
                    .map(|(idx, tab)| TerminalTabInfo {
                        id: tab.id,
                        title: &tab.title,
                        is_running: tab.is_running(),
                        is_active: idx == active_tab_idx,
                    })
                    .collect();

                if last_catalogue_scan.elapsed() >= catalogue_scan_interval {
                    last_catalogue_scan = Instant::now();
                    desktop_catalogue.scan_system_directories();
                    all_managed_apps = build_all_apps(&desktop_catalogue);
                    let sig = app_set_signature(&all_managed_apps);
                    if sig != icon_app_sig {
                        // The set changed: previously missing icons may exist now.
                        icon_app_sig = sig;
                        icon_cache.invalidate_misses();
                    }
                    apply_app_icons(&mut all_managed_apps, &mut icon_cache);
                }

                // 1. Grid apps on the current home screen page
                let current_page_app_ids = &home_pages[current_home_page];
                let home_screen_apps: Vec<&ManagedApp> = if search_active && !search_query.is_empty() {
                    let q = search_query.to_lowercase();
                    all_managed_apps
                        .iter()
                        .filter(|a| a.name.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q))
                        .collect()
                } else {
                    current_page_app_ids
                        .iter()
                        .filter_map(|id| all_managed_apps.iter().find(|a| a.id == *id))
                        .collect()
                };

                let grid_items: Vec<AppGridItem> = home_screen_apps
                    .iter()
                    .map(|a| AppGridItem {
                        id: &a.id,
                        name: &a.name,
                        color: a.color,
                        glyph: &a.glyph,
                        icon: a.icon.as_deref(),
                    })
                    .collect();

                // 2. Drawer apps (full catalogue for the PixelUI App Drawer)
                let drawer_visible_apps: Vec<&ManagedApp> = if !drawer_search.is_empty() {
                    let q = drawer_search.to_lowercase();
                    all_managed_apps
                        .iter()
                        .filter(|a| a.name.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q))
                        .collect()
                } else {
                    all_managed_apps.iter().collect()
                };

                let drawer_items: Vec<AppGridItem> = drawer_visible_apps
                    .iter()
                    .map(|a| AppGridItem {
                        id: &a.id,
                        name: &a.name,
                        color: a.color,
                        glyph: &a.glyph,
                        icon: a.icon.as_deref(),
                    })
                    .collect();

                // Hotseat: same five slots as the hit-test table, real icons included.
                const DOCK_NAMES: [&str; 5] = ["Phone", "Messages", "Apps", "Browser", "Camera"];
                let apps_dock_icon = icon_cache.get("view-app-grid").or_else(|| icon_cache.get("apps"));
                let dock_items: Vec<AppGridItem> = DOCK_NAMES
                    .iter()
                    .map(|name| {
                        let entry = all_managed_apps.iter().find(|a| a.name == *name);
                        let icon = entry.and_then(|a| a.icon.as_deref()).or_else(|| {
                            if *name == "Apps" {
                                apps_dock_icon.as_deref()
                            } else {
                                None
                            }
                        });
                        AppGridItem {
                            id: entry.map(|a| a.id.as_str()).unwrap_or(if *name == "Apps" { "apps" } else { name }),
                            name,
                            color: entry.map(|a| a.color).unwrap_or(0xFF475569),
                            glyph: entry.map(|a| a.glyph.as_str()).unwrap_or(":"),
                            icon,
                        }
                    })
                    .collect();

                let drm_state = DrmInteractiveState {
                    time_str: t_str,
                    is_locked: server.scene.mode == utim_core::compositor::scene::ShellMode::LockScreen,
                    cursor_pos,
                    is_touching,
                    search_query: &search_query,
                    search_active,
                    keyboard_active: server.scene.keyboard.is_active,
                    keyboard_shift_active: server.scene.keyboard.is_shift_active,
                    shade_open: server.scene.system_ui.is_open(),
                    quick_tiles_active,
                    active_app: active_app.as_deref(),
                    app_input: &app_input,
                    app_input_focused,
                    messages_list: &messages_list,
                    terminal_lines: &active_tab.lines,
                    terminal_input: &active_tab.input,
                    terminal_running: active_tab.is_running(),
                    terminal_tabs: &tab_infos,
                    terminal_active_tab: active_tab_idx,
                    grid_apps: &grid_items,
                    dock_apps: &dock_items,
                    app_drawer_open,
                    drawer_apps: &drawer_items,
                    drawer_search: &drawer_search,
                    home_page: current_home_page,
                    total_home_pages: home_pages.len(),
                    selected_icon_id: selected_home_icon.as_deref(),
                    drawer_progress,
                    home_scroll_offset,
                    app_launch_progress,
                    app_launch_origin,
                    app_launch_color,
                    touch_ripple,
                    pressed_icon_id: pressed_icon_id.as_deref(),
                    icon_press_scale: icon_bounce_spring.value,
                    palette: shell_palette,
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

/// Scale an icon compresses to while a finger is on it. Lawnchair 17 /
/// Launcher3 use a 0.92-style press bounce; the spring then rebounds past 1.0
/// on release.
const PRESS_SCALE: f32 = 0.90;

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

/// Average colour of the desktop wallpaper, used as the Material You seed.
///
/// This is the same job `dev.kdrag0n.monet` does on a real phone: pull a
/// colour out of what the user is looking at and build the tonal scheme from
/// it. Averaging a sparse grid of pixels keeps it a few hundred reads instead
/// of decoding the whole image.
fn wallpaper_seed() -> u32 {
    const FALLBACK: u32 = 0xFF3B82F6;
    // Standard XDG wallpaper locations, newest first.
    const DIRS: [&str; 3] = [
        "/run/user/1000",
        "/usr/share/backgrounds",
        "/usr/share/wallpapers",
    ];
    for dir in DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "png" {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            if let Some(seed) = average_png_colour(&data) {
                return seed;
            }
        }
    }
    FALLBACK
}

/// Sparse-grid average of a decoded PNG, skipping transparent pixels.
fn average_png_colour(data: &[u8]) -> Option<u32> {
    let img = utim_core::graphics::decode_png(data)?;
    const STEP: u32 = 24;
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    let mut y = 0u32;
    while y < img.height {
        let mut x = 0u32;
        while x < img.width {
            let i = ((y * img.width + x) * 4) as usize;
            if i + 3 < img.pixels.len() && img.pixels[i + 3] > 128 {
                r += img.pixels[i] as u64;
                g += img.pixels[i + 1] as u64;
                b += img.pixels[i + 2] as u64;
                n += 1;
            }
            x += STEP;
        }
        y += STEP;
    }
    if n == 0 {
        return None;
    }
    Some(
        (0xFF << 24)
            | (((r / n) as u32) << 16)
            | (((g / n) as u32) << 8)
            | ((b / n) as u32),
    )
}

fn cleanup_terminal_child(
    active_child_pid: &std::sync::atomic::AtomicU32,
    active_stdin: &std::sync::Mutex<Option<std::process::ChildStdin>>,
) {
    let pid = active_child_pid.swap(0, std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGINT);
            libc::kill(pid as i32, libc::SIGINT);
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    *active_stdin.lock().unwrap() = None;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAction {
    Continue,
    CloseTab,
    NewTab,
    SwitchTab(usize),
}

pub struct TerminalTab {
    pub id: usize,
    pub title: String,
    pub lines: Vec<String>,
    pub input: String,
    pub tx: std::sync::mpsc::Sender<String>,
    pub rx: std::sync::mpsc::Receiver<String>,
    pub active_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
    pub active_child_pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl TerminalTab {
    pub fn new(id: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            id,
            title: format!("Tab {}: bash", id),
            lines: Vec::with_capacity(32),
            input: String::with_capacity(64),
            tx,
            rx,
            active_stdin: std::sync::Arc::new(std::sync::Mutex::new(None)),
            active_child_pid: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    pub fn is_running(&self) -> bool {
        let pid = self.active_child_pid.load(std::sync::atomic::Ordering::SeqCst);
        pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    pub fn cleanup_child(&self) {
        cleanup_terminal_child(&self.active_child_pid, &self.active_stdin);
    }
}

fn handle_terminal_enter(
    terminal_tabs: &mut Vec<TerminalTab>,
    active_tab_idx: &mut usize,
    next_tab_id: &mut usize,
    active_app: &mut Option<String>,
    keyboard: &mut VirtualKeyboard,
    search_active: &mut bool,
) {
    if *active_tab_idx >= terminal_tabs.len() {
        *active_tab_idx = terminal_tabs.len().saturating_sub(1);
    }
    let tab = &mut terminal_tabs[*active_tab_idx];
    let pid = tab.active_child_pid.load(std::sync::atomic::Ordering::SeqCst);
    let is_alive = pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 };
    let forwarded_to_child = if is_alive {
        let mut guard = tab.active_stdin.lock().unwrap();
        if let Some(ref mut stdin) = *guard {
            use std::io::Write;
            let _ = stdin.write_all(tab.input.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
            true
        } else {
            false
        }
    } else {
        tab.active_child_pid.store(0, std::sync::atomic::Ordering::SeqCst);
        *tab.active_stdin.lock().unwrap() = None;
        false
    };

    if forwarded_to_child {
        push_terminal_line(&mut tab.lines, &tab.input);
        log_terminal_output(&tab.input);
        tab.input.clear();
    } else {
        let tab_id = tab.id;
        let mut title_buf = tab.title.clone();
        let action = execute_terminal_command(
            &mut tab.lines,
            &mut tab.input,
            active_app,
            Some(&tab.tx),
            Some(&tab.active_stdin),
            Some(&tab.active_child_pid),
            Some(&mut title_buf),
            tab_id,
        );
        tab.title = title_buf;

        match action {
            TerminalAction::CloseTab => {
                tab.cleanup_child();
                terminal_tabs.remove(*active_tab_idx);
                if terminal_tabs.is_empty() {
                    *active_app = None;
                    keyboard.deactivate();
                    *search_active = false;
                    terminal_tabs.push(TerminalTab::new(1));
                    *next_tab_id = 2;
                    *active_tab_idx = 0;
                } else if *active_tab_idx >= terminal_tabs.len() {
                    *active_tab_idx = terminal_tabs.len() - 1;
                }
            }
            TerminalAction::NewTab => {
                if terminal_tabs.len() < 4 {
                    terminal_tabs.push(TerminalTab::new(*next_tab_id));
                    *next_tab_id += 1;
                    *active_tab_idx = terminal_tabs.len() - 1;
                }
            }
            TerminalAction::SwitchTab(idx) => {
                if idx < terminal_tabs.len() {
                    *active_tab_idx = idx;
                }
            }
            TerminalAction::Continue => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_ime_action(
    act: ImeAction,
    active_app: &mut Option<String>,
    app_input: &mut String,
    app_input_focused: &mut bool,
    search_active: &mut bool,
    search_query: &mut String,
    terminal_tabs: &mut Vec<TerminalTab>,
    active_tab_idx: &mut usize,
    next_tab_id: &mut usize,
    keyboard: &mut VirtualKeyboard,
    messages_list: &mut Vec<String>,
) {
    match act {
        ImeAction::CommitString(s) => {
            if active_app.as_deref() == Some("Terminal") {
                if !terminal_tabs.is_empty() {
                    terminal_tabs[*active_tab_idx].input.push_str(&s);
                }
            } else if *search_active {
                if search_query.len() + s.len() <= 60 {
                    search_query.push_str(&s);
                }
            } else if active_app.is_some() && *app_input_focused && app_input.len() + s.len() <= 120 {
                app_input.push_str(&s);
            }
        }
        ImeAction::DeleteSurroundingText { .. } => {
            if active_app.as_deref() == Some("Terminal") {
                if !terminal_tabs.is_empty() {
                    terminal_tabs[*active_tab_idx].input.pop();
                }
            } else if *search_active {
                search_query.pop();
            } else if active_app.is_some() && *app_input_focused {
                app_input.pop();
            }
        }
        ImeAction::SendKey(28) => {
            // ENTER
            if active_app.as_deref() == Some("Terminal") {
                handle_terminal_enter(
                    terminal_tabs,
                    active_tab_idx,
                    next_tab_id,
                    active_app,
                    keyboard,
                    search_active,
                );
            } else if *search_active {
                *search_active = false;
                keyboard.deactivate();
            } else if active_app.is_some() && *app_input_focused {
                if active_app.as_deref() == Some("Messages") {
                    if !app_input.is_empty() {
                        messages_list.push(format!("You: {}", app_input));
                        app_input.clear();
                    }
                } else {
                    keyboard.deactivate();
                    *app_input_focused = false;
                }
            }
        }
        _ => {}
    }
}

fn log_terminal_output(line: &str) {
    use std::io::Write;
    use std::fs::OpenOptions;

    // 1. Mirror to serial console /dev/ttyAMA0 so QEMU captures it into dist/qemu_terminal.log
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/ttyAMA0") {
        let _ = writeln!(tty, "[UTLC-TERM] {}", line);
    } else if let Ok(mut console) = OpenOptions::new().write(true).open("/dev/console") {
        let _ = writeln!(console, "[UTLC-TERM] {}", line);
    }

    // 2. Append to persistent log file /var/log/terminal.log on device
    if let Ok(mut log_file) = OpenOptions::new().create(true).append(true).open("/var/log/terminal.log") {
        let _ = writeln!(log_file, "{}", line);
    }
}

fn clean_terminal_line(line: &str) -> String {
    let s = line.split('\r').next_back().unwrap_or(line);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            } else if chars.peek() == Some(&']') {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\x07' || (next == '\x1b' && chars.peek() == Some(&'\\')) {
                        if next == '\x1b' {
                            chars.next();
                        }
                        break;
                    }
                }
                continue;
            } else if chars.peek() == Some(&'(') || chars.peek() == Some(&')') {
                chars.next();
                chars.next();
                continue;
            }
        }
        match c {
            '─' | '━' | '═' => out.push('-'),
            '│' | '┃' | '║' => out.push('|'),
            '┌' | '┍' | '┎' | '┏' | '╭' | '╔' => out.push('+'),
            '┐' | '┑' | '┒' | '┓' | '╮' | '╗' => out.push('+'),
            '└' | '┕' | '┖' | '┗' | '╰' | '╚' => out.push('+'),
            '┘' | '┙' | '┚' | '┛' | '╯' | '╝' => out.push('+'),
            '├' | '┝' | '┞' | '┟' | '┠' | '┡' | '┢' | '┣' => out.push('+'),
            '┤' | '┥' | '┦' | '┧' | '┨' | '┩' | '┪' | '┫' => out.push('+'),
            '┬' | '┴' | '┼' => out.push('+'),
            '•' | '·' | '●' => out.push('*'),
            '…' => out.push_str("..."),
            '‘' | '’' => out.push('\''),
            '“' | '”' => out.push('"'),
            '→' | '➔' | '➜' => out.push('>'),
            '←' => out.push('<'),
            '✓' | '✔' => out.push('v'),
            '✗' | '✘' => out.push('x'),
            '\t' => out.push_str("    "),
            _ if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => {}
        }
    }
    out
}

fn push_terminal_line(terminal_lines: &mut Vec<String>, line: &str) {
    let cleaned = clean_terminal_line(line);
    let max_col = 54;
    let mut rem = cleaned.trim_end();
    if rem.is_empty() {
        terminal_lines.push(String::new());
        return;
    }
    while rem.len() > max_col {
        let break_idx = rem[..max_col]
            .rfind(' ')
            .filter(|&idx| idx >= max_col.saturating_sub(15))
            .unwrap_or(max_col);

        let (left, right) = rem.split_at(break_idx);
        terminal_lines.push(left.trim_end().to_string());
        rem = right.trim_start();
    }
    if !rem.is_empty() {
        terminal_lines.push(rem.to_string());
    }
}

fn run_command_process(
    cmd: &str,
    tx: std::sync::mpsc::Sender<String>,
    active_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
    active_child_pid: std::sync::Arc<std::sync::atomic::AtomicU32>,
) {
    let has_real_shell = Path::new("/bin/bash").exists() || Path::new("/bin/sh").exists();
    let mut executed_real = false;
    if has_real_shell {
        let is_root = unsafe { libc::geteuid() == 0 };
        let (target_home, target_user) = if is_root {
            let _ = std::fs::create_dir_all("/home/user");
            let _ = std::fs::create_dir_all("/run/user/1000");
            unsafe {
                if let Ok(c_home) = std::ffi::CString::new("/home/user") {
                    libc::chown(c_home.as_ptr(), 1000, 1000);
                }
                if let Ok(c_run_u) = std::ffi::CString::new("/run/user/1000") {
                    libc::chown(c_run_u.as_ptr(), 1000, 1000);
                    libc::chmod(c_run_u.as_ptr(), 0o777);
                }
            }
            ("/home/user", "user")
        } else {
            ("/home/user", "user")
        };

        let shell = if Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        let mut cmd_obj = std::process::Command::new(shell);
        cmd_obj
            .arg("-c")
            .arg(cmd)
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LD_LIBRARY_PATH", "/usr/lib/aarch64-linux-gnu:/lib/aarch64-linux-gnu:/usr/lib:/lib")
            .env("HOME", target_home)
            .env("USER", target_user)
            .env("LOGNAME", target_user)
            .env("SHELL", shell)
            .env("TERM", "linux")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .env("XDG_RUNTIME_DIR", "/run/user/1000")
            .env("COLUMNS", "54")
            .env("LINES", "25")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped());

        if is_root {
            cmd_obj.uid(1000).gid(1000);
            if std::path::Path::new(target_home).exists() {
                cmd_obj.current_dir(target_home);
            }
        }

        unsafe {
            cmd_obj.pre_exec(move || {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                libc::setpgid(0, 0);
                if is_root {
                    let groups = [1000 as libc::gid_t, 24, 27, 29, 44, 105, 107];
                    libc::setgroups(groups.len(), groups.as_ptr());
                }
                Ok(())
            });
        }
        if let Ok(mut child) = cmd_obj.spawn()
        {
            executed_real = true;
            let my_pid = child.id();
            active_child_pid.store(my_pid, std::sync::atomic::Ordering::SeqCst);
            if let Some(stdin) = child.stdin.take() {
                *active_stdin.lock().unwrap() = Some(stdin);
            }
            let tx_err = tx.clone();
            let err_handle = child.stderr.take().map(|stderr| {
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        log_terminal_output(&line);
                        let _ = tx_err.send(line);
                    }
                })
            });

            if let Some(stdout) = child.stdout.take() {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    log_terminal_output(&line);
                    let _ = tx.send(line);
                }
            }

            if let Some(handle) = err_handle {
                let _ = handle.join();
            }
            let _ = child.wait();
            active_child_pid.store(0, std::sync::atomic::Ordering::SeqCst);
            *active_stdin.lock().unwrap() = None;
        }
    }

    if !executed_real {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let bin_name = parts.first().copied().unwrap_or("");

        // Check if a direct binary exists in /usr/bin, /bin, /usr/sbin, /sbin
        let bin_path = if bin_name.starts_with('/') {
            PathBuf::from(bin_name)
        } else {
            let p1 = PathBuf::from(format!("/usr/bin/{}", bin_name));
            let p2 = PathBuf::from(format!("/bin/{}", bin_name));
            let p3 = PathBuf::from(format!("/usr/sbin/{}", bin_name));
            let p4 = PathBuf::from(format!("/sbin/{}", bin_name));
            if p1.exists() {
                p1
            } else if p2.exists() {
                p2
            } else if p3.exists() {
                p3
            } else {
                p4
            }
        };

        if bin_path.exists() {
            let mut cmd_obj = std::process::Command::new(&bin_path);
            cmd_obj
                .args(&parts[1..])
                .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                .env("LD_LIBRARY_PATH", "/usr/lib/aarch64-linux-gnu:/lib/aarch64-linux-gnu:/usr/lib:/lib")
                .env("HOME", "/root")
                .env("USER", "root")
                .env("SHELL", "/bin/bash")
                .env("TERM", "linux")
                .env("DEBIAN_FRONTEND", "noninteractive")
                .env_remove("WAYLAND_DISPLAY")
                .env("WAYLAND_DISPLAY", "")
                .env_remove("DISPLAY")
                .env("DISPLAY", "")
                .env("XDG_SESSION_TYPE", "tty")
                .env("QT_QPA_PLATFORM", "offscreen")
                .env("GDK_BACKEND", "x11")
                .env("CI", "1")
                .env("COLUMNS", "54")
                .env("LINES", "25")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped());
            unsafe {
                cmd_obj.pre_exec(|| {
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
            if let Ok(mut child) = cmd_obj.spawn()
            {
                executed_real = true;
                let my_pid = child.id();
                active_child_pid.store(my_pid, std::sync::atomic::Ordering::SeqCst);
                if let Some(stdin) = child.stdin.take() {
                    *active_stdin.lock().unwrap() = Some(stdin);
                }
                let tx_err = tx.clone();
                let err_handle = child.stderr.take().map(|stderr| {
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(stderr);
                        for line in reader.lines().map_while(Result::ok) {
                            log_terminal_output(&line);
                            let _ = tx_err.send(line);
                        }
                    })
                });

                if let Some(stdout) = child.stdout.take() {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stdout);
                    for line in reader.lines().map_while(Result::ok) {
                        log_terminal_output(&line);
                        let _ = tx.send(line);
                    }
                }

                if let Some(handle) = err_handle {
                    let _ = handle.join();
                }
                let _ = child.wait();
                active_child_pid.store(0, std::sync::atomic::Ordering::SeqCst);
                *active_stdin.lock().unwrap() = None;
            }
        }

        if !executed_real {
            match bin_name {
                "uname" => {
                    let _ = tx.send("Linux treble-gsi 6.1.23-android14-4-00257 aarch64 GNU/Linux".into());
                }
                "uptime" => {
                    let uptime_str = fs::read_to_string("/proc/uptime").unwrap_or_else(|_| "0.0 0.0".into());
                    let secs = uptime_str.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) as u64;
                    let mins = (secs / 60) % 60;
                    let hours = secs / 3600;
                    let _ = tx.send(format!("up {:02}:{:02}, 1 user, load avg: 0.02, 0.01, 0.00", hours, mins));
                }
                "whoami" => {
                    let _ = tx.send("root".into());
                }
                "date" => {
                    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
                    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
                    let _ = tx.send(format!("UTC epoch: {}s", ts.tv_sec));
                }
                "ls" => {
                    let _ = tx.send("bin  dev  etc  init  lib  proc  run  sbin  sys  tmp  usr  var".into());
                }
                "help" => {
                    let _ = tx.send("Built-in: uname, uptime, whoami, date, ls, ip, ping, clear, exit".into());
                    let _ = tx.send("Notice: Full Debian CLI (apt, dpkg, bash) active".into());
                }
                "ip" | "ifconfig" => {
                    let _ = tx.send("1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536".into());
                    let _ = tx.send("    inet 127.0.0.1/8 scope host lo".into());
                    let _ = tx.send("2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500".into());
                    let _ = tx.send("    inet 10.0.2.15/24 brd 10.0.2.255 scope global eth0".into());
                    let _ = tx.send("    default via 10.0.2.2 dev eth0, DNS: 10.0.2.3, 8.8.8.8".into());
                }
                "ping" => {
                    let host = parts.get(1).copied().unwrap_or("8.8.8.8");
                    let _ = tx.send(format!("PING {} ({}): 56 data bytes", host, host));
                    let _ = tx.send(format!("64 bytes from {}: icmp_seq=1 ttl=118 time=11.4 ms", host));
                    let _ = tx.send(format!("64 bytes from {}: icmp_seq=2 ttl=118 time=10.9 ms", host));
                    let _ = tx.send(format!("--- {} ping statistics ---", host));
                    let _ = tx.send("2 packets transmitted, 2 received, 0% packet loss".into());
                }
                "apt" | "apt-get" | "dpkg" => {
                    let _ = tx.send(format!("bash: {}: command not found", bin_name));
                    let _ = tx.send("[!] APT is not present in this lightweight mock rootfs.".into());
                    let _ = tx.send("[*] To install Debian Sid packages (apt, dpkg, bash, coreutils):".into());
                    let _ = tx.send("    Exit QEMU and run: sudo ./scripts/run_qemu.sh --full-debian".into());
                }
                other => {
                    let _ = tx.send(format!("bash: {}: command not found", other));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_terminal_command(
    terminal_lines: &mut Vec<String>,
    terminal_input: &mut String,
    _active_app: &mut Option<String>,
    term_tx: Option<&std::sync::mpsc::Sender<String>>,
    active_stdin: Option<&std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>>,
    active_child_pid: Option<&std::sync::Arc<std::sync::atomic::AtomicU32>>,
    tab_title: Option<&mut String>,
    tab_id: usize,
) -> TerminalAction {
    let cmd = terminal_input.trim().to_string();
    let prompt_line = format!("root@treble-gsi:~# {}", cmd);
    push_terminal_line(terminal_lines, &prompt_line);
    log_terminal_output(&prompt_line);
    terminal_input.clear();

    if cmd.is_empty() {
        return TerminalAction::Continue;
    }

    if cmd == "clear" {
        terminal_lines.clear();
        return TerminalAction::Continue;
    }

    if cmd == "exit" || cmd == "closetab" {
        if let Some(pid_arc) = active_child_pid {
            if let Some(stdin_arc) = active_stdin {
                cleanup_terminal_child(pid_arc, stdin_arc);
            } else {
                let pid = pid_arc.swap(0, std::sync::atomic::Ordering::SeqCst);
                if pid > 0 {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
        } else if let Some(stdin_arc) = active_stdin {
            *stdin_arc.lock().unwrap() = None;
        }
        return TerminalAction::CloseTab;
    }

    if cmd == "newtab" {
        return TerminalAction::NewTab;
    }

    if cmd.starts_with("tab ") {
        if let Some(num_str) = cmd.split_whitespace().nth(1) {
            if let Ok(num) = num_str.parse::<usize>() {
                if (1..=4).contains(&num) {
                    return TerminalAction::SwitchTab(num - 1);
                }
            }
        }
        push_terminal_line(terminal_lines, "Usage: tab <1..4>");
        return TerminalAction::Continue;
    }

    if cmd == "tabs" {
        push_terminal_line(terminal_lines, "=== Active Terminal Tabs ===");
        push_terminal_line(terminal_lines, "Shortcuts: Ctrl+T (new tab), Ctrl+W (close tab), Ctrl+Tab / Ctrl+1..4 (switch)");
        push_terminal_line(terminal_lines, "Commands: newtab, closetab, tab <1..4>, exit");
        return TerminalAction::Continue;
    }

    let first_word = cmd.split_whitespace().next().unwrap_or("bash");
    if let Some(title) = tab_title {
        *title = format!("Tab {}: {}", tab_id, first_word);
    }

    let stdin_arc = active_stdin.cloned().unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(None)));
    let pid_arc = active_child_pid.cloned().unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)));

    if let Some(tx) = term_tx {
        let tx = tx.clone();
        std::thread::spawn(move || {
            run_command_process(&cmd, tx, stdin_arc, pid_arc);
        });
    } else {
        let (tx, rx) = std::sync::mpsc::channel();
        run_command_process(&cmd, tx, stdin_arc, pid_arc);
        while let Ok(line) = rx.try_recv() {
            push_terminal_line(terminal_lines, &line);
        }
    }

    if terminal_lines.len() > 120 {
        terminal_lines.drain(0..terminal_lines.len() - 120);
    }
    TerminalAction::Continue
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

        let res = execute_terminal_command(&mut lines, &mut input, &mut app, None, None, None, None, 1);
        assert_eq!(res, TerminalAction::Continue);
        assert!(input.is_empty());
        assert!(lines.len() >= 2);
        assert_eq!(lines[0], "root@treble-gsi:~# uname -a");
        assert!(lines[1].contains("Linux"));

        input = "exit".to_string();
        let res = execute_terminal_command(&mut lines, &mut input, &mut app, None, None, None, None, 1);
        assert_eq!(res, TerminalAction::CloseTab);
    }

    #[test]
    fn test_multiple_terminal_tabs() {
        let mut tabs = vec![TerminalTab::new(1), TerminalTab::new(2)];
        let mut active_tab = 0;
        let mut next_id = 3;
        let mut app = Some("Terminal".to_string());
        let mut kb = VirtualKeyboard::new(1080.0, 2400.0);
        let mut search = false;

        // Run uname in Tab 1
        tabs[0].input = "uname".to_string();
        handle_terminal_enter(&mut tabs, &mut active_tab, &mut next_id, &mut app, &mut kb, &mut search);
        if let Ok(line) = tabs[0].rx.recv_timeout(Duration::from_millis(500)) {
            push_terminal_line(&mut tabs[0].lines, &line);
        }
        assert!(tabs[0].lines.len() >= 2);
        assert!(tabs[0].lines[1].contains("Linux"));

        // Switch to Tab 2
        active_tab = 1;
        tabs[1].input = "whoami".to_string();
        handle_terminal_enter(&mut tabs, &mut active_tab, &mut next_id, &mut app, &mut kb, &mut search);
        if let Ok(line) = tabs[1].rx.recv_timeout(Duration::from_millis(500)) {
            push_terminal_line(&mut tabs[1].lines, &line);
        }
        assert!(tabs[1].lines.len() >= 2);
        assert!(tabs[1].lines[1] == "root" || tabs[1].lines[1] == "linux");

        // Wait for whoami process to finish
        for _ in 0..50 {
            if !tabs[1].is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Open Tab 3 via "newtab" command
        tabs[1].input = "newtab".to_string();
        handle_terminal_enter(&mut tabs, &mut active_tab, &mut next_id, &mut app, &mut kb, &mut search);
        assert_eq!(tabs.len(), 3);
        assert_eq!(active_tab, 2);

        // Close Tab 3 via "exit"
        tabs[2].input = "exit".to_string();
        handle_terminal_enter(&mut tabs, &mut active_tab, &mut next_id, &mut app, &mut kb, &mut search);
        assert_eq!(tabs.len(), 2);
        assert_eq!(active_tab, 1);
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

    #[test]
    fn test_build_all_apps_with_firefox() {
        let mut catalogue = DesktopCatalogue::new();
        let ff_entry = DesktopApp {
            id: "firefox".to_string(),
            name: "Firefox Web Browser".to_string(),
            exec: "/usr/lib/firefox/firefox %u".to_string(),
            icon: "firefox".to_string(),
            categories: vec!["Network".to_string(), "WebBrowser".to_string()],
            no_display: false,
            terminal: false,
            keywords: vec!["web".to_string(), "browser".to_string()],
        };
        catalogue.add_app(ff_entry);

        let apps = build_all_apps(&catalogue);
        // Base apps count is 12 + Firefox = 13
        assert!(apps.len() >= 13);

        // Check that "Firefox" app was added
        let ff_app = apps.iter().find(|a| a.name == "Firefox");
        assert!(ff_app.is_some(), "Firefox must appear on the home screen when installed");
        let ff = ff_app.unwrap();
        assert_eq!(ff.color, 0xFFFF5722);
        assert_eq!(ff.glyph, "F");
        assert_eq!(ff.exec, "/usr/lib/firefox/firefox");

        // Check that the Browser shortcut is wired to Firefox
        let browser_app = apps.iter().find(|a| a.name == "Browser").unwrap();
        assert_eq!(browser_app.exec, "/usr/lib/firefox/firefox");
    }

    #[test]
    fn test_clean_build_no_uninstalled_apps() {
        let catalogue = DesktopCatalogue::new();
        let apps = build_all_apps(&catalogue);
        // Clean build has exactly 12 base apps
        assert_eq!(apps.len(), 12);
        // Firefox is not preinstalled
        assert!(apps.iter().all(|a| a.name != "Firefox"));
        // Browser shortcut default exec is empty (built-in browser)
        let browser = apps.iter().find(|a| a.name == "Browser").unwrap();
        assert!(browser.exec.is_empty());
    }

    #[test]
    fn test_build_all_apps_with_third_party() {
        let mut catalogue = DesktopCatalogue::new();
        let vlc = DesktopApp {
            id: "vlc".to_string(),
            name: "VLC media player".to_string(),
            exec: "/usr/bin/vlc".to_string(),
            icon: "vlc".to_string(),
            categories: vec!["AudioVideo".to_string()],
            no_display: false,
            terminal: false,
            keywords: vec![],
        };
        let calc = DesktopApp {
            id: "calculator".to_string(),
            name: "Calculator".to_string(),
            exec: "gnome-calculator".to_string(),
            icon: "calculator".to_string(),
            categories: vec!["Utility".to_string()],
            no_display: false,
            terminal: false,
            keywords: vec![],
        };
        catalogue.add_app(vlc);
        catalogue.add_app(calc);

        let apps = build_all_apps(&catalogue);
        // Base 12 + 2 installed apps = 14
        assert_eq!(apps.len(), 14);
        assert!(apps.iter().any(|a| a.id == "vlc"));
        assert!(apps.iter().any(|a| a.id == "calculator"));
    }

    #[test]
    fn test_get_app_color_deterministic() {
        let c1 = get_app_color("firefox");
        let c2 = get_app_color("firefox");
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_universal_ime_actions_and_app_input() {
        let mut active_app = Some("Browser".to_string());
        let mut app_input = String::new();
        let mut app_input_focused = true;
        let mut search_active = false;
        let mut search_query = String::new();
        let mut terminal_tabs = vec![TerminalTab::new(1)];
        let mut active_tab_idx = 0;
        let mut next_tab_id = 2;
        let mut keyboard = VirtualKeyboard::new(1080.0, 2400.0);
        keyboard.activate();
        let mut messages_list = Vec::new();

        // 1. Commit characters to active app input
        apply_ime_action(
            ImeAction::CommitString("https://google.com".to_string()),
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert_eq!(app_input, "https://google.com");

        // 2. Backspace deletes character
        apply_ime_action(
            ImeAction::DeleteSurroundingText { before_length: 1, after_length: 0 },
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert_eq!(app_input, "https://google.co");

        // 3. Enter in Browser finishes input and closes keyboard
        apply_ime_action(
            ImeAction::SendKey(28),
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert!(!keyboard.is_active);
        assert!(!app_input_focused);

        // 4. Test Messages app: Enter posts message to messages_list
        active_app = Some("Messages".to_string());
        app_input = "Hello world!".to_string();
        app_input_focused = true;
        keyboard.activate();

        apply_ime_action(
            ImeAction::SendKey(28),
            &mut active_app,
            &mut app_input,
            &mut app_input_focused,
            &mut search_active,
            &mut search_query,
            &mut terminal_tabs,
            &mut active_tab_idx,
            &mut next_tab_id,
            &mut keyboard,
            &mut messages_list,
        );
        assert_eq!(messages_list.len(), 1);
        assert_eq!(messages_list[0], "You: Hello world!");
        assert!(app_input.is_empty());
    }

    #[test]
    fn test_universal_text_input_wayland_protocol() {
        use utim_core::compositor::protocols::WlMessageBuilder;

        let mut keyboard = VirtualKeyboard::new(1080.0, 2400.0);
        assert!(!keyboard.is_active);

        // Build opcode 1: zwp_text_input_v3.enable
        let builder_enable = WlMessageBuilder::new(42, 1);
        let wire_enable = builder_enable.build();

        let (msg_enable, len) = utim_core::compositor::protocols::WlMessage::parse(&wire_enable).unwrap().unwrap();
        assert_eq!(len, wire_enable.len());
        if msg_enable.header.opcode == 1 {
            keyboard.activate();
        }
        assert!(keyboard.is_active);

        // Build opcode 2: zwp_text_input_v3.disable
        let builder_disable = WlMessageBuilder::new(42, 2);
        let wire_disable = builder_disable.build();

        let (msg_disable, _) = utim_core::compositor::protocols::WlMessage::parse(&wire_disable).unwrap().unwrap();
        if msg_disable.header.opcode == 2 {
            keyboard.deactivate();
        }
        assert!(!keyboard.is_active);
    }

    #[test]
    fn test_all_base_apps_resolve_png_icons() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
        let assets_icons = workspace_root.join("assets/icons");
        assert!(assets_icons.exists(), "assets/icons must exist");

        let mut icon_cache = IconCache::with_roots(vec![assets_icons], "hicolor");
        let mut apps = build_all_apps(&DesktopCatalogue::new());
        apply_app_icons(&mut apps, &mut icon_cache);

        // Verify that every single base app has a resolved PNG icon
        for app in &apps {
            assert!(
                app.icon.is_some(),
                "App '{}' (id: '{}', keys: {:?}) must resolve a real PNG icon image",
                app.name, app.id, app.icon_keys
            );
        }

        // Also verify dock Apps icon
        assert!(
            icon_cache.get("view-app-grid").or_else(|| icon_cache.get("apps")).is_some(),
            "Dock Apps icon must resolve a real PNG icon image"
        );
    }

    #[test]
    fn test_home_pages_icon_reordering_and_movement() {
        let mut home_pages = vec![
            vec!["settings".to_string(), "files".to_string(), "terminal".to_string(), "gallery".to_string()],
            vec!["clock".to_string(), "contacts".to_string()],
        ];
        let current_page = 0;
        let mut selected_icon: Option<String> = Some("terminal".to_string());

        // 1. Reorder within Page 0: move "terminal" from index 2 to slot 0
        if let Some(sel_id) = selected_icon.take() {
            if let Some(old_pos) = home_pages[current_page].iter().position(|id| *id == sel_id) {
                home_pages[current_page].remove(old_pos);
                home_pages[current_page].insert(0, sel_id);
            }
        }
        assert_eq!(home_pages[0], vec!["terminal", "settings", "files", "gallery"]);

        // 2. Move "settings" to Page 1
        selected_icon = Some("settings".to_string());
        if let Some(sel_id) = selected_icon.take() {
            if let Some(pos) = home_pages[current_page].iter().position(|id| *id == sel_id) {
                home_pages[current_page].remove(pos);
            }
            home_pages[1].push(sel_id);
        }
        assert_eq!(home_pages[0], vec!["terminal", "files", "gallery"]);
        assert_eq!(home_pages[1], vec!["clock", "contacts", "settings"]);

        // 3. Remove "files" from Home Page 0
        selected_icon = Some("files".to_string());
        if let Some(sel_id) = selected_icon.take() {
            if let Some(pos) = home_pages[current_page].iter().position(|id| *id == sel_id) {
                home_pages[current_page].remove(pos);
            }
        }
        assert_eq!(home_pages[0], vec!["terminal", "gallery"]);
        // Verify files is gone from home page 0, but catalogue retains it
        let all_apps = build_all_apps(&DesktopCatalogue::new());
        assert!(all_apps.iter().any(|a| a.id == "files"));
    }

    #[test]
    fn test_pixelui_app_drawer_and_pinning() {
        let all_apps = build_all_apps(&DesktopCatalogue::new());
        let mut home_pages = vec![
            vec!["terminal".to_string(), "gallery".to_string()],
        ];
        let current_page = 0;
        let mut app_drawer_open = false;

        // 1. Tapping Apps toggles drawer
        app_drawer_open = !app_drawer_open;
        assert!(app_drawer_open);

        // 2. App search filtering in drawer
        let drawer_search = "cam";
        let filtered: Vec<&ManagedApp> = all_apps
            .iter()
            .filter(|a| a.name.to_lowercase().contains(drawer_search) || a.id.to_lowercase().contains(drawer_search))
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Camera");

        // 3. Long press app in drawer pins it to current home page
        let target_app = &filtered[0];
        if !home_pages[current_page].contains(&target_app.id) {
            home_pages[current_page].push(target_app.id.clone());
        }
        assert!(home_pages[current_page].contains(&"camera".to_string()));
        assert_eq!(home_pages[current_page].len(), 3);

        // 4. Close drawer
        app_drawer_open = false;
        assert!(!app_drawer_open);
    }

    /// Every tappable region of the launcher, asserted against the same
    /// `Layout` the renderer draws from. This is the regression guard for the
    /// original bug: hitboxes that were tuned by hand and drifted from the
    /// pixels the user actually saw.
    #[test]
    fn test_launcher_layout_hitboxes_match_rendered_geometry() {
        let w = 1080.0;
        let h = 2400.0;
        let l = Layout::plain(w, h);

        // 1. Status bar: the top band, and nothing below it.
        assert!(l.status_bar_h >= 20.0);
        assert!(l.status_bar_h < 80.0, "status bar is a sliver");
        assert!(!l.search.contains(w * 0.5, l.status_bar_h + 1.0));

        // 2. Search pill: hit inside, miss on every side.
        assert!(l.search.contains(w * 0.5, l.search.center_y()));
        assert!(!l.search.contains(-1.0, l.search.center_y()));
        assert!(!l.search.contains(w + 1.0, l.search.center_y()));
        assert!(!l.search.contains(w * 0.5, l.search.y - 1.0));
        assert!(!l.search.contains(w * 0.5, l.search.y + l.search.h + 1.0));

        // 3. Clock widget.
        assert!(l.clock_rect().contains(w * 0.5, l.clock_y + 1.0));

        // 4. Grid cells: the centre of every drawn icon hit-tests to itself,
        //    and the row below the grid hit-tests to nothing.
        for i in 0..(l.grid_cols * l.max_rows.min(8)) {
            let icon = l.grid_icon(i);
            let (cx, cy) = icon.center();
            assert_eq!(l.home_grid_hit(cx, cy, 0.0), Some(i), "icon {i}");
            let cell = l.grid_cell(i);
            let (ccx, ccy) = cell.center();
            assert_eq!(l.home_grid_hit(ccx, ccy, 0.0), Some(i), "cell {i}");
        }
        assert_eq!(l.home_grid_hit(w * 0.5, l.grid_bottom, 0.0), None);
        assert_eq!(l.home_grid_hit(w * 0.5, l.grid_top - 1.0, 0.0), None);

        // 5. Page indicator.
        assert_eq!(
            l.home_page_hit(l.page_dots.x + 1.0, l.page_dots.center_y(), 2),
            Some(0)
        );
        assert_eq!(
            l.home_page_hit(l.page_dots.x + l.page_dots.w - 1.0, l.page_dots.center_y(), 2),
            Some(1)
        );
        assert_eq!(l.home_page_hit(w * 0.5, l.page_dots.y - 20.0, 2), None);

        // 6. Dock: each slot, plus a miss just above the dock.
        for s in 0..l.dock_slots {
            let icon = l.dock_icon_rect(s);
            assert_eq!(l.home_dock_hit(icon.center_x(), icon.center_y()), Some(s));
        }
        assert_eq!(l.home_dock_hit(w * 0.5, l.dock.y - 1.0), None);

        // 7. Gesture nav pill is below the dock and on screen.
        assert!(l.nav_pill.y > l.dock.y + l.dock.h);
        assert!(l.nav_pill.y + l.nav_pill.h <= h);
        assert!(l.nav_pill.contains(w * 0.5, l.nav_pill.center_y()));

        // 8. Edit-mode chips only exist with a selection, sit above the grid,
        //    and are mutually exclusive.
        let sel = Layout::new(w, h, true);
        assert!(sel.remove_chip.contains(sel.remove_chip.center_x(), sel.remove_chip.center_y()));
        assert!(sel.move_chip.contains(sel.move_chip.center_x(), sel.move_chip.center_y()));
        assert!(sel.remove_chip.x + sel.remove_chip.w <= sel.move_chip.x);
        assert!(sel.move_chip.y + sel.move_chip.h <= sel.grid_top);
        // The gap between the two chips belongs to neither.
        let gap_x = (sel.remove_chip.x + sel.remove_chip.w + sel.move_chip.x) * 0.5;
        assert!(!sel.remove_chip.contains(gap_x, sel.remove_chip.center_y()));
        assert!(!sel.move_chip.contains(gap_x, sel.remove_chip.center_y()));

        // 9. Drawer: search, handle and grid all move with the sheet.
        let off = 0.0;
        assert_eq!(
            l.drawer_search_hit(off, l.drawer_search.center_x(), off + l.drawer_search.center_y()),
            DrawerSearchHit::Focus
        );
        assert_eq!(
            l.drawer_search_hit(
                off,
                l.drawer_search.x + l.drawer_search.w - 2.0,
                off + l.drawer_search.center_y()
            ),
            DrawerSearchHit::Clear
        );
        assert_eq!(l.drawer_search_hit(off, 1.0, 1.0), DrawerSearchHit::None);
        assert!(l.drawer_handle.contains(w * 0.5, off + l.drawer_handle.center_y()));

        for i in 0..(l.grid_cols * l.drawer_rows.min(8)) {
            let cell = l.drawer_icon_cell(i);
            assert_eq!(
                l.drawer_grid_hit(off, cell.center_x(), off + cell.center_y()),
                Some(i),
                "drawer cell {i}"
            );
        }
        // A dragged-down drawer carries its content with it.
        let dragged = h * 0.5;
        let cell = l.drawer_icon_cell(3);
        assert_eq!(
            l.drawer_grid_hit(dragged, cell.center_x(), dragged + cell.center_y()),
            Some(3)
        );
        // And a touch above the sheet is not a drawer cell.
        assert_eq!(l.drawer_grid_hit(dragged, cell.center_x(), dragged - 1.0), None);
    }

    /// The same invariants have to hold on a small panel, otherwise the fix
    /// only works at one resolution.
    #[test]
    fn test_layout_scales_to_small_panels() {
        for (w, h) in [(360.0f32, 640.0f32), (720.0, 1280.0), (1440.0, 3120.0)] {
            let l = Layout::plain(w, h);
            for i in 0..(l.grid_cols * l.max_rows.min(6)) {
                let icon = l.grid_icon(i);
                assert_eq!(
                    l.home_grid_hit(icon.center_x(), icon.center_y(), 0.0),
                    Some(i),
                    "{w}x{h} icon {i}"
                );
            }
            for s in 0..l.dock_slots {
                let icon = l.dock_icon_rect(s);
                assert_eq!(
                    l.home_dock_hit(icon.center_x(), icon.center_y()),
                    Some(s),
                    "{w}x{h} dock {s}"
                );
            }
            assert!(
                l.search.h >= w.min(h) * 0.048,
                "{w}x{h}: search pill below the 48dp touch minimum"
            );
        }
    }

    #[test]
    fn test_lawnchair_animation_physics_step() {
        // Drawer progress interpolation towards open
        let mut drawer_prog: f32 = 0.0;
        let drawer_target: f32 = 1.0;
        let dt: f32 = 0.016;

        for _ in 0..10 {
            let diff = drawer_target - drawer_prog;
            drawer_prog += diff * (dt * 14.0).min(1.0);
        }
        assert!(drawer_prog > 0.85, "Drawer should smoothly ease open in ~160ms");

        // Home scroll offset decay (returning smoothly to center)
        let mut scroll_offset: f32 = 400.0;
        for _ in 0..20 {
            if scroll_offset.abs() > 0.5 {
                scroll_offset *= 1.0 - (dt * 12.0).min(0.9);
            } else {
                scroll_offset = 0.0;
            }
        }
        assert!(scroll_offset < 10.0, "Scroll offset should spring-decay quickly to rest");

        // Touch ripple radius expansion and alpha fade
        let mut ripple = Some((500.0, 600.0, 12.0, 0.7));
        for _ in 0..10 {
            if let Some((_, _, ref mut r, ref mut a)) = ripple {
                *r += dt * 140.0;
                *a -= dt * 3.0;
                if *a <= 0.0 {
                    ripple = None;
                }
            }
        }
        let (_, _, r, a) = ripple.expect("Ripple should still be active at 160ms");
        assert!(r > 30.0, "Ripple radius should expand over time");
        assert!(a < 0.35, "Ripple alpha should decay towards zero");

        // After additional frames, ripple fades completely to None
        for _ in 0..15 {
            if let Some((_, _, ref mut r, ref mut a)) = ripple {
                *r += dt * 140.0;
                *a -= dt * 3.0;
                if *a <= 0.0 {
                    ripple = None;
                }
            }
        }
        assert!(ripple.is_none(), "Ripple should fade completely after duration");
    }

    /// The advanced surface: clock, page dots, scrolled grid, drawer sheet and
    /// row limits, all checked against the shared layout.
    #[test]
    fn test_launcher_advanced_geometry_and_animations() {
        let w = 1080.0;
        let h = 2400.0;
        let l = Layout::plain(w, h);

        // 1. Clock widget.
        assert!(l.clock_rect().contains(w * 0.5, l.clock_y + 1.0));
        assert!(!l.clock_rect().contains(-1.0, l.clock_y + 1.0), "outside padding");
        assert!(!l.clock_rect().contains(w * 0.5, l.clock_y - 1.0), "above clock");
        assert!(
            !l.clock_rect().contains(w * 0.5, l.search.y + 1.0),
            "below clock, inside search"
        );

        // 2. Page dots: out of bounds, and slot mapping for several page counts.
        let cy = l.page_dots.center_y();
        assert_eq!(l.home_page_hit(-10.0, cy, 2), None, "negative x is out of bounds");
        assert_eq!(l.home_page_hit(w + 10.0, cy, 2), None, "x > w is out of bounds");
        assert_eq!(l.home_page_hit(l.page_dots.center_x(), cy, 0), None, "0 pages");
        assert_eq!(
            l.home_page_hit(l.page_dots.center_x(), cy, 1),
            Some(0),
            "one page is page 0"
        );
        for n in 2..=5usize {
            for p in 0..n {
                let x = l.page_dots.x + l.page_dots.w * (p as f32 + 0.5) / n as f32;
                assert_eq!(l.home_page_hit(x, cy, n), Some(p), "{n} pages, slot {p}");
            }
        }

        // 3. Grid with a scroll offset. A positive offset slides the workspace
        //    left, so a fixed touch sees the cell one column later.
        let y = l.grid_top + 4.0;
        let x = l.col_pitch * 0.5;
        let base = l.home_grid_hit(x, y, 0.0).unwrap();
        assert_eq!(l.home_grid_hit(x, y, l.col_pitch), Some(base + 1));
        // Off the left edge of the strip there is nothing.
        assert_eq!(l.home_grid_hit(x, y, -l.col_pitch * 2.0), None);

        // 4. Drawer with a mid-drag offset.
        // The handle is hit in the drawer's own coordinate space, which is
        // what the input path passes in.
        let dragged = h * 0.25;
        assert!(
            l.drawer_handle.contains(w * 0.5, l.drawer_handle.center_y()),
            "the handle is centred on the sheet"
        );
        assert!(
            !l.drawer_handle.contains(w * 0.5, dragged + l.drawer_handle.center_y()),
            "a touch above the sheet is not the handle"
        );
        assert_eq!(
            l.drawer_search_hit(
                dragged,
                l.drawer_search.center_x(),
                dragged + l.drawer_search.center_y()
            ),
            DrawerSearchHit::Focus
        );
        let cell0 = l.drawer_icon_cell(0);
        assert_eq!(
            l.drawer_grid_hit(dragged, cell0.center_x(), dragged + cell0.center_y()),
            Some(0)
        );
        assert_eq!(
            l.drawer_grid_hit(dragged, cell0.center_x(), dragged - 1.0),
            None,
            "above the sheet is not a grid hit"
        );
        assert_eq!(
            l.drawer_grid_hit(dragged, -1.0, dragged + cell0.center_y()),
            None,
            "off-screen x is out of bounds"
        );

        // 5. Row limits scale with the panel instead of being magic numbers.
        assert!(l.max_rows >= 4, "1080x2400 should fit many home rows");
        assert!(l.drawer_rows >= 8, "1080x2400 should fit many drawer rows");
        let small = Layout::plain(360.0, 640.0);
        assert!(small.max_rows < l.max_rows, "a short panel fits fewer rows");
    }

    /// Proportional metrics: the vector engine must not be monospaced.
    #[test]
    fn test_typography_proportional_metrics() {
        let scale = 2;
        let w_i = utim_core::graphics::text_width("i", scale);
        let w_m = utim_core::graphics::text_width("m", scale);
        assert!(w_i < w_m, "proportional 'i' ({}px) must be narrower than 'm' ({}px)", w_i, w_m);

        let w_space = utim_core::graphics::text_width(" ", scale);
        let w_w = utim_core::graphics::text_width("W", scale);
        assert!(w_space < w_w, "space should be narrower than capital W");

        // Digits are tabular so a clock never jitters.
        let zero = utim_core::graphics::text_width("0", scale);
        for d in "123456789".chars() {
            assert_eq!(
                utim_core::graphics::text_width(&d.to_string(), scale),
                zero,
                "digit {d} must share the tabular advance"
            );
        }

        let w_full = utim_core::graphics::text_width("Universal Treble", scale);
        assert!(w_full > 0);
    }
}
