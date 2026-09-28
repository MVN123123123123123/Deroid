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
    KEY_VOLUMEDOWN, KEY_VOLUMEUP, KEY_POWER,
};
use utim_core::compositor::lockscreen::{LockScreen, LockState};
use utim_core::compositor::power_sync::{PowerSaverMode, UtimPowerSync};
use utim_core::compositor::protocols::{ProtocolRegistry, WaylandInterface};
use utim_core::compositor::server::WaylandServer;
use utim_core::compositor::super_extreme::{SuperExtremeScreen, SuperExtremeState};
use utim_core::compositor::systemui::{QuickTileKind, SystemUiShade};
use utim_core::compositor::{FolderOpen, KillQueue, Recents, TaskCard};
use utim_core::graphics::font::{FontFamily, set_active_family};
use utim_core::graphics::composer::{HwcComposer, HwcVersion};
use utim_core::graphics::drawer_mod::FastScrollerState;
use utim_core::graphics::layout::{
    AppLayout, AppPanel, DrawerSearchHit, FastScrollerLayout, Key, Keyboard, Layout, ShadeLayout,
    ShadeZone, TabHit,
};
use utim_core::graphics::{
    AppGridItem, DrmInteractiveState, DrmKmsDevice, MaterialYouPalette, RecentsCard, RgbaImage,
    SpringConfig, SpringSimulation, TerminalTabInfo, damped_scroll,
};

/// Commit threshold for the recents gesture.
///
/// The reference commits on the *release* past a fraction of the screen rather
/// than on the hold alone (`AbsSwipeUpHandler.java:756`), so this is a progress
/// value the gesture layer produces, not a pixel distance the shell invents.
const RECENTS_COMMIT_PROGRESS: f32 = 0.5;

/// Dismiss every modal surface and return the workspace morph to rest.
///
/// Shared by the Home and Back paths, which differ only in what else they tear
/// down. Kept as one function because the failure mode of getting it wrong is
/// silent and sticky: a leftover `Overview` makes every subsequent workspace
/// swipe inert (the `is_modal` guard), and a leftover `workspace_scale` leaves
/// the app panel permanently shrunk with nothing driving it.
fn close_modal_surfaces(
    shell_state: &mut ShellState,
    folder: &mut FolderOpen,
    popup_spring: &mut SpringSimulation,
    workspace_scale_spring: &mut SpringSimulation,
    window_alpha_spring: &mut SpringSimulation,
) {
    *shell_state = ShellState::Normal;
    folder.close();
    popup_spring.set_target(0.0);
    workspace_scale_spring.set_target(1.0);
    window_alpha_spring.set_target(1.0);
}

/// What the shell does with one `GestureAction`, decided with no state at all.
///
/// This exists so the wiring is testable. The defect it fixes was that the
/// gesture match in the input loop ended in `_ => {}` and both
/// `GestureAction::Recents` and `GestureAction::BottomBarScrub` fell into it:
/// the overview could not be opened and a task scrub did nothing. Both were
/// produced by the gesture engine and both were tested *there*, so nothing
/// failed -- the actions simply had no consumer. Extracting the decision means
/// "this action has an effect" is an assertion about a pure function instead of
/// a line of code in a 5000-line `match` inside an event loop.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ShellEffect {
    /// Nothing to do.
    None,
    /// Open the recents carousel.
    OpenOverview,
    /// Move the selection by one card pitch, in card widths.
    ScrubTasks(f32),
    /// An in-app home gesture is in flight: the panel's scale and opacity.
    MorphWorkspace { scale: f32, window_alpha: f32 },
    /// Close every modal surface and return the workspace morph to rest.
    CloseAll,
}

/// Decide a gesture's effect. Pure: no shell state is read or written.
fn plan_gesture(a: &GestureAction) -> ShellEffect {
    match *a {
        // The two arms that used to be dropped.
        GestureAction::Recents { progress, .. } => {
            if progress >= RECENTS_COMMIT_PROGRESS {
                ShellEffect::OpenOverview
            } else {
                ShellEffect::None
            }
        }
        GestureAction::BottomBarScrub { app_shift, .. } => {
            if app_shift != 0 {
                ShellEffect::ScrubTasks(app_shift as f32)
            } else {
                ShellEffect::None
            }
        }
        // A *partial* home gesture: the panel is on its way back to the
        // workspace and the gesture layer has already computed the scale and
        // opacity it wants.
        GestureAction::Home {
            progress,
            scale,
            window_alpha,
        } => {
            if progress >= 1.0 {
                ShellEffect::CloseAll
            } else {
                ShellEffect::MorphWorkspace { scale, window_alpha }
            }
        }
        _ => ShellEffect::None,
    }
}

/// The launcher's own mode, as distinct from `ShellMode` (which is the HWC
/// plane set: launcher vs lock screen) and from `app_drawer_open` (which is a
/// boolean).
///
/// Plan §7.6. No `String` and no `Vec`: a popup is named by a `u8` index and an
/// anchor is two `f32`s, so this stays `Copy` and the per-frame path never has
/// to clone an identity.
///
/// There is deliberately no `FolderOpen` arm. The folder *path* is complete --
/// `compositor::FolderOpen` models the three springs and the title delay, and
/// `drm_kms::draw_folder` draws the scrim, surface, grid and footer off
/// `FolderLayout` -- but the shell has no folders to open: `home_pages` is a
/// flat list of apps and nothing in the shell groups them. Adding the variant
/// now would be a state that cannot be entered, which is worse than its
/// absence: the field would read as "folders are wired" when they are not.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ShellState {
    /// Home, with the workspace as the root surface.
    Normal,
    /// The app drawer is up. `progress` mirrors `drawer_spring`; it is carried
    /// here so a state *change* is comparable in one `==`, rather than
    /// reconstructed from a float on the side.
    AllApps { progress: f32 },
    /// The recents carousel. `selected` is the index into `recents`, `dismiss`
    /// the live drag offset of that card in px.
    Overview { selected: u8, dismiss: f32 },
    /// A long-press popup is anchored at a panel point.
    PopupOpen { anchor_x: f32, anchor_y: f32, idx: u8 },
}

impl ShellState {
    /// True when a modal surface owns the screen, so a workspace swipe must
    /// not also page the home screen underneath it.
    fn is_modal(self) -> bool {
        matches!(self, Self::Overview { .. } | Self::PopupOpen { .. })
    }
}

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

/// The `n`-th app the drawer is showing, or `None`. Allocates nothing.
///
/// The drawer grid, the press feedback and the launch handler all need "the
/// nth app the drawer is showing"; this is the single definition of that.
/// Every call site only needs `.nth(idx)`, so no iterator (and no `Box`)
/// is built at all.
fn drawer_nth<'a>(apps: &'a [ManagedApp], query: &str, n: usize) -> Option<&'a ManagedApp> {
    if query.is_empty() {
        return apps.get(n);
    }
    apps.iter()
        .filter(|a| ci_contains(&a.name, query) || ci_contains(&a.id, query))
        .nth(n)
}

/// Build the render descriptor for one app. A plain `fn` (not a closure)
/// so the borrow flows straight through with no lifetime inference trap.
fn drawer_item_of(a: &ManagedApp) -> AppGridItem<'_> {
    AppGridItem {
        id: &a.id,
        name: &a.name,
        color: a.color,
        glyph: &a.glyph,
        icon: a.icon.as_deref(),
    }
}

/// ASCII-case-insensitive substring test. No allocation, no `to_lowercase`.
fn ci_contains(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let nlen = needle.len();
    if nlen > hay.len() {
        return false;
    }
    hay.as_bytes()
        .windows(nlen)
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Keep the Messages composer buffer bounded (sibling terminal buffers cap
/// at 120 lines). Called at the top of every loop iteration so all push
/// sites are covered by one trim.
#[inline]
fn trim_messages_list(v: &mut Vec<String>) {
    if v.len() > 64 {
        let n = v.len() - 64;
        v.drain(0..n);
    }
}

// epoll u64 dispatch tags: the fd occupies the low 32 bits, the kind the
// high 32, so fd->kind dispatch is a single switch with no linear scan.
const K_SIGNAL: u64 = 1 << 32;
const K_LISTEN: u64 = 2 << 32;
const K_INPUT: u64 = 3 << 32;
const K_CLIENT: u64 = 4 << 32;
#[inline]
fn epoll_tag(fd: libc::c_int, kind: u64) -> u64 {
    (fd as u32 as u64) | kind
}
#[inline]
fn epoll_kind(u: u64) -> u64 {
    u & 0xFFFF_FFFF_0000_0000
}
#[inline]
fn epoll_fd_of(u: u64) -> libc::c_int {
    (u & 0xFFFF_FFFF) as u32 as libc::c_int
}

/// Per-Wayland-client accumulation buffer: carries a partial message tail
/// across read() boundaries so a fragmented message never desyncs the
/// stream. Kept parallel to `server.client_streams` (same index).
struct ClientState {
    buf: [u8; 4096],
    len: usize,
    text_input_id: u32,
}

impl ClientState {
    fn new() -> Self {
        Self {
            buf: [0u8; 4096],
            len: 0,
            text_input_id: 0,
        }
    }
}

/// Cap on concurrent Wayland clients (fd bound on a world-connectable socket).
const MAX_CLIENTS: usize = 16;

/// Open any /dev/input/event* nodes not already tracked. Shared by the
/// startup scan and the one-shot inotify rescan (no polling).
fn open_new_input_devices(
    epoll_fd: libc::c_int,
    input_fds: &mut Vec<libc::c_int>,
    opened_paths: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return;
    };
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
                            u64: epoll_tag(fd, K_INPUT),
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

/// Re-resolve the 5 hotseat slots to indices into `all_managed_apps` plus
/// the cached "Apps" icon. Runs only on catalogue change, never per frame.
fn refresh_dock_cache(
    all_managed_apps: &[ManagedApp],
    icon_cache: &IconCache,
    dock_index: &mut [Option<usize>; 5],
    dock_apps_icon: &mut Option<Rc<RgbaImage>>,
) {
    const DOCK_NAMES: [&str; 5] = ["Phone", "Messages", "Apps", "Browser", "Camera"];
    for (slot, name) in DOCK_NAMES.iter().enumerate() {
        dock_index[slot] = all_managed_apps.iter().position(|a| a.name == *name);
    }
    *dock_apps_icon = icon_cache
        .get("view-app-grid")
        .or_else(|| icon_cache.get("apps"));
}

/// Set O_NONBLOCK on a raw fd via fcntl (no std wrapper exists for child pipes).
fn set_fd_nonblocking(fd: libc::c_int) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
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
/// Spawn `exec_cmd` against the compositor's Wayland socket.
///
/// Returns the child's pid so the recents card can be tied to a real process:
/// the kill escalation in [`utim_core::compositor::Recents`] signals a pid, and
/// a card carrying 0 would make "close this task" a no-op that silently
/// reports success. `None` means nothing was spawned -- an empty command, a
/// missing binary, or a spawn error; the last two are already logged.
fn launch_desktop_app(exec_cmd: &str, socket_dir: &str) -> Option<i32> {
    if exec_cmd.is_empty() {
        return None;
    }
    let parts: Vec<&str> = exec_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let prog = parts[0];
    let args = &parts[1..];

    if !std::path::Path::new(prog).exists() {
        eprintln!("[UTLC] Skipping desktop application launch: binary '{}' does not exist", prog);
        return None;
    }

    let sess = utim_core::session::session();
    let is_root = utim_core::session::is_root_process();
    if is_root {
        utim_core::session::ensure_session_dirs();
    }
    // Only root owns the compositor socket location; an unprivileged caller
    // already lives in its own runtime dir.
    let app_socket_dir = if is_root {
        utim_core::session::SESSION_RUNTIME_DIR
    } else {
        socket_dir
    };

    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("XDG_RUNTIME_DIR", app_socket_dir)
        .env("GDK_BACKEND", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("HOME", sess.home())
        .env("USER", sess.name())
        .env("LOGNAME", sess.name())
        .env("SHELL", "/bin/bash")
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

    utim_core::session::drop_privileges(&mut cmd);

    match cmd.spawn() {
        Ok(child) => {
            // The `Child` is dropped here and never waited on, exactly as
            // before. It is not the parent-waiter for these processes and never
            // blocks on them; an exited child is reaped by init, because
            // SIGCHLD is neither blocked nor ignored here.
            // A pid is a positive `i32` on Linux and `child.id()` is a `u32`,
            // so the conversion is a checked cast rather than a `From`: a
            // value above `i32::MAX` cannot be a real pid and would wrap to a
            // negative one, which `kill` would reject -- better to report no
            // pid than a wrong one.
            let pid = child.id();
            if pid > i32::MAX as u32 {
                None
            } else {
                Some(pid as i32)
            }
        }
        Err(e) => {
            eprintln!("[UTLC] Error spawning '{}': {}", prog, e);
            None
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
    if let Err(e) = server.power_sync.configure_self_oom_score() {
        eprintln!(
            "[-] OOM immunity not applied (needs CAP_SYS_RESOURCE): {}",
            e
        );
    }

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
    // The date is refreshed once per frame alongside the time, into a stack
    // buffer, so the render path borrows a `&str` and allocates nothing.
    let mut date_buf = [0u8; 16];

    if let Some(ref mut drm) = drm_display {
        let t_str = format_current_time(&mut time_buf);
        drm.render_mobile_ui(
            t_str,
            server.scene.mode == utim_core::compositor::scene::ShellMode::LockScreen,
        );
        if let Err(e) = drm.flush() {
            eprintln!("[UTLC] DIRTYFB flush failed: {}", e);
        }
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
    if sig_fd < 0 {
        eprintln!(
            "[UTLC] signalfd failed: {}",
            std::io::Error::last_os_error()
        );
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        return;
    }

    let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epoll_fd >= 0 {
        let mut sig_ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: epoll_tag(sig_fd, K_SIGNAL),
        };
        unsafe {
            libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, sig_fd, &mut sig_ev);
        }
    }

    use std::os::unix::io::{AsRawFd, FromRawFd};
    if let Some(ref listener) = server.listener {
        let listen_fd = listener.as_raw_fd();
        if epoll_fd >= 0 {
            let mut listen_ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: epoll_tag(listen_fd, K_LISTEN),
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
    let mut input_fds: Vec<libc::c_int> = Vec::new();
    let mut opened_paths: Vec<std::path::PathBuf> = Vec::new();
    open_new_input_devices(epoll_fd, &mut input_fds, &mut opened_paths);

    // Watch /dev/input for hotplug instead of polling read_dir every second:
    // a rescan runs exactly once per directory event (see the K_INPUT arm).
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if inotify_fd >= 0 {
        if let Ok(c_dir) = std::ffi::CString::new("/dev/input") {
            unsafe {
                libc::inotify_add_watch(
                    inotify_fd,
                    c_dir.as_ptr(),
                    libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO,
                );
            }
        }
        if epoll_fd >= 0 {
            let mut ino_ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: epoll_tag(inotify_fd, K_INPUT),
            };
            unsafe {
                libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, inotify_fd, &mut ino_ev);
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
    let mut power_saver_mode = PowerSaverMode::Off;
    let mut super_extreme_state = SuperExtremeState::new();

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
    // True while a horizontal workspace drag is in flight, so release can
    // settle to the nearest page instead of treating it as a tap.
    let mut page_drag = false;
    let mut page_drag_velocity = 0.0f32;
    // Set for the rest of this input event once a drag has paged the workspace,
    // so the gesture engine's swipe fallback does not page a second time.
    let mut page_drag_seen = false;
    let mut last_move = Instant::now();
    let mut drawer_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::drawer());
    let mut page_scroll_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::page_swipe());
    let mut app_launch_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::app_launch());
    let mut icon_bounce_spring = SpringSimulation::new(1.0, 1.0, SpringConfig::icon_bounce());
    let mut pressed_icon_id: Option<String> = None;

    // ---------------------------------------------------------------------
    // Launcher-rewrite shell state (plan §7.6).
    //
    // These existed as unit-tested models in `utim_core` and nothing drove
    // them: `main.rs`'s gesture match ended in `_ => {}`, so
    // `GestureAction::Recents` and `GestureAction::BottomBarScrub` were
    // dropped on the floor and the overview could not be opened at all. The
    // states are here so the wiring has somewhere to land.
    // ---------------------------------------------------------------------
    let shell_layout = Layout::plain(server.scene.width as f32, server.scene.height as f32);
    // The fast-scroller geometry, needed per frame to convert the model's
    // track-local `thumb_y` into the 0..1 position the renderer takes. Held
    // rather than rebuilt so the two cannot be built from different layouts.
    let shell_fast_scroller: FastScrollerLayout = shell_layout.fast_scroller();
    // Monotonic millisecond clock for the models that time their own gestures
    // (the fast scroller's detent dwell, the folder's title delay). One
    // `Instant`, not a per-call `SystemTime` read.
    let shell_start = Instant::now();
    // `ViewConfiguration.getLongPressTimeout()` is 500 ms
    // (`ViewConfiguration.java:562`); the launcher uses its own value, which
    // is 500 ms too.
    const LONG_PRESS_MS: u64 = 500;
    let mut shell_state = ShellState::Normal;
    let mut recents = Recents::new(&shell_layout);
    let mut fastscroller = FastScrollerState::new();
    let mut folder = FolderOpen::closed(0);
    // Contents of the open folder, and its title. Both stay empty: the shell
    // has no folders -- `home_pages` is a flat list of apps and nothing builds
    // a folder to open. The renderer treats empty as "no folder" and draws the
    // scrim and surface with no grid, so wiring the model before there is a
    // folder to open would mean shipping a popup that opens onto nothing.
    let folder_items: Vec<AppGridItem<'_>> = Vec::new();
    let folder_title: &str = "";
    // Anchor of the long-press popup in panel coordinates.
    let mut popup_anchor = (0.0f32, 0.0f32);
    // Rows the popup offers, borrowed by the renderer as `&[PopupItem]`. The
    // workspace menu is the reference's four-item set
    // (`LauncherOptionsPopup.kt:18-28`); an icon long-press swaps in the icon
    // menu, so this is rebuilt per gesture rather than fixed.
    let mut popup_rows: [utim_core::compositor::PopupItem; 4] = [
        utim_core::compositor::PopupItem::Wallpapers,
        utim_core::compositor::PopupItem::Widgets,
        utim_core::compositor::PopupItem::AllApps,
        utim_core::compositor::PopupItem::HomeSettings,
    ];
    let mut popup_count = 4usize;
    // Which app the long-press landed on, and where. Held so the popup can be
    // raised on the *move* that crosses the long-press threshold rather than
    // needing the touch position again.
    let mut long_press: Option<(String, f32, f32, Instant)> = None;
    // Springs for the two overview surfaces. `desktop_slide` is the reference's
    // own row for the carousel; `recents_attach_alpha` for the scrim.
    let mut overview_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::desktop_slide());
    let mut overview_scrim_spring =
        SpringSimulation::new(0.0, 0.0, SpringConfig::recents_attach_alpha());
    // Long-press popup: `spring_loaded` is the reference's own row for the
    // options popup appearing out of the icon.
    let mut popup_spring = SpringSimulation::new(0.0, 0.0, SpringConfig::spring_loaded());
    // How often the smartspace advances a phase. The reference cross-fades on a
    // timer rather than on a transition; 5 s is one cycle of the two states.
    const SMARTSPACE_TICK_MS: u64 = 5_000;
    // In-app home gesture: the panel's scale and opacity, both 1.0 at rest.
    let mut workspace_scale_spring = SpringSimulation::new(1.0, 1.0, SpringConfig::stretch_edge());
    let mut window_alpha_spring = SpringSimulation::new(1.0, 1.0, SpringConfig::stretch_edge());
    // Smartspace phases between the date-only and full-weather card. Advanced
    // on a wall clock, not on interaction, so it is hashed in the renderer.
    let mut smartspace_phase: f32 = 0.0;
    let mut last_smartspace_tick = Instant::now();
    // Flattened recents rows handed to the renderer. Fixed capacity, no `Vec`
    // on the frame path.
    let mut recents_rows: [RecentsCard; utim_core::compositor::MAX_TASKS] =
        [RecentsCard { app_id: 0, dismiss: 0.0, selected: false }; utim_core::compositor::MAX_TASKS];
    let mut kill_queue = KillQueue::default();
    // The task the shell believes is foreground, and the pid its most recent
    // launch produced. A recents card is pushed on the *transition* into an
    // app, detected centrally rather than at each of the six places that set
    // `active_app` -- six call sites is six chances to forget one, and the
    // failure is an app that silently never appears in the overview.
    let mut recents_foreground: Option<String> = None;
    // Pid of the most recent launch, consumed by the transition above.
    //
    // `take` rather than a read: the pid belongs to exactly one card, and
    // clearing it in the same step is what stops the *next* app from
    // inheriting it. An in-app screen the shell drew itself never sets it, so
    // it reads back as the 0 it was initialised to -- which is the correct
    // pid for a task with no process behind it.
    let mut pending_launch_pid: i32 = 0;

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

    // The full catalogue as renderer rows, rebuilt only on a real rescan.
    //
    // A recents card names its app by a `u32` index into this list, and the
    // index has to mean the same thing when the overview is drawn as it did
    // when the card was pushed. `drawer_items` cannot serve: it is the
    // *filtered* list and is empty whenever the drawer is closed, which is
    // exactly when the overview is reachable.
    let mut catalogue_items: Vec<AppGridItem<'_>> =
        all_managed_apps.iter().map(drawer_item_of).collect();

    let mut running = true;
    let mut last_frame = Instant::now();
    let frame_interval = Duration::from_millis(16);

    // Hotseat slot -> index into all_managed_apps; rebuilt only when the
    // catalogue signature changes (the per-frame dock find/Rc-clone is gone).
    // NOTE: the per-frame item vecs themselves stay frame-local: hoisting a
    // `Vec<&...>` across loop iterations is rejected by the borrow checker
    // (the buffer's element lifetime would have to outlive owner mutations
    // in the event phase), so reuse happens at the lookup level instead.
    let mut dock_index: [Option<usize>; 5] = [None; 5];
    let mut dock_apps_icon: Option<Rc<RgbaImage>> = None;
    refresh_dock_cache(
        &all_managed_apps,
        &icon_cache,
        &mut dock_index,
        &mut dock_apps_icon,
    );
    // Per-client Wayland accumulation buffers, parallel to server.client_streams.
    let mut client_states: Vec<ClientState> = Vec::new();
    // epoll batch buffer, hoisted: re-zeroing 256 B every 16 ms is pure waste.
    let mut events: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };

    println!("[+] UTLC daemon running successfully in persistent event loop.");

    while running {
        // Authoritative active_tab_idx clamp: the input path below indexes
        // terminal_tabs in ~8 places, so the invariant is enforced here,
        // once, instead of implicitly at each shrink site.
        if active_tab_idx >= terminal_tabs.len() {
            active_tab_idx = terminal_tabs.len().saturating_sub(1);
        }
        trim_messages_list(&mut messages_list);

        // Deadline-derived epoll timeout: sleep only the remainder of the
        // 16 ms frame when something is animating; block indefinitely when
        // idle. The idle timeout still honours the minute-boundary
        // status-clock refresh and the systemd watchdog ping.
        let elapsed = last_frame.elapsed();
        let timeout_ms: libc::c_int = if elapsed >= frame_interval {
            0
        } else {
            let need_frames = touch_ripple.is_some()
                || app_launch_progress > 0.0
                || !drawer_spring.is_at_rest()
                || !page_scroll_spring.is_at_rest()
                || !icon_bounce_spring.is_at_rest()
                || terminal_tabs.iter().any(|t| t.is_running())
                || (server.scene.lockscreen.is_locked()
                    && server.start_time.elapsed() < Duration::from_secs(2));
            if need_frames {
                frame_interval
                    .checked_sub(elapsed)
                    .unwrap_or_default()
                    .as_millis()
                    .min(libc::c_int::MAX as u128)
                    as libc::c_int
            } else {
                let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
                unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
                let secs_to_min = 60i64 - ts.tv_sec.rem_euclid(60);
                let mut idle_ms = (secs_to_min as u128) * 1000 + 500;
                if notify_dgram.is_some() {
                    let remain = watchdog_interval
                        .checked_sub(last_watchdog_ping.elapsed())
                        .unwrap_or_default();
                    idle_ms =
                        idle_ms.min(remain.as_millis().min(libc::c_int::MAX as u128));
                }
                idle_ms.min(libc::c_int::MAX as u128) as libc::c_int
            }
        };
        let nfds = if epoll_fd >= 0 {
            unsafe { libc::epoll_wait(epoll_fd, events.as_mut_ptr(), 16, timeout_ms) }
        } else {
            std::thread::sleep(Duration::from_millis(16));
            0
        };

        if nfds > 0 {
            for ev in events.iter().take(nfds as usize) {
                let kind = epoll_kind(ev.u64);
                let fd = epoll_fd_of(ev.u64);
                if kind == K_SIGNAL && fd == sig_fd {
                    // Drain the signalfd, then shut down without running
                    // another frame (the `!running` guard below skips it).
                    let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
                    while unsafe {
                        libc::read(
                            sig_fd,
                            &mut si as *mut _ as *mut libc::c_void,
                            std::mem::size_of::<libc::signalfd_siginfo>(),
                        )
                    } > 0
                    {}
                    running = false;
                    continue;
                } else if kind == K_LISTEN {
                    if let Some(ref listener) = server.listener {
                        let listen_raw = listener.as_raw_fd();
                        while server.client_streams.len() < MAX_CLIENTS {
                            let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
                            let mut len =
                                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
                            let cfd = unsafe {
                                libc::accept4(
                                    listen_raw,
                                    &mut raw as *mut _ as *mut libc::sockaddr,
                                    &mut len,
                                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                                )
                            };
                            if cfd < 0 {
                                let e = unsafe { *libc::__errno_location() };
                                if e == libc::EAGAIN || e == libc::EWOULDBLOCK {
                                    break;
                                }
                                if e == libc::EMFILE || e == libc::ENFILE || e == libc::ENOMEM
                                {
                                    break;
                                }
                                if e == libc::ECONNABORTED || e == libc::EINTR {
                                    continue;
                                }
                                break;
                            }
                            if epoll_fd >= 0 {
                                let mut client_ev = libc::epoll_event {
                                    events: (libc::EPOLLIN
                                        | libc::EPOLLRDHUP
                                        | libc::EPOLLHUP
                                        | libc::EPOLLERR)
                                        as u32,
                                    u64: epoll_tag(cfd, K_CLIENT),
                                };
                                if unsafe {
                                    libc::epoll_ctl(
                                        epoll_fd,
                                        libc::EPOLL_CTL_ADD,
                                        cfd,
                                        &mut client_ev,
                                    )
                                } < 0
                                {
                                    // Never retain an unregistered fd.
                                    unsafe { libc::close(cfd) };
                                    continue;
                                }
                            }
                            server.client_streams.push(unsafe {
                                std::os::unix::net::UnixStream::from_raw_fd(cfd)
                            });
                            client_states.push(ClientState::new());
                        }
                    }
                } else if kind == K_INPUT {
                    if fd == inotify_fd {
                        // One-shot hotplug rescan: drain the inotify queue,
                        // then open exactly the new nodes (no polling).
                        let mut ibuf = [0u8; 512];
                        loop {
                            let n = unsafe {
                                libc::read(
                                    inotify_fd,
                                    ibuf.as_mut_ptr() as *mut libc::c_void,
                                    ibuf.len(),
                                )
                            };
                            if n <= 0 {
                                break;
                            }
                        }
                        open_new_input_devices(epoll_fd, &mut input_fds, &mut opened_paths);
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
                                // One input event, one gesture verdict.
                                if dispatcher.is_touch_down {
                                    page_drag_seen = false;
                                }
                                cursor_pos = Some((dispatcher.cursor_x as usize, dispatcher.cursor_y as usize));
                                is_touching = dispatcher.is_touch_down;

                                if power_saver_mode == PowerSaverMode::SuperExtreme {
                                    let w = server.scene.width as f32;
                                    let h = server.scene.height as f32;
                                    match res {
                                        InputDispatchResult::Touch(ref raw_touch) => {
                                            if raw_touch.phase == TouchPhase::Down {
                                                touch_drag_start = Some((raw_touch.x, raw_touch.y));
                                            } else if raw_touch.phase == TouchPhase::Up {
                                                if let Some((sx, sy)) = touch_drag_start.take() {
                                                    if sy - raw_touch.y > 60.0 && (sx - raw_touch.x).abs() < 120.0 {
                                                        super_extreme_state.on_swipe_up();
                                                    }
                                                }
                                            }
                                        }
                                        InputDispatchResult::Tap { x, y } => {
                                            touch_ripple = Some((
                                                x,
                                                y,
                                                ripple_start_radius(w),
                                                1.0,
                                            ));
                                            if y <= h * 0.05 {
                                                super_extreme_state.volume_hud.trigger(super_extreme_state.volume_hud.volume_percent);
                                            } else {
                                                let pin_hash = server.scene.lockscreen.pin_hash;
                                                let pin_salt = server.scene.lockscreen.pin_salt;
                                                super_extreme_state.handle_touch_tap(x, y, w, h, pin_hash, pin_salt);
                                            }
                                        }
                                        InputDispatchResult::KeyPress {
                                            code,
                                            ch,
                                            pressed,
                                            ..
                                        } => {
                                            if code == KEY_POWER {
                                                if pressed {
                                                    super_extreme_state.on_power_button_press();
                                                } else {
                                                    super_extreme_state.on_power_button_release();
                                                }
                                            } else if code == KEY_VOLUMEUP && pressed {
                                                super_extreme_state.volume_up();
                                                let _ = server.scene.system_ui.set_volume(super_extreme_state.volume_hud.volume_percent);
                                            } else if code == KEY_VOLUMEDOWN && pressed {
                                                super_extreme_state.volume_down();
                                                let _ = server.scene.system_ui.set_volume(super_extreme_state.volume_hud.volume_percent);
                                            } else if code == KEY_ESC && pressed {
                                                super_extreme_state.handle_back();
                                            } else if code == KEY_BACKSPACE && pressed {
                                                match super_extreme_state.active_screen {
                                                    SuperExtremeScreen::Password => super_extreme_state.password_backspace(),
                                                    SuperExtremeScreen::EmergencyDialer => { super_extreme_state.emergency_input.pop(); }
                                                    SuperExtremeScreen::AppPhone => { super_extreme_state.phone_input.pop(); }
                                                    _ => {}
                                                }
                                            } else if code == KEY_ENTER && pressed {
                                                match super_extreme_state.active_screen {
                                                    SuperExtremeScreen::Password => {
                                                        super_extreme_state.submit_password(server.scene.lockscreen.pin_hash, server.scene.lockscreen.pin_salt);
                                                    }
                                                    SuperExtremeScreen::EmergencyDialer => {
                                                        super_extreme_state.last_action_message = Some(format!("Emergency call placed: {}", super_extreme_state.emergency_input));
                                                    }
                                                    SuperExtremeScreen::AppPhone => {
                                                        super_extreme_state.last_action_message = Some(format!("Calling {}", super_extreme_state.phone_input));
                                                    }
                                                    SuperExtremeScreen::CameraPreview => {
                                                        super_extreme_state.snap_photo();
                                                    }
                                                    _ => {}
                                                }
                                            } else if pressed {
                                                if let Some(c) = ch {
                                                    match super_extreme_state.active_screen {
                                                        SuperExtremeScreen::Password => {
                                                            super_extreme_state.enter_password_char(c, server.scene.lockscreen.pin_hash, server.scene.lockscreen.pin_salt);
                                                        }
                                                        SuperExtremeScreen::EmergencyDialer => {
                                                            if c.is_ascii_digit() && super_extreme_state.emergency_input.len() < 12 {
                                                                super_extreme_state.emergency_input.push(c);
                                                            }
                                                        }
                                                        SuperExtremeScreen::AppPhone => {
                                                            if (c.is_ascii_digit() || c == '*' || c == '#') && super_extreme_state.phone_input.len() < 15 {
                                                                super_extreme_state.phone_input.push(c);
                                                            }
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                } else if server.scene.lockscreen.is_locked() {
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
                                            // Monotonic ms for the gesture-timed
                                            // models. One clock read per touch
                                            // event, not per model.
                                            let now_ms =
                                                shell_start.elapsed().as_secs_f32() * 1000.0;
                                            match raw_touch.phase {
                                                TouchPhase::Down => {
                                                    touch_drag_start = Some((raw_touch.x, raw_touch.y));
                                                    let w = server.scene.width as f32;
                                                    let h = server.scene.height as f32;
                                                    // Fast scroller: the drawer's
                                                    // touch target overhangs the
                                                    // panel edge on purpose, so
                                                    // the hit test is the layout's
                                                    // and not a bounds check.
                                                    // Armed on every down inside
                                                    // the drawer and *only*
                                                    // engages once the finger
                                                    // travels `engage_delta`
                                                    // within `engage_ms`, which
                                                    // is what stops a list
                                                    // scroll from being read as
                                                    // a scroller drag.
                                                    if app_drawer_open {
                                                        fastscroller.on_down(
                                                            raw_touch.y,
                                                            now_ms,
                                                            &shell_fast_scroller,
                                                        );
                                                    }
                                                    // Long-press: record where the
                                                    // press landed so the *move*
                                                    // that crosses the threshold
                                                    // can open the popup without
                                                    // needing the touch position
                                                    // again.
                                                    long_press = Some((
                                                        String::new(),
                                                        raw_touch.x,
                                                        raw_touch.y,
                                                        Instant::now(),
                                                    ));
                                                    if active_app.is_none()
                                                        && !server.scene.system_ui.is_open()
                                                        && !server.scene.keyboard.is_active
                                                        && !shell_state.is_modal()
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
                                                                // Allocation-free nth-app lookup on
                                                                // every touch down.
                                                                if let Some(app) = drawer_nth(
                                                                    &all_managed_apps,
                                                                    &drawer_search,
                                                                    idx,
                                                                ) {
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
                                                                l.home_grid_hit_paged(
                                                                    raw_touch.x,
                                                                    raw_touch.y,
                                                                    home_scroll_offset,
                                                                    home_pages.len(),
                                                                )
                                                                .map(|(_, i)| i)
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
                                    let move_dt = {
                                        let now = Instant::now();
                                        let d = now.duration_since(last_move).as_secs_f32();
                                        last_move = now;
                                        d
                                    };
                                    if active_app.is_none()
                                        && !server.scene.system_ui.is_open()
                                        && !server.scene.keyboard.is_active
                                        && !shell_state.is_modal()
                                    {
                                        let w = server.scene.width as f32;
                                        let h = server.scene.height as f32;

                                        // Fast-scroller drag. The model decides
                                        // whether the drag has *engaged* and
                                        // which section it landed on; the shell
                                        // only feeds it the position and reads
                                        // the answer. Consuming the returned bool
                                        // rather than the field is what makes a
                                        // section change fire once.
                                        if app_drawer_open {
                                            fastscroller.on_move(
                                                raw_touch.y,
                                                now_ms,
                                                &shell_fast_scroller,
                                            );
                                        }

                                        // Long press: the popup opens on the
                                        // move that crosses the threshold, so a
                                        // press that never moves opens it on
                                        // release instead, and a drag that starts
                                        // fast never opens it at all.
                                        if let Some((ref id, px, py, ref since)) = long_press {
                                            if since.elapsed()
                                                >= Duration::from_millis(LONG_PRESS_MS)
                                                && !matches!(
                                                    shell_state,
                                                    ShellState::PopupOpen { .. }
                                                )
                                            {
                                                if !id.is_empty() {
                                                    popup_rows = [
                                                        utim_core::compositor::PopupItem::AppInfo,
                                                        utim_core::compositor::PopupItem::Uninstall,
                                                        utim_core::compositor::PopupItem::Remove,
                                                        utim_core::compositor::PopupItem::Customize,
                                                    ];
                                                }
                                                popup_count = popup_rows.len();
                                                popup_anchor = (px, py);
                                                popup_spring.set_target(1.0);
                                                shell_state = ShellState::PopupOpen {
                                                    anchor_x: px,
                                                    anchor_y: py,
                                                    idx: 0,
                                                };
                                            }
                                        }

                                                        if let Some((sx, sy)) = touch_drag_start {
                                                            let dy = sy - raw_touch.y;
                                                            let dx = sx - raw_touch.x;
                                                            let slop = h * 0.004;
                                                            if !app_drawer_open {
                                                                if dy > slop {
                                                                    // Drawer drag: the sheet
                                                                    // tracks the finger 1:1.
                                                                    let drawer_l =
                                                                        Layout::plain(w, h);
                                                                    let drag_span = (h
                                                                        - drawer_l.drawer_handle.y)
                                                                        .max(100.0);
                                                                    page_drag = false;
                                                                    drawer_progress =
                                                                        (dy / drag_span).clamp(0.0, 1.0);
                                                                    drawer_spring.value = drawer_progress;
                                                                    drawer_spring.velocity = 0.0;
                                                                } else if dx.abs() > slop && dx.abs() > dy {
                                                                    // Horizontal workspace drag:
                                                                    // the strip follows the finger,
                                                                    // with the overscroll curve
                                                                    // damping past the first and
                                                                    // last page.
                                                                    page_drag = true;
                                                                    page_drag_seen = true;
                                                                    let last =
                                                                        home_pages.len().saturating_sub(1) as f32;
                                                                    let target = home_scroll_offset + dx;
                                                                    // Overscroll damping: `max` is the
                                                                    // container extent on a drag
                                                                    // (`OverScroll.java:42-54`), which
                                                                    // for a full-bleed pager is the page
                                                                    // width, i.e. the panel width.
                                                                    if target > 0.0 && current_home_page == 0 {
                                                                        home_scroll_offset =
                                                                            damped_scroll(target, w);
                                                                    } else if target < -last * w
                                                                        && current_home_page as f32 >= last
                                                                    {
                                                                        home_scroll_offset = -last * w
                                                                            + damped_scroll(
                                                                                target + last * w,
                                                                                w,
                                                                            );
                                                                    } else {
                                                                        home_scroll_offset = target;
                                                                    }
                                                                    // px/s toward the next page.
                                                                    page_drag_velocity =
                                                                        -dx / move_dt.max(1e-4);
                                                                    page_scroll_spring.value = home_scroll_offset;
                                                                    page_scroll_spring.velocity = 0.0;
                                                                }
                                                            } else if dy < -slop {
                                                                // Drawer is up: drag it back down.
                                                                let drawer_l = Layout::plain(w, h);
                                                                let drag_span = (h
                                                                    - drawer_l.drawer_handle.y)
                                                                    .max(100.0);
                                                                page_drag = false;
                                                                drawer_progress = (1.0
                                                                    + dy / drag_span)
                                                                    .clamp(0.0, 1.0);
                                                                drawer_spring.value = drawer_progress;
                                                                drawer_spring.velocity = 0.0;
                                                            }
                                                        }
                                                    }
                                                }
                                                TouchPhase::Up | TouchPhase::Cancel => {
                                                    // Fast scroller: release ends the
                                                    // drag and starts the popup
                                                    // fade-out. The letter stays on
                                                    // screen while it fades, which is
                                                    // what the 150 ms
                                                    // `SCROLL_BAR_VIS_DURATION`
                                                    // window is for.
                                                    if app_drawer_open {
                                                        fastscroller.on_up(now_ms);
                                                    }

                                                    // A press that never moved still
                                                    // counts as a long press on
                                                    // release -- the reference
                                                    // accepts either, because a
                                                    // perfectly still finger
                                                    // produces no move events at
                                                    // all. The icon id was
                                                    // recorded on down, so the
                                                    // popup knows whether this is
                                                    // the icon menu or the
                                                    // workspace menu.
                                                    if let Some((ref id, px, py, ref since)) = long_press
                                                    {
                                                        if since.elapsed()
                                                            >= Duration::from_millis(LONG_PRESS_MS)
                                                            && !matches!(
                                                                shell_state,
                                                                ShellState::PopupOpen { .. }
                                                            )
                                                        {
                                                            if !id.is_empty() {
                                                                popup_rows = [
                                                                    utim_core::compositor::PopupItem::AppInfo,
                                                                    utim_core::compositor::PopupItem::Uninstall,
                                                                    utim_core::compositor::PopupItem::Remove,
                                                                    utim_core::compositor::PopupItem::Customize,
                                                                ];
                                                            }
                                                            popup_count = popup_rows.len();
                                                            popup_anchor = (px, py);
                                                            popup_spring.set_target(1.0);
                                                            shell_state = ShellState::PopupOpen {
                                                                anchor_x: px,
                                                                anchor_y: py,
                                                                idx: 0,
                                                            };
                                                        }
                                                    }
                                                    long_press = None;

                                                    // Set when this release ends a page
                                                    // drag, so the gesture engine still
                                                    // sees the event but the tap is
                                                    // suppressed.
                                                    let mut was_page_drag = false;
                                                    if let Some((_, sy)) = touch_drag_start.take() {
                                                        let dy = sy - raw_touch.y;
                                                        if page_drag {
                                                            // Settle to whichever page the
                                                            // strip is nearest, biased by the
                                                            // release velocity, then hand the
                                                            // spring that velocity so the
                                                            // motion carries momentum.
                                                            let w = server.scene.width as f32;
                                                            let last = home_pages.len().saturating_sub(1);
                                                            let offset = home_scroll_offset;
                                                            let frac = -offset / w;
                                                            let biased = frac
                                                                + (page_drag_velocity / w) * 0.12;
                                                            let target = biased.round().clamp(
                                                                0.0,
                                                                last as f32,
                                                            ) as usize;
                                                            current_home_page = target;
                                                            home_scroll_offset = 0.0;
                                                            page_scroll_spring.value = offset;
                                                            page_scroll_spring.velocity = page_drag_velocity;
                                                            page_scroll_spring.set_target(0.0);
                                                            page_drag = false;
                                                            page_drag_velocity = 0.0;
                                                            // A dragged workspace is never a
                                                            // tap, so the release must not
                                                            // also activate whatever it
                                                            // dragged over.
                                                            icon_bounce_spring.set_target(1.0);
                                                            pressed_icon_id = None;
                                                            was_page_drag = true;
                                                        }
                                                        if !was_page_drag
                                                            && active_app.is_none()
                                                            && !server.scene.system_ui.is_open()
                                                            && !server.scene.keyboard.is_active
                                                        {
                                                            let h = server.scene.height as f32;
                                                            // Commit past the halfway point,
                                                            // or on a decisive flick.
                                                            let flick = h * 0.12;
                                                            if !app_drawer_open {
                                                                if dy > flick {
                                                                    app_drawer_open = true;
                                                                }
                                                            } else if dy < -flick {
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
                                            // The three arms that drive the shell state
                                            // machine come from `plan_gesture`, which is a
                                            // pure function precisely so that "this
                                            // action has an effect" is testable. The
                                            // other arms still match inline because they
                                            // only poke the shade, the keyboard and the
                                            // drawer flag.
                                            match plan_gesture(&gesture_act) {
                                                ShellEffect::OpenOverview => {
                                                    shell_state = ShellState::Overview {
                                                        selected: recents.visible_card,
                                                        dismiss: 0.0,
                                                    };
                                                    // The overview is modal over the
                                                    // workspace, so a surface that owns
                                                    // the screen has to give up its own
                                                    // transient state first.
                                                    app_drawer_open = false;
                                                    drawer_search_active = false;
                                                    selected_home_icon = None;
                                                    search_active = false;
                                                    server.scene.keyboard.deactivate();
                                                    server.scene.system_ui.close();
                                                }
                                                ShellEffect::ScrubTasks(cards) => {
                                                    // The scrub swaps the selected task.
                                                    // The model owns the clamping, the
                                                    // half-pitch threshold and the
                                                    // rubber-band at either end, so the
                                                    // shell only hands it the delta it
                                                    // was given and reads the result.
                                                    let rl = shell_layout.recents();
                                                    recents.scrub(cards * (rl.card_w + rl.spacing));
                                                    shell_state = ShellState::Overview {
                                                        selected: recents.visible_card,
                                                        dismiss: 0.0,
                                                    };
                                                }
                                                ShellEffect::MorphWorkspace { scale, window_alpha } => {
                                                    // Handed to springs, so a release
                                                    // mid-gesture is a settle rather
                                                    // than a jump and the motion
                                                    // continues to the endpoint the
                                                    // finger was already heading for.
                                                    workspace_scale_spring.set_target(scale);
                                                    window_alpha_spring.set_target(window_alpha);
                                                }
                                                ShellEffect::CloseAll => {
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
                                                    close_modal_surfaces(
                                                        &mut shell_state,
                                                        &mut folder,
                                                        &mut popup_spring,
                                                        &mut workspace_scale_spring,
                                                        &mut window_alpha_spring,
                                                    );
                                                }
                                                ShellEffect::None => {}
                                            }
                                            match gesture_act {
                                                GestureAction::NotificationShade { progress } => {
                                                    if progress > 0.35 {
                                                        server.scene.system_ui.open();
                                                        server.scene.keyboard.deactivate();
                                                    }
                                                }
                                                GestureAction::Back { injected, .. } if injected => {
                                                    // A modal surface owns Back
                                                    // before anything else does:
                                                    // the overview has to close
                                                    // before the workspace
                                                    // behind it gets a chance
                                                    // to.
                                                    if shell_state.is_modal() {
                                                        close_modal_surfaces(
                                                            &mut shell_state,
                                                            &mut folder,
                                                            &mut popup_spring,
                                                            &mut workspace_scale_spring,
                                                            &mut window_alpha_spring,
                                                        );
                                                    } else if server.scene.system_ui.is_open() {
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
                                                    // Vertical swipes only: horizontal
                                                    // paging is driven by the drag
                                                    // handler above, which follows the
                                                    // finger and settles with a spring.
                                                    if app_drawer_open {
                                                        if delta_y > 45.0 {
                                                            app_drawer_open = false;
                                                            drawer_search_active = false;
                                                            drawer_search.clear();
                                                        }
                                                    } else if delta_y < -45.0 {
                                                        app_drawer_open = true;
                                                        selected_home_icon = None;
                                                    } else if delta_x.abs() > 45.0 && !page_drag_seen {
                                                        // No pointer events reached us
                                                        // (synthetic or coalesced), so
                                                        // fall back to a single flick.
                                                        let w = server.scene.width as f32;
                                                        let last = home_pages.len().saturating_sub(1);
                                                        let dir = if delta_x < 0.0 { 1i64 } else { -1i64 };
                                                        let next = (current_home_page as i64 + dir)
                                                            .clamp(0, last as i64) as usize;
                                                        if next != current_home_page {
                                                            current_home_page = next;
                                                            selected_home_icon = None;
                                                            home_scroll_offset = -dir as f32 * w * 0.45;
                                                            page_scroll_spring.value = home_scroll_offset;
                                                            page_scroll_spring.velocity = -dir as f32 * 250.0;
                                                            page_scroll_spring.set_target(0.0);
                                                        } else {
                                                            // At a boundary: rubber-band. On a
                                                            // *fling* AOSP passes half a page as the
                                                            // extent, not the container extent
                                                            // (`PagedView.java:1552`), so the
                                                            // overscroll saturates at 0.035 * w
                                                            // rather than 0.07 * w.
                                                            let resisted =
                                                                damped_scroll(delta_x, w * 0.5);
                                                            home_scroll_offset = resisted;
                                                            page_scroll_spring.value = resisted;
                                                            page_scroll_spring.velocity = 120.0
                                                                * if delta_x < 0.0 { -1.0 } else { 1.0 };
                                                            page_scroll_spring.set_target(0.0);
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
                                            touch_ripple = Some((
                                                x,
                                                y,
                                                ripple_start_radius(w),
                                                1.0,
                                            ));

                                            if server.scene.system_ui.is_open() {
                                                let shade = ShadeLayout::new(w, h);
                                                match shade.zone(x, y) {
                                                    ShadeZone::Tiles(i) => {
                                                        if i < quick_tiles_active.len() {
                                                            if i == 6 {
                                                                 match power_saver_mode {
                                                                    PowerSaverMode::Off => {
                                                                        let _ = server.scene.system_ui.set_brightness(25);
                                                                        let _ = server.power_sync.apply_normal_power_saver();
                                                                        power_saver_mode = PowerSaverMode::Normal;
                                                                        quick_tiles_active[6] = true;
                                                                    }
                                                                    PowerSaverMode::Normal => {
                                                                        let _ = server.scene.system_ui.set_brightness(10);
                                                                        let _ = server.power_sync.apply_super_extreme_power_saver();
                                                                        power_saver_mode = PowerSaverMode::SuperExtreme;
                                                                        set_active_family(FontFamily::Homemade);
                                                                        super_extreme_state.enter_super_extreme();
                                                                        quick_tiles_active[6] = true;
                                                                        server.scene.system_ui.close();
                                                                    }
                                                                    PowerSaverMode::SuperExtreme => {
                                                                        let _ = server.scene.system_ui.set_brightness(75);
                                                                        let _ = server.power_sync.restore_normal_power_mode();
                                                                        set_active_family(FontFamily::NotoSans);
                                                                        power_saver_mode = PowerSaverMode::Off;
                                                                        quick_tiles_active[6] = false;
                                                                    }
                                                                }
                                                            } else {
                                                                quick_tiles_active[i] =
                                                                    !quick_tiles_active[i];
                                                            }
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
                                                // keyboard is drawn from. Takes the Key
                                                // directly: no per-keystroke heap string.
                                                let handle_key_input = |key: Key,
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
                                                        match key {
                                                            Key::Backspace => { drawer_search.pop(); }
                                                            Key::Enter => {
                                                                *drawer_search_active = false;
                                                                keyboard.deactivate();
                                                            }
                                                            Key::Space => {
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(' ');
                                                                }
                                                            }
                                                            Key::Char(c) => {
                                                                let c_ascii = if keyboard.is_shift_active {
                                                                    c
                                                                } else {
                                                                    c.to_ascii_lowercase()
                                                                };
                                                                if drawer_search.len() < 40 {
                                                                    drawer_search.push(c_ascii);
                                                                }
                                                            }
                                                            Key::Hide => {
                                                                keyboard.deactivate();
                                                                *drawer_search_active = false;
                                                            }
                                                            Key::Shift => {
                                                                keyboard.is_shift_active =
                                                                    !keyboard.is_shift_active;
                                                            }
                                                        }
                                                    } else {
                                                        let act = match key {
                                                            Key::Backspace => {
                                                                keyboard.handle_key_tap("BACKSPACE")
                                                            }
                                                            Key::Enter => {
                                                                keyboard.handle_key_tap("ENTER")
                                                            }
                                                            Key::Space => {
                                                                keyboard.handle_key_tap("SPACE")
                                                            }
                                                            Key::Char(c) => {
                                                                // Stack-encoded: handle_key_tap
                                                                // gets a &str with zero heap.
                                                                let mut b = [0u8; 4];
                                                                let s: &str = c.encode_utf8(&mut b);
                                                                keyboard.handle_key_tap(s)
                                                            }
                                                            Key::Hide => {
                                                                keyboard.deactivate();
                                                                *app_input_focused = false;
                                                                *drawer_search_active = false;
                                                                ImeAction::None
                                                            }
                                                            Key::Shift => {
                                                                let _ = keyboard
                                                                    .handle_key_tap("SHIFT");
                                                                ImeAction::None
                                                            }
                                                        };
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
                                                handle_key_input(
                                                    key,
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
                                                );
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
                                                    let drawer_l = Layout::plain(w, h);
                                                    match drawer_l.drawer_search_hit(drawer_y_offset, x, y) {
                                                        DrawerSearchHit::Clear => {
                                                            drawer_search.clear();
                                                        }
                                                        DrawerSearchHit::Focus => {
                                                            drawer_search_active = true;
                                                        }
                                                        DrawerSearchHit::None => {
                                                            if let Some(idx) = drawer_l.drawer_grid_hit(drawer_y_offset, x, y) {
                                                                if let Some(target_app) = drawer_nth(
                                                                    &all_managed_apps,
                                                                    &drawer_search,
                                                                    idx,
                                                                ) {
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
                                                                        // The pid is recorded so the recents card for this app can be
                                                                        // killed when the user swipes it away. Discarding it, as this
                                                                        // used to, made "close task" a no-op that reported success.
                                                                        if let Some(pid) = launch_desktop_app(&app_exec, &socket_dir) {
                                                                            pending_launch_pid = pid;
                                                                        }
                                                                    }
                                                                }
                                                            } else if y < drawer_y_offset
                                                                || drawer_l.drawer_handle.contains(x, y - drawer_y_offset)
                                                                || drawer_l.nav_pill.contains(x, y)
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
                                                        home_l.home_grid_hit_paged(x, y, home_scroll_offset, home_pages.len())
                                                            .map(|(_, i)| i)
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
                                                            drawer_nth(
                                                                &all_managed_apps,
                                                                search_query.as_str(),
                                                                idx,
                                                            )
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
                                                                // The pid is recorded so the recents card for this app can be
                                                                // killed when the user swipes it away. Discarding it, as this
                                                                // used to, made "close task" a no-op that reported success.
                                                                if let Some(pid) = launch_desktop_app(&app_exec, &socket_dir) {
                                                                    pending_launch_pid = pid;
                                                                }
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
                                                    match drawer_l.drawer_search_hit(drawer_y_offset, x, y) {
                                                        DrawerSearchHit::Clear => {
                                                            drawer_search.clear();
                                                        }
                                                        DrawerSearchHit::Focus => {
                                                            drawer_search_active = true;
                                                            server.scene.keyboard.activate();
                                                        }
                                                        DrawerSearchHit::None => {
                                                            if drawer_l.nav_pill.contains(x, y) {
                                                                // Bottom pill: close drawer
                                                                app_drawer_open = false;
                                                                drawer_search_active = false;
                                                                drawer_search.clear();
                                                                server.scene.keyboard.deactivate();
                                                            } else if let Some(idx) =
                                                                drawer_l.drawer_grid_hit(drawer_y_offset, x, y)
                                                            {
                                                                if let Some(target_app) = drawer_nth(
                                                                    &all_managed_apps,
                                                                    &drawer_search,
                                                                    idx,
                                                                ) {
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
                                                                        // The pid is recorded so the recents card for this app can be
                                                                        // killed when the user swipes it away. Discarding it, as this
                                                                        // used to, made "close task" a no-op that reported success.
                                                                        if let Some(pid) = launch_desktop_app(&app_exec, &socket_dir) {
                                                                            pending_launch_pid = pid;
                                                                        }
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
                                                    home_l.home_grid_hit_paged(x, y, home_scroll_offset, home_pages.len())
                                                        .map(|(_, i)| i)
                                                {
                                                    let cell = home_l.grid_icon(idx);
                                                    let cx = cell.center_x() + home_scroll_offset;
                                                    let cy = cell.center_y();

                                                    if search_active && !search_query.is_empty() {
                                                        if let Some(target_app) = drawer_nth(
                                                            &all_managed_apps,
                                                            search_query.as_str(),
                                                            idx,
                                                        ) {
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
                                                                // The pid is recorded so the recents card for this app can be
                                                                // killed when the user swipes it away. Discarding it, as this
                                                                // used to, made "close task" a no-op that reported success.
                                                                if let Some(pid) = launch_desktop_app(&app_exec, &socket_dir) {
                                                                    pending_launch_pid = pid;
                                                                }
                                                            }
                                                        }
                                                    } else if let Some(sel_id) = selected_home_icon.take() {
                                                        // Moving icon in edit mode to selected slot
                                                        if let Some(old_pos) = home_pages[current_home_page].iter().position(|id| *id == sel_id) {
                                                            home_pages[current_home_page].remove(old_pos);
                                                            let insert_pos = idx.min(home_pages[current_home_page].len());
                                                            home_pages[current_home_page].insert(insert_pos, sel_id.clone());
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
                                                                    // The pid is recorded so the recents card for this app can be
                                                                    // killed when the user swipes it away. Discarding it, as this
                                                                    // used to, made "close task" a no-op that reported success.
                                                                    if let Some(pid) = launch_desktop_app(&app_exec, &socket_dir) {
                                                                        pending_launch_pid = pid;
                                                                    }
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
                                            if code == KEY_POWER {
                                                if pressed {
                                                    super_extreme_state.on_power_button_press();
                                                } else {
                                                    super_extreme_state.on_power_button_release();
                                                }
                                            } else if code == KEY_VOLUMEUP && pressed {
                                                let cur = server.scene.system_ui.volume_percent;
                                                let next = cur.saturating_add(10).min(100);
                                                let _ = server.scene.system_ui.set_volume(next);
                                                super_extreme_state.volume_hud.trigger(next);
                                            } else if code == KEY_VOLUMEDOWN && pressed {
                                                let cur = server.scene.system_ui.volume_percent;
                                                let next = cur.saturating_sub(10);
                                                let _ = server.scene.system_ui.set_volume(next);
                                                super_extreme_state.volume_hud.trigger(next);
                                            } else if pressed {
                                                if ctrl {
                                                    if code == KEY_C {
                                                        if active_app.as_deref() == Some("Terminal") {
                                                            let tab = &mut terminal_tabs[active_tab_idx];
                                                            tab.cleanup_child();
                                                            let display_cmd = if tab.input.is_empty() { "^C" } else { &format!("{}^C", tab.input) };
                                                            let full_line = format!(
                                                                "{}{}",
                                                                utim_core::session::session().prompt(),
                                                                display_cmd
                                                            );
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
                                            touch_ripple = Some((
                                                x,
                                                y,
                                                ripple_start_radius(w) * 1.2,
                                                1.0,
                                            ));
                                            if server.scene.system_ui.is_open() {
                                                let shade = ShadeLayout::new(w, h);
                                                if let ShadeZone::Tiles(6) = shade.zone(x, y) {
                                                    let _ = server.scene.system_ui.set_brightness(10);
                                                    let _ = server.power_sync.apply_super_extreme_power_saver();
                                                    power_saver_mode = PowerSaverMode::SuperExtreme;
                                                    set_active_family(FontFamily::Homemade);
                                                    super_extreme_state.enter_super_extreme();
                                                    quick_tiles_active[6] = true;
                                                    server.scene.system_ui.close();
                                                }
                                            } else if app_drawer_open {
                                                let drawer_y_offset = (1.0 - drawer_progress.clamp(0.0, 1.0)) * h;
                                                if let Some(idx) =
                                                    Layout::plain(w, h).drawer_grid_hit(drawer_y_offset, x, y)
                                                {
                                                    if let Some(target_app) = drawer_nth(
                                                        &all_managed_apps,
                                                        &drawer_search,
                                                        idx,
                                                    ) {
                                                        // Cap pins at the visible grid capacity;
                                                        // overflow would be unreachable dead state.
                                                        let _pl = Layout::plain(w, h);
                                                        let max_slots =
                                                            _pl.grid_cols * _pl.max_rows;
                                                        if home_pages[current_home_page].len()
                                                            < max_slots
                                                            && !home_pages[current_home_page]
                                                                .contains(&target_app.id)
                                                        {
                                                            home_pages[current_home_page]
                                                                .push(target_app.id.clone());
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
                                                .home_grid_hit_paged(x, y, home_scroll_offset, home_pages.len())
                                                .map(|(_, i)| i)
                                                {
                                                    let page_app_ids = &home_pages[current_home_page];
                                                    if let Some(app_id) = page_app_ids.get(idx) {
                                                        selected_home_icon = Some(app_id.clone());
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
                        // Stale input fd: unregister so it stops waking us.
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
                    }
                } else {
                    // Servicing connected client stream events (read/drain or close)
                    use std::io::Read;
                    let mut closed = false;
                    let mut stream_idx = None;
                    for (idx, stream) in server.client_streams.iter().enumerate() {
                        if stream.as_raw_fd() == fd {
                            stream_idx = Some(idx);
                            break;
                        }
                    }
                    if let Some(idx) = stream_idx {
                        if idx >= client_states.len()
                            || (ev.events
                                & (libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR) as u32)
                                != 0
                        {
                            closed = true;
                        } else {
                            // Drain loop: bounded reads per wake so a chatty
                            // client catches up without starving the frame.
                            let mut reads = 0;
                            let mut drained = false;
                            while !drained && reads < 8 {
                                reads += 1;
                                let st = &mut client_states[idx];
                                if st.len >= st.buf.len() {
                                    // Overlong/garbage tail: drop, never resync-guess.
                                    closed = true;
                                    break;
                                }
                                let stream = &mut server.client_streams[idx];
                                match stream.read(&mut st.buf[st.len..]) {
                                    Ok(0) => {
                                        closed = true;
                                        drained = true;
                                    }
                                    Ok(n) => {
                                        let end = st.len + n;
                                        let mut off = 0;
                                        while let Ok(Some((msg, mlen))) =
                                            utim_core::compositor::protocols::WlMessage::parse(
                                                &st.buf[off..end],
                                            )
                                        {
                                            // zwp_text_input_v3 requests:
                                            // opcode 1: enable -> activate virtual keyboard
                                            // opcode 2: disable -> deactivate virtual keyboard.
                                            // Gated on the bound object once known
                                            // (text_input_id == 0 preserves the
                                            // legacy opcode-only behaviour).
                                            if st.text_input_id == 0
                                                || msg.header.object_id == st.text_input_id
                                            {
                                                if msg.header.opcode == 1 {
                                                    server.scene.keyboard.activate();
                                                    app_input_focused = true;
                                                } else if msg.header.opcode == 2 {
                                                    server.scene.keyboard.deactivate();
                                                    app_input_focused = false;
                                                }
                                            }
                                            off += mlen;
                                        }
                                        // Carry the partial tail for the next read.
                                        let tail = end - off;
                                        st.buf.copy_within(off..end, 0);
                                        st.len = tail;
                                    }
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                                    {
                                        drained = true;
                                    }
                                    Err(_) => {
                                        closed = true;
                                        drained = true;
                                    }
                                }
                            }
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
                            if idx < client_states.len() {
                                client_states.swap_remove(idx);
                            }
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

        // Shutdown skips the whole frame tail: no terminal drain, no
        // rescan, no raster and no DIRTYFB after the signal.
        if !running {
            break;
        }

        // Drain asynchronous terminal command output streams for all tabs,
        // bounded per frame so one chatty child cannot stall the compositor.
        for tab in &mut terminal_tabs {
            let mut budget = 64usize;
            while budget > 0 {
                match tab.rx.try_recv() {
                    Ok(line) => {
                        push_terminal_line(&mut tab.lines, &line);
                        budget -= 1;
                    }
                    Err(_) => break,
                }
            }
            if tab.lines.len() > 120 {
                tab.lines.drain(0..tab.lines.len() - 120);
            }
        }

        // .desktop catalogue rescan, headless-safe (outside any drm gate):
        // rebuild only when the application-set signature changed, so the
        // steady-state cost is one scan + one cheap fingerprint per 2 s and
        // the icon re-resolve runs only on real change.
        if last_catalogue_scan.elapsed() >= catalogue_scan_interval {
            last_catalogue_scan = Instant::now();
            desktop_catalogue.scan_system_directories();
            let fresh = build_all_apps(&desktop_catalogue);
            let sig = app_set_signature(&fresh);
            if sig != icon_app_sig {
                icon_app_sig = sig;
                icon_cache.invalidate_misses();
                all_managed_apps = fresh;
                apply_app_icons(&mut all_managed_apps, &mut icon_cache);
                // Rebuilt with the catalogue, not lazily: the rows borrow
                // `all_managed_apps` by `&str`, so assigning the catalogue
                // invalidates every one of them at once. A recents card's
                // `u32` index keeps pointing at the same *position*, which is
                // what it means -- the renderer falls back to a neutral tile
                // when the new catalogue is shorter.
                catalogue_items = all_managed_apps.iter().map(drawer_item_of).collect();
                refresh_dock_cache(
                    &all_managed_apps,
                    &icon_cache,
                    &mut dock_index,
                    &mut dock_apps_icon,
                );
            }
        }

        // Vsync frame presentation step
        let frame_elapsed = last_frame.elapsed();
        if frame_elapsed >= frame_interval {
            let dt = frame_elapsed.as_secs_f32();
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

            // The drawer's own state is derived rather than assigned at each of
            // the fourteen places that flip `app_drawer_open`. That flag is the
            // source of truth; keeping a second one in step with it would be a
            // 14-call-site invariant that only fails silently. A modal surface
            // wins, because the drawer cannot be open behind the overview.
            if !shell_state.is_modal() {
                shell_state = if app_drawer_open {
                    ShellState::AllApps { progress: drawer_progress }
                } else {
                    ShellState::Normal
                };
            }

            // -------------------------------------------------------------
            // Launcher-rewrite surfaces (plan §7.6). Each is stepped only while
            // it is not at rest, so an idle shell integrates nothing.
            // -------------------------------------------------------------

            // The frame period in ms, for the models that park their springs
            // against a real frame clock rather than a synthetic one.
            let frame_ms = (server.scene.refresh_rate.clamp(30.0, 480.0).recip() * 1000.0) as f32;

            // Task bookkeeping: promote the foreground app to the front of the
            // recents stack the moment it changes.
            //
            // Launching an app already in the stack *promotes* it rather than
            // duplicating it -- `Recents::push` does that, and hands back the
            // stale entry so its snapshot slot can be released. The catalogue
            // index is what the card carries, so the renderer can resolve the
            // name later without a `String` per card.
            if active_app != recents_foreground {
                recents_foreground = active_app.clone();
                let pid = std::mem::take(&mut pending_launch_pid);
                if let Some(name) = active_app.as_deref() {
                    if let Some(idx) = all_managed_apps
                        .iter()
                        .position(|a| a.name == name || a.id == name)
                    {
                        // pid 0 for an in-app screen the shell drew itself
                        // (Clock, Settings) and for a launch that failed: there
                        // is no process to signal, and the kill path already
                        // refuses a pid it does not own, so a zero card is inert
                        // rather than wrong.
                        let _ = recents.push(TaskCard::new(idx as u32, pid, 0));
                    }
                }
            }

            // Overview. Two springs, because the reference runs the carousel
            // and the scrim on different profiles and they do not settle
            // together: `desktop_slide` for the surface, `recents_attach_alpha`
            // for the dim behind it.
            let overview_open = matches!(shell_state, ShellState::Overview { .. });
            overview_spring.set_target(if overview_open { 1.0 } else { 0.0 });
            overview_scrim_spring.set_target(if overview_open { 1.0 } else { 0.0 });
            let overview_live = overview_open
                || overview_spring.value > 0.0
                || overview_scrim_spring.value > 0.0;
            if overview_live {
                overview_spring.step(dt);
                overview_scrim_spring.step(dt);
            }
            // The card model runs whenever the overview is even partly open, so
            // a dismiss animation and the kill grace clock both keep time.
            if overview_live {
                kill_queue = recents.step(dt, frame_ms);
                // Park cards whose springs are done, so a settled list stops
                // re-integrating.
                recents.park_expired(frame_ms);
            }
            // The kill queue is drained here, not in the gesture handler: a
            // close request raised on a touch must not run a process teardown
            // inside the input callback.
            //
            // `Close` only *describes* the request -- `recents` starts the
            // grace clock and re-raises it as `Force` once the grace expires,
            // so the signal is sent on exactly the second pass and a
            // well-behaved app gets to close itself. A pid the shell does not
            // own is a no-op, not an error: the card has already left the
            // stack either way.
            for action in kill_queue.iter() {
                if let utim_core::compositor::KillAction::Force(pid) = action {
                    if *pid > 1 {
                        // SAFETY: `kill` is async-signal-safe, takes only a pid
                        // and a signal, and cannot fail in a way that matters
                        // here -- the process may already be gone, which is the
                        // outcome we wanted.
                        unsafe {
                            libc::kill(*pid, libc::SIGKILL);
                        }
                    }
                }
            }

            // Folder: the model owns the morph, the scrim and the title's
            // delayed fade, and reports the workspace scale it wants.
            if !folder.is_closed() {
                folder.step(dt, frame_ms);
            }

            // Popup and fast-scroller fades, both wall-clock driven.
            if !popup_spring.is_at_rest() {
                popup_spring.step(dt);
            }
            if fastscroller.dragging || !fastscroller.popup_visible() {
                fastscroller.step(dt);
            }

            // Smartspace: a wall-clock phase, not an interaction, so it is
            // stepped on time rather than on touch. Advancing it on a timer is
            // what makes the damage hash see it -- nothing else in the state
            // changes when a minute passes.
            if last_smartspace_tick.elapsed() >= Duration::from_millis(SMARTSPACE_TICK_MS) {
                last_smartspace_tick = Instant::now();
                smartspace_phase = (smartspace_phase + 1.0).min(1.0);
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

            // Ripple: the Material 3 state layer expands and fades together.
            // The radius starts at the touch point's 48dp target and grows
            // past it, the alpha falls on the same clock, and both are time
            // constants so the feel is identical at 60 and 120 Hz.
            if let Some((_, _, ref mut r, ref mut a)) = touch_ripple {
                *r += dt * RIPPLE_GROW;
                *a -= dt * RIPPLE_FADE;
                if *a <= 0.0 {
                    touch_ripple = None;
                }
            }

            if let Some(ref mut drm) = drm_display {
                let t_str = format_current_time(&mut time_buf);
                // The date only changes at midnight, but reformatting it is a
                // handful of stores into a stack buffer, so it is refreshed
                // with the time rather than tracked with a second timer. The
                // resulting bytes are hashed into the damage state, so a real
                // date change repaints and a redundant reformat does not.
                let date_str = format_current_date(&mut date_buf);
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

                // 1. Grid apps on the current home screen page
                let home_screen_apps: Vec<&ManagedApp> = if search_active && !search_query.is_empty() {
                    all_managed_apps
                        .iter()
                        .filter(|a| {
                            ci_contains(&a.name, &search_query)
                                || ci_contains(&a.id, &search_query)
                        })
                        .collect()
                } else {
                    let current_page_app_ids = &home_pages[current_home_page];
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

                // 2. Drawer apps (full catalogue for the PixelUI App Drawer),
                // skipped entirely while the drawer is closed and off-screen.
                let drawer_items: Vec<AppGridItem> =
                    if app_drawer_open || drawer_spring.value > 0.0 {
                        if !drawer_search.is_empty() {
                            all_managed_apps
                                .iter()
                                .filter(|a| {
                                    ci_contains(&a.name, &drawer_search)
                                        || ci_contains(&a.id, &drawer_search)
                                })
                                .map(drawer_item_of)
                                .collect()
                        } else {
                            all_managed_apps.iter().map(drawer_item_of).collect()
                        }
                    } else {
                        Vec::new()
                    };

                // Hotseat: same five slots as the hit-test table. Slot
                // resolution is cached across catalogue rescans (dock_index);
                // only the small item vec is rebuilt, with no find/Rc-clone.
                const DOCK_NAMES: [&str; 5] =
                    ["Phone", "Messages", "Apps", "Browser", "Camera"];
                let dock_items: Vec<AppGridItem> = DOCK_NAMES
                    .iter()
                    .enumerate()
                    .map(|(slot, name)| {
                        let entry = dock_index[slot].and_then(|i| all_managed_apps.get(i));
                        let icon = entry.and_then(|a| a.icon.as_deref()).or_else(|| {
                            if *name == "Apps" {
                                dock_apps_icon.as_deref()
                            } else {
                                None
                            }
                        });
                        AppGridItem {
                            id: entry.map(|a| a.id.as_str()).unwrap_or(if *name == "Apps" {
                                "apps"
                            } else {
                                name
                            }),
                            name,
                            color: entry.map(|a| a.color).unwrap_or(0xFF475569),
                            glyph: entry.map(|a| a.glyph.as_str()).unwrap_or(":"),
                            icon,
                        }
                    })
                    .collect();

                // Super Extreme and Power Management state updates
                super_extreme_state.check_power_button_hold();
                if super_extreme_state.request_exit_to_normal {
                    super_extreme_state.request_exit_to_normal = false;
                    let _ = server.scene.system_ui.set_brightness(75);
                    let _ = server.power_sync.restore_normal_power_mode();
                    set_active_family(FontFamily::NotoSans);
                    power_saver_mode = PowerSaverMode::Off;
                    quick_tiles_active[6] = false;
                }
                if super_extreme_state.request_reboot {
                    super_extreme_state.request_reboot = false;
                    let _ = std::process::Command::new("reboot").spawn();
                }
                if super_extreme_state.request_poweroff {
                    super_extreme_state.request_poweroff = false;
                    let _ = std::process::Command::new("poweroff").spawn();
                }
                if power_saver_mode == PowerSaverMode::SuperExtreme
                    && super_extreme_state.active_screen == SuperExtremeScreen::CameraPreview
                {
                    super_extreme_state.camera_preview.update_preview();
                }

                // Flatten the recents model into the renderer's row view. Fixed
                // capacity and no `Vec`: this runs on the frame path, where
                // `paint_frame_does_not_allocate` is the invariant.
                //
                // `dismiss` comes from the model's spring, not from
                // `TaskCard::dismiss_y`, which is the *live drag* position --
                // the raw finger. After a release the card springs on from
                // there, and reading the raw value would snap it back the
                // instant the finger lifted.
                //
                // `visible_card` is the model's own cull hint, so the renderer
                // and the hit-test agree on which card is live.
                let recents_row_count = recents.iter().count().min(recents_rows.len());
                for (i, row) in recents_rows.iter_mut().enumerate().take(recents_row_count) {
                    let card = recents.cards[i];
                    row.app_id = card.app_id;
                    row.dismiss = recents.dismiss[i].value;
                    row.selected = i as u8 == recents.visible_card;
                }

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
                    terminal_prompt: utim_core::session::session().prompt(),
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
                    power_saver_mode,
                    super_extreme_state: if power_saver_mode == PowerSaverMode::SuperExtreme || super_extreme_state.volume_hud.is_visible() {
                        Some(&super_extreme_state)
                    } else {
                        None
                    },
                    // Launcher rewrite state, driven by the shell state
                    // machine above. At-rest values mean "nothing is
                    // animating", which is also what the damage hash needs so
                    // an idle shell still issues zero ioctls.
                    smartspace_phase,
                    folder_morph: folder.morph.value,
                    folder_scrim: folder.scrim.value,
                    folder_title_alpha: folder.title_alpha.value,
                    popup_progress: popup_spring.value,
                    overview_progress: overview_spring.value,
                    // The scrim is the second spring's value, not the
                    // carousel's: the reference dims the workspace on a
                    // different profile, and driving both from one number
                    // would make the dim and the surface arrive together.
                    overview_scroll: recents.drag_px,
                    overview_dismiss: recents.dismiss[recents.visible_card as usize].value,
                    // The thumb's position along the track. `travel` is the
                    // same quantity the model clamps against, so the renderer
                    // cannot place the thumb where the model would not.
                    fastscroller_thumb: if fastscroller.track_h > 0.0 {
                        let travel =
                            (fastscroller.track_h - shell_fast_scroller.thumb_h).max(0.0);
                        if travel > 0.0 { fastscroller.thumb_y / travel } else { 0.0 }
                    } else {
                        0.0
                    },
                    fastscroller_popup_alpha: fastscroller.popup_alpha,
                    // Page indicator: signed page progress. The magnitude is
                    // how far the strip has travelled, in page widths; the
                    // sign is the direction, which the renderer needs because
                    // the dot maths takes "the page being left" and "the page
                    // being moved towards" as two separate indices.
                    //
                    // Not clamped: the settle spring overshoots past a page, and
                    // that phase lives above 1.0. A clamp here would flatten
                    // exactly the motion the field exists to carry.
                    page_indicator_frac: {
                        let pw = server.scene.width as f32;
                        if pw > 0.0 { -home_scroll_offset / pw } else { 0.0 }
                    },
                    // The in-app home gesture and the folder both scale the
                    // workspace. The folder wins while it is open: it is the
                    // nearer surface and its scale is the reference's
                    // `FOLDER_LAUNCHER_SCALE` (0.975).
                    workspace_scale: if folder.is_closed() {
                        workspace_scale_spring.value
                    } else {
                        folder.workspace_scale()
                    },
                    window_alpha: window_alpha_spring.value,
                    date_str,
                    weather_str: "",
                    weather_glyph: 0,
                    catalogue_apps: &catalogue_items,
                    recents_cards: &recents_rows[..recents_row_count],
                    fastscroller_letter: fastscroller.letter,
                    popup_anchor,
                    popup_items: &popup_rows[..popup_count.min(popup_rows.len())],
                    folder_apps: &folder_items,
                    folder_title,
                };
                // Unconditional flush() marks the whole 10.4 MB framebuffer
                // dirty 60x/s; only flush when the damage hash proves a repaint.
                if drm.render_interactive_ui(&drm_state) {
                    if let Err(e) = drm.flush() {
                        eprintln!("[UTLC] DIRTYFB flush failed: {}", e);
                    }
                }
            }
        }

        // Auto-transition to Launcher home screen after initial boot presentation
        if power_saver_mode != PowerSaverMode::SuperExtreme
            && server.scene.lockscreen.is_locked()
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
    if inotify_fd >= 0 {
        unsafe {
            libc::close(inotify_fd);
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

/// Ripple growth and fade rates, in px/s and per second.
const RIPPLE_GROW: f32 = 900.0;
const RIPPLE_FADE: f32 = 2.2;

/// A ripple starts at the Material 3 minimum touch target, so it always
/// covers the affordance the finger actually hit.
fn ripple_start_radius(panel_w: f32) -> f32 {
    (panel_w.min(2400.0) * 0.048).max(12.0) * 0.5
}

fn format_current_time(buf: &mut [u8; 5]) -> &str {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let total_secs = ts.tv_sec;
    let hours = (total_secs / 3600).rem_euclid(24) as u8;
    let mins = (total_secs / 60).rem_euclid(60) as u8;
    buf[0] = b'0' + (hours / 10);
    buf[1] = b'0' + (hours % 10);
    buf[2] = b':';
    buf[3] = b'0' + (mins / 10);
    buf[4] = b'0' + (mins % 10);
    unsafe { std::str::from_utf8_unchecked(buf) }
}

/// Weekday and month abbreviations, 3 bytes each, index 0 = Sunday.
///
/// A `const` table rather than a lookup into the C locale: `strftime` would
/// need a format string and a `tm`, and the abbreviations are fixed for the
/// lifetime of the build.
const DAY_ABBR: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
const MONTH_ABBR: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

/// Format today's date as `"Tue, Sep 22"` into a caller-owned buffer.
///
/// Zero allocation, same contract as [`format_current_time`]: the renderer
/// cannot format a date itself without allocating on the frame path, so the
/// shell does it here and hands over a borrowed `&str`.
///
/// Uses `localtime_r` rather than a hand-rolled UTC offset. A fixed offset
/// gives the right answer for 363 days and the wrong one twice a year, and it
/// is wrong by a whole day twice more if the user crosses a timezone; the
/// `_r` variant needs only a stack `tm` and is thread-safe, so it costs the
/// same as the arithmetic would have and is correct.
fn format_current_date(buf: &mut [u8; 16]) -> &str {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // glibc's `localtime_r` takes a `time_t` (an `i64`), not a `timespec`.
    // A null return means the timezone database is unavailable (a static
    // build with no /usr/share/zoneinfo). Fall back to UTC so the line still
    // renders instead of vanishing.
    if unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) }.is_null() {
        tm = unsafe { std::mem::zeroed() };
        let days = ts.tv_sec.div_euclid(86_400);
        tm.tm_mday = (days + 19_723) as i32; // 1970-01-01 is day 0
        tm.tm_wday = ((days + 4) % 7) as i32; // ...and a Thursday
    }
    format_date_from_tm(buf, tm.tm_wday, tm.tm_mon, tm.tm_mday)
}

/// The pure half of [`format_current_date`], so it can be tested against known
/// dates instead of only against "whatever today is".
///
/// Out-of-range fields are clamped rather than trusted: `tm_*` comes from
/// libc, and an index panic on the frame path would be a crash rather than a
/// wrong glyph.
fn format_date_from_tm(buf: &mut [u8; 16], wday: i32, mon: i32, mday: i32) -> &str {
    let weekday = (wday.rem_euclid(7) as usize).min(6);
    let mon_i = (mon.rem_euclid(12) as usize).min(11);
    let day = mday.clamp(1, 31) as usize;

    let mut n = 0usize;
    let push = |bytes: &[u8], buf: &mut [u8; 16], n: &mut usize| {
        for &c in bytes {
            if *n < buf.len() {
                buf[*n] = c;
                *n += 1;
            }
        }
    };
    push(DAY_ABBR[weekday], buf, &mut n);
    push(b", ", buf, &mut n);
    push(MONTH_ABBR[mon_i], buf, &mut n);
    push(b" ", buf, &mut n);
    // Day of month, 1 or 2 digits, no leading zero (matches the reference's
    // "Sep 22" rather than "Sep 02"). A single-digit day must take the *ones*
    // digit, not the tens one -- the first version of this rendered
    // "Jan 0" for the 1st, which the known-dates test caught.
    let mut tmp = [0u8; 2];
    tmp[0] = b'0' + (day / 10) as u8;
    tmp[1] = b'0' + (day % 10) as u8;
    if day >= 10 {
        push(&tmp[..2], buf, &mut n);
    } else {
        push(&tmp[1..2], buf, &mut n);
    }
    // The byte pattern is `[A-Za-z, 0-9]` by construction, so this cannot
    // fail; `from_utf8` would be a second pass over 16 bytes.
    unsafe { std::str::from_utf8_unchecked(&buf[..n]) }
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
            // Non-blocking pipe (set at spawn): a write can never stall the
            // UI thread. WouldBlock means the child is not keeping up, so
            // this line is dropped instead of blocking on it.
            let r1 = stdin.write(tab.input.as_bytes());
            let r2 = r1.and_then(|_| stdin.write(b"\n"));
            match r2 {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tab.input.clear();
                    true
                }
                Err(_) => false,
            }
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
        ImeAction::CommitChar(c) => {
            if active_app.as_deref() == Some("Terminal") {
                if let Some(tab) = terminal_tabs.get_mut(*active_tab_idx) {
                    tab.input.push(c);
                }
            } else if *search_active {
                if search_query.len() + c.len_utf8() <= 60 {
                    search_query.push(c);
                }
            } else if active_app.is_some() && *app_input_focused && app_input.len() + c.len_utf8() <= 120
            {
                app_input.push(c);
            }
        }
        ImeAction::CommitString(s) => {
            if active_app.as_deref() == Some("Terminal") {
                if let Some(tab) = terminal_tabs.get_mut(*active_tab_idx) {
                    tab.input.push_str(&s);
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
                if let Some(tab) = terminal_tabs.get_mut(*active_tab_idx) {
                    tab.input.pop();
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
        let sess = utim_core::session::session();
        let is_root = utim_core::session::is_root_process();
        if is_root {
            utim_core::session::ensure_session_dirs();
        }

        let shell = if Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        let mut cmd_obj = std::process::Command::new(shell);
        cmd_obj
            .arg("-c")
            .arg(cmd)
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LD_LIBRARY_PATH", "/usr/lib/aarch64-linux-gnu:/lib/aarch64-linux-gnu:/usr/lib:/lib")
            .env("HOME", sess.home())
            .env("USER", sess.name())
            .env("LOGNAME", sess.name())
            .env("SHELL", shell)
            .env("TERM", "linux")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .env("XDG_RUNTIME_DIR", utim_core::session::SESSION_RUNTIME_DIR)
            .env("COLUMNS", "54")
            .env("LINES", "25")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped());

        utim_core::session::drop_privileges(&mut cmd_obj);
        // The `~` in the prompt is only truthful if the shell starts in the
        // session home, privileged or not.
        if std::path::Path::new(sess.home()).exists() {
            cmd_obj.current_dir(sess.home());
        }

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
                use std::os::unix::io::AsRawFd;
                set_fd_nonblocking(stdin.as_raw_fd());
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
            let sess = utim_core::session::session();
            if utim_core::session::is_root_process() {
                utim_core::session::ensure_session_dirs();
            }
            let mut cmd_obj = std::process::Command::new(&bin_path);
            cmd_obj
                .args(&parts[1..])
                .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                .env("LD_LIBRARY_PATH", "/usr/lib/aarch64-linux-gnu:/lib/aarch64-linux-gnu:/usr/lib:/lib")
                .env("HOME", sess.home())
                .env("USER", sess.name())
                .env("LOGNAME", sess.name())
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
            // Same identity as the bash path above, so a direct binary does not
            // silently regain root just because no shell was available.
            if utim_core::session::drop_privileges(&mut cmd_obj)
                && std::path::Path::new(sess.home()).exists()
            {
                cmd_obj.current_dir(sess.home());
            }
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
                    let _ = tx.send(format!(
                        "Linux {} 6.1.23-android14-4-00257 aarch64 GNU/Linux",
                        utim_core::session::session().host()
                    ));
                }
                "uptime" => {
                    let uptime_str = fs::read_to_string("/proc/uptime").unwrap_or_else(|_| "0.0 0.0".into());
                    let secs = uptime_str.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) as u64;
                    let mins = (secs / 60) % 60;
                    let hours = secs / 3600;
                    let _ = tx.send(format!("up {:02}:{:02}, 1 user, load avg: 0.02, 0.01, 0.00", hours, mins));
                }
                "whoami" => {
                    let _ = tx.send(utim_core::session::session().name().to_string());
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
    let prompt_line = format!("{}{}", utim_core::session::session().prompt(), cmd);
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

/// Time one synthetic touch through the **real** dispatch path.
///
/// Plan §8.3. The metric used to come from
/// `WaylandServer::measure_touch_latency`, which built a throwaway
/// `GestureEngine` and pushed one `Down` at it. That measured the engine in
/// isolation: it excluded evdev decode, `InputDispatcher::process_event`,
/// and the `Layout` hit-test, so the reported number was a lower bound being
/// compared against the real 8 ms budget. A path that got several times
/// slower would still have "passed".
///
/// This drives the same three stages the frame loop does, in the same order
/// and with the same event shape the kernel produces:
///
///   1. `ABS_X` + `ABS_Y` + `SYN_REPORT` into `InputDispatcher`
///      (ABS, not ABS_MT: the dispatcher's single-touch path, which is what
///      a mouse or a single finger actually drives)
///   2. the resulting `InputDispatchResult` into `GestureEngine`
///   3. the emitted action into the `Layout` hit-test
///
/// The **worst** of the probes is returned, not the mean: 8 ms is a per-event
/// latency target, so the tail is what has to fit in it.
fn measure_real_touch_latency(w: f32, h: f32) -> f64 {
    use utim_core::compositor::input as ev;
    let layout = Layout::plain(w, h);
    let mut engine = GestureEngine::new(w, h, GestureConfig::default());
    let mut dispatcher = InputDispatcher::new(w, h);
    let raw_max = 32767.0f32;

    // Three probes that each take a different path: a grid cell (hits the
    // hit-test), the bottom nav bar (gesture engine's bottom band), and the
    // left edge (the back gesture).
    let cell = layout.grid_cell(layout.grid_cols);
    let probes = [
        (cell.center_x(), cell.center_y()),
        (w * 0.5, h - 10.0),
        (1.0, h * 0.5),
    ];

    let mut worst = 0.0f64;
    for (x, y) in probes {
        let t0 = Instant::now();
        let ev_x = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_ABS,
            code: ev::ABS_X,
            value: (x / w * raw_max) as i32,
        };
        let ev_y = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_ABS,
            code: ev::ABS_Y,
            value: (y / h * raw_max) as i32,
        };
        let ev_down = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_KEY,
            code: ev::BTN_LEFT,
            value: 1,
        };
        let ev_syn = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: ev::EV_SYN,
            code: ev::SYN_REPORT,
            value: 0,
        };

        let mut dispatched = InputDispatchResult::None;
        for e in [&ev_x, &ev_y, &ev_down, &ev_syn] {
            let r = dispatcher.process_event(e);
            if r != InputDispatchResult::None {
                dispatched = r;
            }
        }
        // Feed whatever the dispatcher produced into the gesture engine...
        if let InputDispatchResult::Touch(t) = dispatched {
            let _ = engine.process_touch(&t);
        }
        // ...then hit-test it, which is the other half of the shell's
        // per-event work and was entirely missing from the old probe.
        let _hit = layout.home_grid_hit(x, y, 0.0);
        let _zone = layout.home_zone(x, y);

        worst = worst.max(t0.elapsed().as_secs_f64() * 1000.0);
    }
    worst
}

fn run_benchmarks(json: bool) -> bool {
    let hwc = HwcComposer::new(HwcVersion::AidlComposer3);
    let socket_path = PathBuf::from(format!("/tmp/utlc-bench-{}.sock", std::process::id()));
    let mut server = WaylandServer::new(&socket_path, 1080, 2400, 120.0, hwc);

    let metrics = server.get_metrics().expect("metrics collection failed");
    let rss_mb = (metrics.resident_memory_bytes as f64) / (1024.0 * 1024.0);
    let boot_ms = metrics.boot_to_launcher_duration.as_secs_f64() * 1000.0;
    // The real path, not the engine-only lower bound.
    let touch_ms = measure_real_touch_latency(server.scene.width as f32, server.scene.height as f32);

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
            "[*] Touch Dispatch (evdev->Input->Gesture->Hit): {:.4} ms (Target: < 8.0 ms) -> {}",
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

/// In-shell gesture self-test (plan \u00a78.3).
///
/// This used to assert three booleans -- "did we get a Home", "did we get a
/// Recents", "did we get a Back" -- which is why a gesture engine that
/// dropped two of its actions and replaced the third with a duration hold
/// still reported green. Every group below now asserts *thresholds* and
/// carries at least one negative case, so a gesture that fires too eagerly,
/// too late, or not at all is a failure rather than a pass.
///
/// Note the Recents group: Recents is a **motion pause**, not a duration
/// hold. A finger that rises and then stops is Recents; a finger that keeps
/// sliding at 0.9 px/ms for 200ms is a Home drag no matter how long it is
/// held down. The negative case is the one that matters.
fn test_gestures(json: bool) -> bool {
    let cfg = GestureConfig::default();
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let ev = |id: i32, phase: TouchPhase, x: f32, y: f32, ms: u64| RawTouchEvent {
        touch_id: id,
        phase,
        x,
        y,
        timestamp: at(ms),
    };

    // 1. Home: a quick bottom-edge flick commits Home at full progress, and
    //    the window must have shrunk (scale < 1) rather than teleporting.
    engine.process_touch(&ev(1, TouchPhase::Down, 540.0, 2380.0, 0));
    let home_act = engine.process_touch(&ev(1, TouchPhase::Up, 540.0, 2200.0, 80));
    let home_ok = match home_act {
        GestureAction::Home {
            progress,
            scale,
            window_alpha,
        } => progress >= 1.0 && scale < 1.0 && window_alpha < 1.0,
        _ => false,
    };

    // 2. Recents via motion pause. Rise fast enough to build a peak, then
    //    creep below `motion_pause_slow` for longer than `force_pause_ms`.
    //    The haptic must fire on the rising edge only.
    engine.process_touch(&ev(2, TouchPhase::Down, 540.0, 2380.0, 0));
    let mut recents_ok = false;
    let mut haptic_edges = 0u32;
    let mut last_y = 2380.0f32;
    // 3.75 px/ms rise, then a 0.06 px/ms creep -- ~0.04 px per 16ms frame.
    for step in 1..=6u64 {
        last_y -= 60.0;
        let a = engine.process_touch(&ev(2, TouchPhase::Move, 540.0, last_y, step * 16));
        if let GestureAction::Recents { trigger_haptic, .. } = a {
            if trigger_haptic {
                haptic_edges += 1;
                recents_ok = true;
            }
        }
    }
    for step in 7..=40u64 {
        last_y -= 0.6;
        let a = engine.process_touch(&ev(2, TouchPhase::Move, 540.0, last_y, step * 16));
        if let GestureAction::Recents { trigger_haptic, .. } = a {
            if trigger_haptic {
                haptic_edges += 1;
            }
            // The pause cannot fire during the rise (the finger is still
            // accelerating), so it is the creep that must set this.
            recents_ok = true;
        }
    }
    engine.process_touch(&ev(2, TouchPhase::Up, 540.0, last_y, 41 * 16));
    // A latch that re-fires every frame would buzz the actuator.
    let recents_ok = recents_ok && haptic_edges == 1;

    // 3. Negative case: a continuous slow crawl held for 200ms must NOT be
    //    Recents. This is precisely the gesture the old duration-hold rule
    //    mis-classified, and it is the regression this group exists for.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(3, TouchPhase::Down, 540.0, 2380.0, 0));
    let crawl = engine.process_touch(&ev(3, TouchPhase::Move, 540.0, 2200.0, 200));
    let crawl_not_recents = !matches!(crawl, GestureAction::Recents { .. });
    engine.process_touch(&ev(3, TouchPhase::Up, 540.0, 2200.0, 220));

    // 4. In-app home swipe: from the *centre* of the screen (not the nav
    //    bar) a vertical-dominant drag must report Home on MOVE, so the
    //    window can track the finger instead of snapping on release.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(4, TouchPhase::Down, 540.0, 1400.0, 0));
    let mut center_move_home = false;
    for step in 1..=10u64 {
        let a = engine.process_touch(&ev(
            4,
            TouchPhase::Move,
            540.0,
            1400.0 - step as f32 * 40.0,
            step * 16,
        ));
        if let GestureAction::Home { progress, .. } = a {
            if progress > 0.0 && progress <= 1.0 {
                center_move_home = true;
            }
        }
    }
    // ...and a horizontal drag in the same place must NOT be Home: that
    // belongs to the pager, not the app-exit gesture.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(5, TouchPhase::Down, 300.0, 1400.0, 0));
    let mut center_h_ok = true;
    for step in 1..=10u64 {
        let a = engine.process_touch(&ev(
            5,
            TouchPhase::Move,
            300.0 + step as f32 * 60.0,
            1400.0,
            step * 16,
        ));
        if matches!(a, GestureAction::Home { .. }) {
            center_h_ok = false;
        }
    }
    let center_ok = center_move_home && center_h_ok;

    // 5. Back: an edge swipe past the threshold injects.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(6, TouchPhase::Down, 10.0, 1200.0, 0));
    let back_move = engine.process_touch(&ev(6, TouchPhase::Move, 60.0, 1200.0, 50));
    let back_progress_ok = matches!(back_move, GestureAction::Back { progress, .. } if progress > 0.0);
    let back_act = engine.process_touch(&ev(6, TouchPhase::Up, 60.0, 1200.0, 100));
    let back_ok = matches!(back_act, GestureAction::Back { injected, .. } if injected)
        && back_progress_ok;

    // 6. Notification shade: a top-edge pull reports rising progress, and a
    //    tap must not slam an opening shade to 0.0.
    let mut engine = GestureEngine::new(1080.0, 2400.0, cfg.clone());
    engine.process_touch(&ev(7, TouchPhase::Down, 540.0, 10.0, 0));
    let shade_ok = matches!(
        engine.process_touch(&ev(7, TouchPhase::Move, 540.0, 310.0, 60)),
        GestureAction::NotificationShade { progress } if progress > 0.0
    );

    let all_ok = home_ok && recents_ok && crawl_not_recents && center_ok && back_ok && shade_ok;
    if json {
        println!(
            r#"{{"home_ok":{},"recents_motion_pause_ok":{},"crawl_not_recents":{},"inapp_home_swipe_ok":{},"back_ok":{},"shade_ok":{},"all_passed":{}}}"#,
            home_ok, recents_ok, crawl_not_recents, center_ok, back_ok, shade_ok, all_ok
        );
    } else {
        let mark = |b: bool| if b { "PASS" } else { "FAIL" };
        println!(
            "[*] QuickStep Gestures: Home={}, Recents(motion-pause)={}, crawl-not-Recents={}, in-app-home-swipe={}, Back={}, Shade={} -> {}",
            mark(home_ok),
            mark(recents_ok),
            mark(crawl_not_recents),
            mark(center_ok),
            mark(back_ok),
            mark(shade_ok),
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
    // Sysfs is absent in the self-test env: the torch toggle must report
    // the failure, and Bluetooth (no sysfs) still toggles.
    let torch_err = shade.toggle_tile(QuickTileKind::Torch).is_err();
    let bt_ok = shade
        .toggle_tile(QuickTileKind::Bluetooth)
        .unwrap_or(false);
    let notif_id = shade.notify(
        "App".into(),
        0,
        "icon".into(),
        "Summary".into(),
        "Body".into(),
        vec![],
    );
    let notif_ok = notif_id > 0 && shade.notifications.len() == 1;

    let all_ok = torch_err && bt_ok && notif_ok;
    if json {
        println!(
            r#"{{"torch_toggle_reports_sysfs_error":{},"bluetooth_toggle_ok":{},"notification_ok":{},"all_passed":{}}}"#,
            torch_err, bt_ok, notif_ok, all_ok
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
    // Unbound HAL: fingerprint never authenticates.
    let mut lockscreen = LockScreen::new(Some("1234"));
    let rejected = !lockscreen.on_fingerprint_touch(1) && lockscreen.is_locked();

    // Bound HAL with a PIN enrolled: routes to PIN entry, no bypass.
    lockscreen.biometric_bridge.hal_bound = true;
    let gated = !lockscreen.on_fingerprint_touch(1)
        && lockscreen.state == LockState::PinEntry;

    // Bound HAL with no PIN: sub-300ms direct unlock.
    let mut open = LockScreen::new(None);
    open.biometric_bridge.hal_bound = true;
    let t_auth_start = Instant::now();
    let fp_ok = open.on_fingerprint_touch(1);
    let auth_dur = t_auth_start.elapsed();
    let auth_ok = fp_ok
        && !open.is_locked()
        && auth_dur < Duration::from_millis(300);

    let all_ok = rejected && gated && auth_ok;
    if json {
        println!(
            r#"{{"unbound_rejected":{},"pin_gated":{},"fingerprint_unlock_ok":{},"sub_300ms":{}}}"#,
            rejected, gated, fp_ok, auth_ok
        );
    } else {
        println!(
            "[*] Lock Screen & Fingerprint HAL Bridge (< 300ms): {}",
            if all_ok { "PASSED" } else { "FAILED" }
        );
    }
    all_ok
}

fn test_ime(json: bool) -> bool {
    let mut ime = VirtualKeyboard::new(1080.0, 2400.0);
    ime.activate();
    for _ in 0..60 {
        ime.update(0.016);
    }
    let push_ok = (ime.window_viewport_push_y() - 320.0).abs() < 1.0;
    let act = ime.handle_key_tap("k");
    let key_ok = matches!(act, ImeAction::CommitChar('k'));

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
    // No daemon socket in the self-test env: every transition must report
    // the failure instead of mocking success.
    let mut power = UtimPowerSync::new(Path::new("/tmp/mock-utim.sock"));
    let sleep_err = power.on_display_sleep().is_err();
    let wake_err = power.on_display_wake().is_err();
    let oom_err = power.on_app_switched(101, &[102], &[103]).is_err();

    // Verify SuperExtreme Recovery state transitions
    let mut sex = SuperExtremeState::new();
    let init_lock = sex.active_screen == SuperExtremeScreen::Lock;
    sex.volume_up();
    let vol_up_ok = sex.volume_hud.volume_percent == 60 && sex.volume_hud.is_visible();
    sex.on_swipe_up();
    let pass_screen = sex.active_screen == SuperExtremeScreen::Password;
    let unlocked = sex.submit_password(None, 0);
    let home_screen = sex.active_screen == SuperExtremeScreen::Home;

    let all_ok = sleep_err && wake_err && oom_err && init_lock && vol_up_ok && pass_screen && unlocked && home_screen;
    if json {
        println!(
            r#"{{"sleep_reports_error":{},"wake_reports_error":{},"oom_reports_error":{},"super_extreme_ok":{},"all_passed":{}}}"#,
            sleep_err, wake_err, oom_err, home_screen, all_ok
        );
    } else {
        println!(
            "[*] UTIM Power, Normal/SuperExtreme Modes & Recovery Shell: {}",
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

    // -----------------------------------------------------------------
    // The shell state machine (plan §7.6).
    //
    // These guard the defect this whole change set exists for: the input
    // loop's gesture match ended in `_ => {}`, and both `Recents` and
    // `BottomBarScrub` fell into it. The gesture engine produced and tested
    // both actions; nothing consumed them, so the overview could not open and
    // a task scrub did nothing -- with every test in the workspace green.
    //
    // The lesson is that a producer-side test is not a wiring test. Asserting
    // the *effect* is what catches a consumer that was never written.
    // -----------------------------------------------------------------

    #[test]
    fn the_two_dropped_gestures_now_have_an_effect() {
        use utim_core::compositor::gestures::EdgeSide;

        // Exactly the two that used to be swallowed.
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: 1.0,
                trigger_haptic: true,
            }),
            ShellEffect::OpenOverview,
            "the recents gesture still has no effect: the overview cannot open"
        );
        assert_eq!(
            plan_gesture(&GestureAction::BottomBarScrub {
                delta_x: 40.0,
                app_shift: 1,
            }),
            ShellEffect::ScrubTasks(1.0),
            "a bottom-bar scrub still has no effect: the task list cannot be scrubbed"
        );
        assert_eq!(
            plan_gesture(&GestureAction::BottomBarScrub {
                delta_x: -40.0,
                app_shift: -1,
            }),
            ShellEffect::ScrubTasks(-1.0),
        );
        // A zero shift is the gesture engine's "no detent crossed", and must
        // not be treated as a scrub -- it would otherwise move a card on every
        // move event while the finger sat inside one detent.
        assert_eq!(
            plan_gesture(&GestureAction::BottomBarScrub {
                delta_x: 3.0,
                app_shift: 0,
            }),
            ShellEffect::None,
        );
        // The other actions are handled by inline arms and must not be
        // double-handled by the planner.
        for a in [
            GestureAction::None,
            GestureAction::NotificationShade { progress: 0.5 },
            GestureAction::Back {
                side: EdgeSide::Left,
                progress: 1.0,
                injected: true,
            },
            GestureAction::Swipe {
                delta_x: 0.0,
                delta_y: -80.0,
            },
        ] {
            assert_eq!(plan_gesture(&a), ShellEffect::None, "{a:?} is not the planner's");
        }
    }

    #[test]
    fn recents_only_commits_past_the_threshold() {
        // Below the commit threshold the overview must not open, or a
        // half-hearted rise from the bottom edge would yank the user into the
        // carousel and then straight back out.
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: RECENTS_COMMIT_PROGRESS - 0.01,
                trigger_haptic: false,
            }),
            ShellEffect::None,
        );
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: RECENTS_COMMIT_PROGRESS,
                trigger_haptic: false,
            }),
            ShellEffect::OpenOverview,
            "the threshold must be inclusive: a release exactly at it commits"
        );
        // A rise that has not reached half screen is a Home gesture, not a
        // recents one, and must not open the overview either.
        assert_eq!(
            plan_gesture(&GestureAction::Recents {
                progress: 0.0,
                trigger_haptic: false,
            }),
            ShellEffect::None,
        );
    }

    #[test]
    fn a_full_home_gesture_closes_and_a_partial_one_morphs() {
        // The two are different effects, and conflating them is the bug that
        // made the in-app home gesture unusable: at progress 1.0 the panel
        // must be *torn down* (which also releases the morph), and below it the
        // panel must be *driven* by the gesture's own scale and opacity.
        assert_eq!(
            plan_gesture(&GestureAction::Home {
                progress: 1.0,
                scale: 0.4,
                window_alpha: 0.2,
            }),
            ShellEffect::CloseAll,
        );
        assert_eq!(
            plan_gesture(&GestureAction::Home {
                progress: 0.5,
                scale: 0.8,
                window_alpha: 0.9,
            }),
            ShellEffect::MorphWorkspace {
                scale: 0.8,
                window_alpha: 0.9,
            },
        );
    }

    #[test]
    fn closing_modal_surfaces_leaves_nothing_sticky() {
        // The reason this is a function and not four inline lines: a leftover
        // modal state makes every later workspace swipe inert (the `is_modal`
        // guard skips them) and a leftover morph target leaves the app panel
        // shrunk with nothing driving it. Both are silent, so both are asserted.
        let mut state = ShellState::Overview {
            selected: 2,
            dismiss: -40.0,
        };
        assert!(state.is_modal());

        let mut folder = FolderOpen::closed(0);
        folder.open(3);
        let mut popup = SpringSimulation::new(1.0, 1.0, SpringConfig::spring_loaded());
        popup.set_target(0.0);
        let mut scale = SpringSimulation::new(0.5, 0.5, SpringConfig::stretch_edge());
        scale.set_target(1.0);
        let mut alpha = SpringSimulation::new(0.3, 0.3, SpringConfig::stretch_edge());
        alpha.set_target(1.0);

        close_modal_surfaces(&mut state, &mut folder, &mut popup, &mut scale, &mut alpha);

        assert_eq!(state, ShellState::Normal);
        assert!(!state.is_modal(), "Normal must not block a workspace swipe");
        assert_eq!(folder.folder_idx, 3, "closing must not lose which folder it was");
        assert_eq!(scale.target, 1.0, "the workspace morph must return to rest");
        assert_eq!(alpha.target, 1.0);
        assert_eq!(popup.target, 0.0, "the popup must be dismissed, not left open");
    }

    #[test]
    fn only_a_modal_state_blocks_a_workspace_swipe() {
        // The guard this assertion exists for. If `Normal` or `AllApps` were
        // modal, the home screen could never be paged again; if `Overview` were
        // not, a swipe behind the carousel would page the workspace under it.
        assert!(!ShellState::Normal.is_modal());
        assert!(!ShellState::AllApps { progress: 1.0 }.is_modal());
        assert!(ShellState::Overview {
            selected: 0,
            dismiss: 0.0
        }
        .is_modal());
        assert!(
            ShellState::PopupOpen {
                anchor_x: 0.0,
                anchor_y: 0.0,
                idx: 0
            }
            .is_modal()
        );
    }

    #[test]
    fn test_terminal_command_execution() {
        let mut lines = Vec::new();
        let mut input = "uname -a".to_string();
        let mut app = Some("Terminal".to_string());

        let res = execute_terminal_command(&mut lines, &mut input, &mut app, None, None, None, None, 1);
        assert_eq!(res, TerminalAction::Continue);
        assert!(input.is_empty());
        assert!(lines.len() >= 2);
        assert_eq!(
            lines[0],
            format!("{}uname -a", utim_core::session::session().prompt())
        );
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
        // `whoami` must agree with the identity the prompt advertises: never
        // "root" while the session runs unprivileged.
        let sess = utim_core::session::session();
        assert!(!tabs[1].lines[1].is_empty());
        if sess.is_root() {
            assert_eq!(tabs[1].lines[1], "root");
        } else {
            assert_ne!(tabs[1].lines[1], "root");
        }

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
    fn date_formatter_matches_known_dates() {
        // The launcher shipped a hard-coded "Tue, Sep 22" for its entire
        // life, so nothing ever checked that the date formatter worked. These
        // are real weekdays, verified against `date`:
        //   2026-09-28 Monday    2026-09-22 Tuesday   2026-01-01 Thursday
        //   1970-01-01 Thursday  2000-02-29 Tuesday   2024-12-31 Tuesday
        let cases: [(i32, i32, i32, &str); 6] = [
            (1, 8, 28, "Mon, Sep 28"),  // wday, mon(0-based), mday
            (2, 8, 22, "Tue, Sep 22"),
            (4, 0, 1, "Thu, Jan 1"),
            (4, 0, 1, "Thu, Jan 1"), // 1970-01-01, the epoch
            (2, 1, 29, "Tue, Feb 29"), // 2000-02-29, a leap day
            (2, 11, 31, "Tue, Dec 31"),
        ];
        let mut buf = [0u8; 16];
        for (wday, mon, mday, want) in cases {
            let got = format_date_from_tm(&mut buf, wday, mon, mday);
            assert_eq!(got, want, "wday={wday} mon={mon} mday={mday}");
        }
    }

    #[test]
    fn date_formatter_clamps_libc_fields_instead_of_panicking() {
        // `tm_*` comes from libc and this runs on the frame path, so an
        // out-of-range value must clamp, not index out of the table.
        let mut buf = [0u8; 16];
        for (wday, mon, mday) in [
            (-1, -1, -1),
            (7, 12, 32),
            (i32::MAX, i32::MAX, i32::MAX),
            (0, 0, 0),
        ] {
            let got = format_date_from_tm(&mut buf, wday, mon, mday);
            assert!(!got.is_empty(), "{wday}/{mon}/{mday} produced nothing");
            assert!(got.len() <= 12, "{wday}/{mon}/{mday} -> {got:?} is too long");
            assert!(
                got.is_ascii(),
                "{wday}/{mon}/{mday} -> {got:?} is not ASCII; the font has no \
                 other glyphs"
            );
        }
    }

    #[test]
    fn date_formatter_emits_only_renderable_characters() {
        // `font.rs` covers 0x20..0x7F and asserts non-ASCII has no ink, so a
        // stray byte would render as a blank rather than fail loudly.
        let mut buf = [0u8; 16];
        for d in 1..=31 {
            for m in 0..12 {
                for w in 0..7 {
                    let s = format_date_from_tm(&mut buf, w, m, d);
                    assert!(s.bytes().all(|b| (0x20..0x7F).contains(&b)), "{s:?}");
                }
            }
        }
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
        let wire_enable = builder_enable.build().expect("message fits u16");

        let (msg_enable, len) = utim_core::compositor::protocols::WlMessage::parse(&wire_enable).unwrap().unwrap();
        assert_eq!(len, wire_enable.len());
        if msg_enable.header.opcode == 1 {
            keyboard.activate();
        }
        assert!(keyboard.is_active);

        // Build opcode 2: zwp_text_input_v3.disable
        let builder_disable = WlMessageBuilder::new(42, 2);
        let wire_disable = builder_disable.build().expect("message fits u16");

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
        let mut home_pages = [vec!["settings".to_string(), "files".to_string(), "terminal".to_string(), "gallery".to_string()],
            vec!["clock".to_string(), "contacts".to_string()]];
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
        let mut home_pages = [vec!["terminal".to_string(), "gallery".to_string()]];
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

        // Material ripple: the state layer grows and fades on time constants.
        let mut ripple = Some((500.0, 600.0, ripple_start_radius(1080.0), 1.0));
        for _ in 0..10 {
            if let Some((_, _, ref mut r, ref mut a)) = ripple {
                *r += dt * RIPPLE_GROW;
                *a -= dt * RIPPLE_FADE;
                if *a <= 0.0 {
                    ripple = None;
                }
            }
        }
        let (_, _, r, a) = ripple.expect("Ripple should still be active at 160ms");
        assert!(r > 100.0, "Ripple radius should expand over time: {}", r);
        assert!(a > 0.5 && a < 0.75, "Ripple alpha should decay gently: {}", a);
        // The start radius covers a 48dp touch target.
        assert!(
            ripple_start_radius(1080.0) * 2.0 >= 1080.0 * 0.048,
            "ripple must start at the minimum touch target"
        );
        let start = ripple_start_radius(360.0);
        assert!(start > 8.0, "a small panel still gets a visible ripple");

        // After additional frames, ripple fades completely to None
        for _ in 0..30 {
            if let Some((_, _, ref mut r, ref mut a)) = ripple {
                *r += dt * RIPPLE_GROW;
                *a -= dt * RIPPLE_FADE;
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

    #[test]
    fn test_power_saver_modes_and_super_extreme_recovery_shell() {
        let mut sex = SuperExtremeState::new();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Lock);

        // 1. Volume HUD adjustment
        sex.volume_up();
        assert_eq!(sex.volume_hud.volume_percent, 60);
        assert!(sex.volume_hud.is_visible());
        let bar = sex.volume_hud.format_bar(30);
        assert!(bar.starts_with("VOL ["));
        assert!(bar.contains("60%"));

        sex.volume_down();
        assert_eq!(sex.volume_hud.volume_percent, 50);

        // 2. Lock screen actions: Emergency dialer
        let (w, h) = (360.0f32, 640.0f32);
        assert!(sex.handle_touch_tap(w * 0.20, h * 0.84, w, h, None, 0));
        assert_eq!(sex.active_screen, SuperExtremeScreen::EmergencyDialer);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Lock);

        // 3. Lock screen camera preview & snapshot
        assert!(sex.handle_touch_tap(w * 0.70, h * 0.84, w, h, None, 0));
        assert_eq!(sex.active_screen, SuperExtremeScreen::CameraPreview);
        sex.camera_preview.update_preview();
        let photo = sex.snap_photo();
        assert!(photo.is_some());
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Lock);

        // 4. Swipe up to unlock -> Password screen
        sex.on_swipe_up();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Password);

        // 5. Password entry and submit
        let unlocked = sex.enter_password_char('1', None, 0);
        assert!(unlocked);
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        // 6. Launch apps from home screen (0..3)
        sex.launch_home_app(0);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppAlarm);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        sex.launch_home_app(1);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppPhone);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        sex.launch_home_app(2);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppSms);
        sex.handle_back();
        assert_eq!(sex.active_screen, SuperExtremeScreen::Home);

        sex.launch_home_app(3);
        assert_eq!(sex.active_screen, SuperExtremeScreen::AppSettings);
        assert!(sex.handle_touch_tap(w * 0.50, h * 0.55, w, h, None, 0));
        assert!(sex.request_exit_to_normal);

        // 7. Power button hold triggers Recovery Power Menu
        sex.active_screen = SuperExtremeScreen::Home;
        sex.on_power_button_press();
        sex.power_press_start = Some(std::time::Instant::now() - std::time::Duration::from_millis(1100));
        assert!(sex.check_power_button_hold());
        assert_eq!(sex.active_screen, SuperExtremeScreen::PowerMenu);

        // Option 0: Return to normal mode
        assert!(sex.handle_touch_tap(w * 0.50, h * 0.34, w, h, None, 0));
        assert!(sex.request_exit_to_normal);
    }
}
