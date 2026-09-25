//! Direct Rendering Manager (DRM) Kernel Mode Setting (KMS) Hardware Scanout.
//! Provides zero-dependency, bare-metal hardware display presentation for Universal Treble Linux
//! via Linux DRM dumb buffers and CRTC modesetting. Adheres strictly to GEMINI.md systems rules.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

// --- DRM KMS IOCTL Definitions (Standard Linux ABI) ---
const DRM_IOCTL_MODE_GETRESOURCES: libc::c_ulong = 0xc04064a0;
const DRM_IOCTL_MODE_GETCONNECTOR: libc::c_ulong = 0xc05064a7;
const DRM_IOCTL_MODE_CREATE_DUMB: libc::c_ulong = 0xc02064b2;
const DRM_IOCTL_MODE_ADDFB: libc::c_ulong = 0xc01c64ae;
const DRM_IOCTL_MODE_MAP_DUMB: libc::c_ulong = 0xc01064b3;
const DRM_IOCTL_MODE_SETCRTC: libc::c_ulong = 0xc06864a2;
const DRM_IOCTL_MODE_DIRTYFB: libc::c_ulong = 0xc01864b1;
const DRM_IOCTL_MODE_DESTROY_DUMB: libc::c_ulong = 0xc00464b4;
const DRM_IOCTL_MODE_RMFB: libc::c_ulong = 0xc00464af;

const DRM_MODE_TYPE_DRIVER: u32 = 1 << 6;
const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct DrmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub mode_type: u32,
    pub name: [u8; 32],
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeFbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: DrmModeModeInfo,
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeDestroyDumb {
    handle: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct DrmModeFbDirtyCmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

/// Direct hardware DRM KMS display scanout device
pub struct DrmKmsDevice {
    file: File,
    pub crtc_id: u32,
    pub connector_id: u32,
    fb_id: u32,
    dumb_handle: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub size: usize,
    mmap_ptr: *mut u32,
    pub mode: DrmModeModeInfo,
    frame_cache_hash: u64,
    frame_cache_valid: bool,
}

impl DrmKmsDevice {
    /// Open the DRM device node (typically /dev/dri/card0) and configure scanout
    pub fn open_card<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)?;

        let fd = file.as_raw_fd();

        // 1. Get resources
        let mut res = DrmModeCardRes::default();
        let mut crtcs = [0u32; 16];
        let mut connectors = [0u32; 16];
        let mut encoders = [0u32; 16];

        res.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
        res.connector_id_ptr = connectors.as_mut_ptr() as u64;
        res.encoder_id_ptr = encoders.as_mut_ptr() as u64;
        res.count_crtcs = crtcs.len() as u32;
        res.count_connectors = connectors.len() as u32;
        res.count_encoders = encoders.len() as u32;

        let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        if res.count_connectors == 0 || res.count_crtcs == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No DRM connectors or CRTCs found",
            ));
        }

        let connector_id = connectors[0];
        let crtc_id = crtcs[0];

        // 2. Query connector modes
        let mut conn = DrmModeGetConnector::default();
        let mut modes = [DrmModeModeInfo::default(); 32];
        conn.connector_id = connector_id;
        conn.modes_ptr = modes.as_mut_ptr() as u64;
        conn.count_modes = modes.len() as u32;

        let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let selected_mode = if conn.count_modes > 0 {
            modes[0]
        } else {
            // Fallback synthesis for virtual displays (QEMU virtio-gpu)
            let mut name = [0u8; 32];
            let name_bytes = b"1080x2400\0";
            name[..name_bytes.len()].copy_from_slice(name_bytes);
            DrmModeModeInfo {
                clock: 74250,
                hdisplay: 1080,
                hsync_start: 1120,
                hsync_end: 1140,
                htotal: 1200,
                vdisplay: 2400,
                vsync_start: 2410,
                vsync_end: 2420,
                vtotal: 2450,
                vrefresh: 60,
                mode_type: DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED,
                name,
                ..Default::default()
            }
        };

        let width = selected_mode.hdisplay as u32;
        let height = selected_mode.vdisplay as u32;

        // 3. Create dumb buffer
        let mut create_dumb = DrmModeCreateDumb {
            width,
            height,
            bpp: 32,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut create_dumb) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        // 4. Add framebuffer
        let mut fb_cmd = DrmModeFbCmd {
            fb_id: 0,
            width,
            height,
            pitch: create_dumb.pitch,
            bpp: 32,
            depth: 24,
            handle: create_dumb.handle,
        };
        let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_ADDFB, &mut fb_cmd) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            let mut destroy = DrmModeDestroyDumb {
                handle: create_dumb.handle,
            };
            unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy) };
            return Err(err);
        }

        // 5. Map dumb buffer into process virtual address space
        let mut map_dumb = DrmModeMapDumb {
            handle: create_dumb.handle,
            pad: 0,
            offset: 0,
        };
        let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut map_dumb) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            let mut destroy = DrmModeDestroyDumb {
                handle: create_dumb.handle,
            };
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_cmd.fb_id);
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            return Err(err);
        }

        let mmap_res = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                create_dumb.size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                map_dumb.offset as libc::off_t,
            )
        };
        if mmap_res == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            let mut destroy = DrmModeDestroyDumb {
                handle: create_dumb.handle,
            };
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_cmd.fb_id);
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            return Err(err);
        }
        let mmap_ptr = mmap_res as *mut u32;

        // 6. Set CRTC mode
        let mut conn_ids = [connector_id];
        let mut crtc = DrmModeCrtc {
            set_connectors_ptr: conn_ids.as_mut_ptr() as u64,
            count_connectors: 1,
            crtc_id,
            fb_id: fb_cmd.fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: selected_mode,
        };

        let ret = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut crtc) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::munmap(mmap_ptr as *mut libc::c_void, create_dumb.size as usize);
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_cmd.fb_id);
                let mut destroy = DrmModeDestroyDumb {
                    handle: create_dumb.handle,
                };
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            return Err(err);
        }

        Ok(Self {
            file,
            crtc_id,
            connector_id,
            fb_id: fb_cmd.fb_id,
            dumb_handle: create_dumb.handle,
            width,
            height,
            pitch: create_dumb.pitch,
            size: create_dumb.size as usize,
            mmap_ptr,
            mode: selected_mode,
            frame_cache_hash: 0,
            frame_cache_valid: false,
        })
    }

    /// Access mutable frame buffer pixel slice
    #[inline]
    pub fn buffer_mut(&mut self) -> &mut [u32] {
        let num_pixels = (self.height * (self.pitch / 4)) as usize;
        unsafe { std::slice::from_raw_parts_mut(self.mmap_ptr, num_pixels) }
    }

    /// Flush / trigger dirtyfb update to virtual display
    pub fn flush(&mut self) {
        let mut dirty = DrmModeFbDirtyCmd {
            fb_id: self.fb_id,
            flags: 0,
            color: 0,
            num_clips: 0,
            clips_ptr: 0,
        };
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), DRM_IOCTL_MODE_DIRTYFB, &mut dirty);
        }
    }
}

/// Lightweight summary of a terminal tab for zero-copy presentation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTabInfo<'a> {
    pub id: usize,
    pub title: &'a str,
    pub is_running: bool,
    pub is_active: bool,
}

/// Zero-allocation descriptor for an app icon displayed in the home screen grid
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppGridItem<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub color: u32,
    pub glyph: &'a str,
}

pub struct DrmInteractiveState<'a> {
    pub time_str: &'a str,
    pub is_locked: bool,
    pub cursor_pos: Option<(usize, usize)>,
    pub is_touching: bool,
    pub search_query: &'a str,
    pub search_active: bool,
    pub keyboard_active: bool,
    pub shade_open: bool,
    pub quick_tiles_active: [bool; 8],
    pub active_app: Option<&'a str>,
    pub terminal_lines: &'a [String],
    pub terminal_input: &'a str,
    pub terminal_running: bool,
    pub terminal_tabs: &'a [TerminalTabInfo<'a>],
    pub terminal_active_tab: usize,
    pub grid_apps: &'a [AppGridItem<'a>],
}

impl<'a> Default for DrmInteractiveState<'a> {
    fn default() -> Self {
        Self {
            time_str: "12:00",
            is_locked: false,
            cursor_pos: None,
            is_touching: false,
            search_query: "",
            search_active: false,
            keyboard_active: false,
            shade_open: false,
            quick_tiles_active: [true, true, true, false, true, false, false, false],
            active_app: None,
            terminal_lines: &[],
            terminal_input: "",
            terminal_running: false,
            terminal_tabs: &[],
            terminal_active_tab: 0,
            grid_apps: &[],
        }
    }
}

impl DrmKmsDevice {
    /// Backward-compatible wrapper for static rendering
    pub fn render_mobile_ui(&mut self, time_str: &str, is_locked: bool) {
        let state = DrmInteractiveState {
            time_str,
            is_locked,
            ..Default::default()
        };
        self.render_interactive_ui(&state);
    }

    /// Render the Universal Treble Mobile Launcher, SystemUI, or active App with live interactivity.
    /// Skips the full software redraw when the visible state hash is unchanged
    /// (damage-tracking fast path: saves ~2.6M px/frame on idle screens).
    pub fn render_interactive_ui(&mut self, state: &DrmInteractiveState) {
        let hash = interactive_state_hash(state);
        if self.frame_cache_valid && hash == self.frame_cache_hash {
            return;
        }
        self.frame_cache_hash = hash;
        self.frame_cache_valid = true;

        let w = self.width as usize;
        let h = self.height as usize;
        let stride = (self.pitch / 4) as usize;
        let buf = self.buffer_mut();

        // 1. Background: Modern Sleek Deep Space Mobile Gradient
        // Top: #0a1128 -> Center: #080d1a -> Bottom: #04060b
        for y in 0..h {
            let t = y as f32 / h as f32;
            let r = ((1.0 - t) * 12.0 + t * 4.0) as u32;
            let g = ((1.0 - t) * 20.0 + t * 6.0) as u32;
            let b = ((1.0 - t) * 45.0 + t * 15.0) as u32;
            let row_offset = y * stride;
            let pixel = (0xFF << 24) | (r << 16) | (g << 8) | b;
            for x in 0..w {
                buf[row_offset + x] = pixel;
            }
        }

        // Subtle ambient glowing mesh accents (Cyan top-right, Violet bottom-left)
        draw_glow_circle(buf, stride, w, h, (w * 8) / 10, h / 8, 300, 0x00, 0x99, 0xff, 25);
        draw_glow_circle(buf, stride, w, h, (w * 2) / 10, (h * 8) / 10, 350, 0x88, 0x33, 0xff, 20);

        // 2. SystemUI Status Bar (Y: 0 .. 44)
        draw_rect(buf, stride, w, h, 0, 0, w, 44, 0x22000000);
        draw_text(buf, stride, w, h, 24, 14, state.time_str, 0xFFFFFFFF, 2);

        // Status Icons (Right side of status bar)
        let icon_right = w - 24;
        draw_battery(buf, stride, w, h, icon_right - 40, 14, 98);
        if state.quick_tiles_active[0] {
            draw_wifi(buf, stride, w, h, icon_right - 80, 14);
        }
        let rat_label = if state.quick_tiles_active[1] { "5G" } else { "OFF" };
        let rat_color = if state.quick_tiles_active[1] { 0xFFFFFFFF } else { 0xFF888888 };
        draw_text(buf, stride, w, h, icon_right - 135, 14, rat_label, rat_color, 2);

        if state.is_locked {
            // Lock Screen UI
            let center_x = w / 2;
            let center_y = h / 3;
            draw_text_centered(buf, stride, w, h, center_x, center_y, state.time_str, 0xFFFFFFFF, 6);
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                center_x,
                center_y + 80,
                "Tuesday, Sep 22",
                0xFFB0C4DE,
                2,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                center_x,
                h - 180,
                "Click or Swipe up to unlock",
                0xFF8899A6,
                2,
            );
            draw_padlock(buf, stride, w, h, center_x, center_y - 80);
        } else if state.shade_open {
            // 3. Full-Screen Quick Settings & Notification Shade
            draw_rect(buf, stride, w, h, 0, 44, w, h - 44, 0xDD080D1A);
            
            // Header clock & date
            draw_text(buf, stride, w, h, 36, 65, state.time_str, 0xFFFFFFFF, 4);
            draw_text(buf, stride, w, h, 36, 115, "Tue, Sep 22 | Universal Treble GSI", 0xFF94A3B8, 2);

            // Quick Settings Tiles (2 columns x 4 rows)
            let tile_names = [
                "Wi-Fi",
                "Mobile Data",
                "Bluetooth",
                "Flashlight",
                "Auto-rotate",
                "Airplane mode",
                "Battery Saver",
                "Hotspot",
            ];
            let tile_w = (w - 72 - 20) / 2;
            let tile_h = 70;
            let tile_start_y = 155;

            for (idx, name) in tile_names.iter().enumerate() {
                let col = idx % 2;
                let row = idx / 2;
                let tx = 36 + col * (tile_w + 20);
                let ty = tile_start_y + row * (tile_h + 14);

                let is_active = state.quick_tiles_active[idx];
                let bg_color = if is_active { 0xFF2563EB } else { 0xFF1E293B };
                let text_color = if is_active { 0xFFFFFFFF } else { 0xFF94A3B8 };
                let status_str = if is_active { "ON" } else { "OFF" };

                draw_rounded_rect(buf, stride, w, h, tx, ty, tile_w, tile_h, 16, bg_color);
                draw_text(buf, stride, w, h, tx + 18, ty + 18, name, text_color, 2);
                draw_text(buf, stride, w, h, tx + 18, ty + 42, status_str, if is_active { 0xFF93C5FD } else { 0xFF64748B }, 1);
            }

            // Brightness Slider Bar
            let slider_y = tile_start_y + 4 * (tile_h + 14) + 10;
            let slider_w = w - 72;
            draw_rounded_rect(buf, stride, w, h, 36, slider_y, slider_w, 42, 21, 0xFF1E293B);
            let fill_w = (slider_w * 78) / 100;
            draw_rounded_rect(buf, stride, w, h, 36, slider_y, fill_w, 42, 21, 0xFF38BDF8);
            draw_text(buf, stride, w, h, 54, slider_y + 14, "* Brightness: 78%", 0xFF082F49, 2);

            // Notifications List Section
            let notif_y = slider_y + 65;
            draw_text(buf, stride, w, h, 36, notif_y, "NOTIFICATIONS", 0xFF64748B, 2);

            // Notification Card 1
            draw_rounded_rect(buf, stride, w, h, 36, notif_y + 26, w - 72, 85, 18, 0xFF1E293B);
            draw_text(buf, stride, w, h, 56, notif_y + 40, "UTIM PID 1 & UTLC Wayland", 0xFFF8FAFC, 2);
            draw_text(buf, stride, w, h, 56, notif_y + 68, "Interactive mobile compositor active with < 8ms input response", 0xFF94A3B8, 1);

            // Notification Card 2
            draw_rounded_rect(buf, stride, w, h, 36, notif_y + 125, w - 72, 85, 18, 0xFF1E293B);
            draw_text(buf, stride, w, h, 56, notif_y + 139, "Direct DRM KMS Scanout", 0xFFF8FAFC, 2);
            draw_text(buf, stride, w, h, 56, notif_y + 167, "1080x2400 @ 120Hz native scanout via /dev/dri/card0", 0xFF94A3B8, 1);

            // Pull handle at bottom
            let handle_y = h - 60;
            draw_rounded_rect(buf, stride, w, h, (w - 140) / 2, handle_y, 140, 6, 3, 0xFF64748B);
            draw_text_centered(buf, stride, w, h, w / 2, handle_y - 25, "Tap to close", 0xFF94A3B8, 1);
        } else if let Some(app_name) = state.active_app {
            // 4. Active Application Window View
            // Top App Bar
            let bar_h = 56;
            let bar_y = 48;
            draw_rounded_rect(buf, stride, w, h, 16, bar_y, w - 32, bar_h, 16, 0xFF1E293B);

            // Back button
            draw_rounded_rect(buf, stride, w, h, 26, bar_y + 8, 95, 40, 10, 0xFF334155);
            draw_text(buf, stride, w, h, 38, bar_y + 18, "< Back", 0xFFF8FAFC, 2);

            // App title
            draw_text_centered(buf, stride, w, h, w / 2, bar_y + 18, app_name, 0xFFFFFFFF, 2);

            // Close button
            draw_rounded_rect(buf, stride, w, h, w - 85, bar_y + 8, 60, 40, 10, 0xFFEF4444);
            draw_text(buf, stride, w, h, w - 63, bar_y + 18, "X", 0xFFFFFFFF, 2);

            // App Content Container
            let content_y = bar_y + bar_h + 12;
            let content_h = if state.keyboard_active {
                h - content_y - 450
            } else {
                h - content_y - 50
            };
            draw_rounded_rect(buf, stride, w, h, 16, content_y, w - 32, content_h, 18, 0xFF0A0E17);

            if app_name == "Terminal" {
                // Interactive Linux Shell Terminal Window with Tabbed Multi-Terminal Bar
                let tab_bar_y = content_y + 12;
                let tab_bar_h = 42;

                // Render tabs if available
                let default_single_tab = [TerminalTabInfo {
                    id: 1,
                    title: "Tab 1: bash",
                    is_running: state.terminal_running,
                    is_active: true,
                }];
                let tabs = if !state.terminal_tabs.is_empty() {
                    state.terminal_tabs
                } else {
                    &default_single_tab[..]
                };

                let start_x = 36;
                let tab_w = 200;
                let spacing = 10;

                for (i, tab) in tabs.iter().enumerate().take(4) {
                    let tab_x = start_x + i * (tab_w + spacing);
                    let is_active = tab.is_active || (tabs.len() == 1 && i == 0);

                    // Tab background: vibrant sky highlight for active tab, subtle slate for inactive
                    let bg_color = if is_active {
                        0xFF0284C7 // Sky-600
                    } else {
                        0xFF1E293B // Slate-800
                    };
                    draw_rounded_rect(buf, stride, w, h, tab_x, tab_bar_y, tab_w, tab_bar_h, 8, bg_color);

                    // Running indicator dot
                    let text_offset_x = if tab.is_running {
                        draw_rounded_rect(buf, stride, w, h, tab_x + 10, tab_bar_y + 16, 10, 10, 5, 0xFF10B981); // Emerald dot
                        tab_x + 26
                    } else {
                        tab_x + 12
                    };

                    // Tab title (Scale 2)
                    let text_color = if is_active { 0xFFFFFFFF } else { 0xFF94A3B8 };
                    draw_text(buf, stride, w, h, text_offset_x, tab_bar_y + 12, tab.title, text_color, 2);

                    // Close indicator 'x' on active tab when multiple tabs open
                    if is_active && tabs.len() > 1 {
                        draw_text(buf, stride, w, h, tab_x + tab_w - 20, tab_bar_y + 12, "x", 0xFFE2E8F0, 2);
                    }
                }

                // Add Tab button [+] if less than 4 tabs
                if tabs.len() < 4 {
                    let plus_x = start_x + tabs.len() * (tab_w + spacing);
                    draw_rounded_rect(buf, stride, w, h, plus_x, tab_bar_y, 56, tab_bar_h, 8, 0xFF334155);
                    draw_text_centered(buf, stride, w, h, plus_x + 28, tab_bar_y + 10, "+", 0xFF38BDF8, 3);
                }

                // Divider line below tab bar
                draw_rect(buf, stride, w, h, 24, tab_bar_y + tab_bar_h + 6, w - 48, 2, 0xFF1E293B);

                let mut line_y = tab_bar_y + tab_bar_h + 16;
                draw_text(buf, stride, w, h, 36, line_y, "Universal Treble Linux 1.0 (Debian Sid ARM64)", 0xFF38BDF8, 3);
                line_y += 38;
                draw_text(buf, stride, w, h, 36, line_y, "Linux 6.1.23-android14-4-00257 (Android GKI)", 0xFF94A3B8, 2);
                line_y += 26;
                draw_text(buf, stride, w, h, 36, line_y, "UTIM PID 1 init | UTLC Wayland Compositor", 0xFF94A3B8, 2);
                line_y += 26;
                draw_text(buf, stride, w, h, 36, line_y, "Debian Sid ARM64 GNU/Linux - Multi-Tab Terminal Active", 0xFF64748B, 2);
                line_y += 38;

                // Terminal text scaling (Scale 3 = 18x21px font cell, 34px line height)
                let line_h = 34;
                let header_used = line_y - content_y;
                let available_h = content_h.saturating_sub(header_used + 45);
                let max_lines = (available_h / line_h).saturating_sub(1);

                // Auto-scroll viewport: show the most recent lines so the active prompt is always visible
                let visible_lines = if state.terminal_lines.len() > max_lines {
                    &state.terminal_lines[state.terminal_lines.len() - max_lines..]
                } else {
                    state.terminal_lines
                };

                for line in visible_lines {
                    if line_y + line_h <= content_y + content_h - 40 {
                        draw_text(buf, stride, w, h, 36, line_y, line, 0xFFE2E8F0, 3);
                        line_y += line_h;
                    }
                }

                // Active prompt line with typed characters and blinking cursor (Scale 3)
                if line_y + line_h <= content_y + content_h {
                    if state.terminal_running {
                        draw_text(buf, stride, w, h, 36, line_y, "[running... (Ctrl+C to stop)]", 0xFFF59E0B, 3);
                    } else {
                        draw_text(buf, stride, w, h, 36, line_y, "root@treble-gsi:~# ", 0xFF10B981, 3);
                        let prompt_w = 19 * 18;
                        draw_text(buf, stride, w, h, 36 + prompt_w, line_y, state.terminal_input, 0xFFFFFFFF, 3);
                        let cursor_x = 36 + prompt_w + state.terminal_input.len() * 18;
                        draw_rect(buf, stride, w, h, cursor_x, line_y, 14, 22, 0xFF10B981);
                    }
                }
            } else if app_name == "Settings" {
                // Interactive Mobile Settings Page
                let mut card_y = content_y + 24;
                let cards = [
                    ("Network & Internet", "Wi-Fi, Mobile, Hotspot, VPN"),
                    ("Connected Devices", "Bluetooth, Android HAL bridge"),
                    ("Display & Graphics", "1080x2400 @ 120Hz Direct DRM KMS"),
                    ("Sound & Multimedia", "PipeWire spa-droid Audio"),
                    ("Storage", "4.00 GB ext4 System GSI Image"),
                    ("Battery", "98% - Mobile Power Governor active"),
                    ("About Phone", "Universal Treble Linux (Android 14 GKI)"),
                ];
                for (title, desc) in cards {
                    if card_y + 70 < content_y + content_h {
                        draw_rounded_rect(buf, stride, w, h, 32, card_y, w - 64, 60, 12, 0xFF1E293B);
                        draw_text(buf, stride, w, h, 48, card_y + 12, title, 0xFFF8FAFC, 2);
                        draw_text(buf, stride, w, h, 48, card_y + 36, desc, 0xFF94A3B8, 1);
                        card_y += 72;
                    }
                }
            } else if app_name == "Firefox" || app_name == "Browser" || app_name.contains("Firefox") {
                // Interactive Modern Mobile Browser Window
                let bar_top = content_y + 12;
                let bar_h = 46;
                let pad = 16;
                let url_w = w - pad * 2 - 32;

                // Browser Navigation & Address Bar
                draw_rounded_rect(buf, stride, w, h, pad + 16, bar_top, url_w, bar_h, 14, 0xFF1E293B);
                // SSL Lock indicator (Emerald)
                draw_rounded_rect(buf, stride, w, h, pad + 28, bar_top + 14, 16, 16, 4, 0xFF10B981);
                draw_text(buf, stride, w, h, pad + 32, bar_top + 15, "*", 0xFFFFFFFF, 1);
                // URL display
                draw_text(buf, stride, w, h, pad + 54, bar_top + 14, "https://duckduckgo.com", 0xFFF8FAFC, 2);
                // Reload icon on right side
                draw_text(buf, stride, w, h, pad + 16 + url_w - 30, bar_top + 14, "O", 0xFF94A3B8, 2);

                // Browser Content View
                let page_y = bar_top + bar_h + 16;
                let page_h = content_h.saturating_sub(bar_h + 36);
                draw_rounded_rect(buf, stride, w, h, pad + 16, page_y, url_w, page_h, 16, 0xFF0F172A);

                // Firefox Branding & Status
                let center_x = w / 2;
                draw_rounded_rect(buf, stride, w, h, center_x - 36, page_y + 36, 72, 72, 20, 0xFFFF5722);
                draw_text_centered(buf, stride, w, h, center_x, page_y + 54, "F", 0xFFFFFFFF, 4);

                draw_text_centered(buf, stride, w, h, center_x, page_y + 130, "Firefox Web Browser", 0xFFFFFFFF, 3);
                draw_text_centered(buf, stride, w, h, center_x, page_y + 165, "Wayland Native Mobile Client (wayland-0)", 0xFF10B981, 2);

                // Quick dial shortcuts
                let qd_y = page_y + 210;
                let qd_w = (url_w - 40) / 2;
                let qd_h = 64;

                let shortcuts = [
                    ("DuckDuckGo", "Web Search", 0xFFDE5833),
                    ("Debian Sid", "Package Archive", 0xFFD70A53),
                    ("Treble Linux", "GSI Mobile Docs", 0xFF3B82F6),
                    ("GitHub", "Code Repository", 0xFF24292F),
                ];

                for (idx, (stitle, ssub, scolor)) in shortcuts.iter().enumerate() {
                    let col = idx % 2;
                    let row = idx / 2;
                    let sx = pad + 20 + col * (qd_w + 12);
                    let sy = qd_y + row * (qd_h + 14);
                    if sy + qd_h < page_y + page_h - 20 {
                        draw_rounded_rect(buf, stride, w, h, sx, sy, qd_w, qd_h, 12, 0xFF1E293B);
                        draw_rounded_rect(buf, stride, w, h, sx + 12, sy + 14, 36, 36, 8, *scolor);
                        draw_text_centered(buf, stride, w, h, sx + 30, sy + 22, &stitle[..1], 0xFFFFFFFF, 2);
                        draw_text(buf, stride, w, h, sx + 56, sy + 14, stitle, 0xFFFFFFFF, 2);
                        draw_text(buf, stride, w, h, sx + 56, sy + 38, ssub, 0xFF94A3B8, 1);
                    }
                }
            } else {
                // Generic Modern Mobile App Screen
                draw_text_centered(buf, stride, w, h, w / 2, content_y + 80, app_name, 0xFF38BDF8, 4);
                draw_text_centered(buf, stride, w, h, w / 2, content_y + 130, "Universal Treble Linux Mobile Application", 0xFF94A3B8, 2);
                draw_rounded_rect(buf, stride, w, h, (w - 200) / 2, content_y + 180, 200, 50, 14, 0xFF3B82F6);
                draw_text_centered(buf, stride, w, h, w / 2, content_y + 195, "Action Ready", 0xFFFFFFFF, 2);
            }

            // Bottom Navigation Pill
            let nav_y = h - 20;
            let nav_w = 140;
            let nav_x = (w - nav_w) / 2;
            draw_rounded_rect(buf, stride, w, h, nav_x, nav_y, nav_w, 5, 2, 0xFFFFFFFF);
        } else {
            // 5. Digital Clock & Date Widget on Home Screen
            let widget_y = 120;
            draw_text_centered(buf, stride, w, h, w / 2, widget_y, state.time_str, 0xFFFFFFFF, 5);
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                w / 2,
                widget_y + 65,
                "Tue, Sep 22  |  28 C Sunny",
                0xFF88A0C0,
                2,
            );

            // 6. Google / Treble Search Pill Widget
            let search_y = widget_y + 115;
            let search_w = w - 64;
            let search_x = 32;
            let pill_bg = if state.search_active { 0xFF334155 } else { 0xFF2A3345 };
            let pill_border = if state.search_active { 0xFF38BDF8 } else { 0xFF475569 };
            draw_rounded_rect(buf, stride, w, h, search_x - 2, search_y - 2, search_w + 4, 60, 30, pill_border);
            draw_rounded_rect(buf, stride, w, h, search_x, search_y, search_w, 56, 28, pill_bg);
            draw_text(buf, stride, w, h, search_x + 20, search_y + 16, "G", 0xFF4285F4, 3);

            if state.search_active {
                let disp_query = if state.search_query.is_empty() {
                    "Type to search..."
                } else {
                    state.search_query
                };
                let q_color = if state.search_query.is_empty() { 0xFF94A3B8 } else { 0xFFFFFFFF };
                draw_text(buf, stride, w, h, search_x + 55, search_y + 18, disp_query, q_color, 2);
                let cur_x = search_x + 55 + (if state.search_query.is_empty() { 0 } else { state.search_query.len() * 12 });
                draw_rect(buf, stride, w, h, cur_x, search_y + 16, 2, 24, 0xFF38BDF8);
            } else {
                draw_text(buf, stride, w, h, search_x + 55, search_y + 18, "Search apps, web...", 0xFF8A99AD, 2);
                draw_text(buf, stride, w, h, search_x + search_w - 36, search_y + 16, "*", 0xFFEA4335, 3);
            }

            // 7. App Grid Icons (4 columns x dynamic rows)
            let grid_top = search_y + 90;
            let cols = 4;
            let col_width = w / cols;
            let icon_size = 64;

            let default_apps = [
                AppGridItem { id: "phone", name: "Phone", color: 0xFF10B981, glyph: "P" },
                AppGridItem { id: "messages", name: "Messages", color: 0xFF3B82F6, glyph: "M" },
                AppGridItem { id: "browser", name: "Browser", color: 0xFF06B6D4, glyph: "B" },
                AppGridItem { id: "camera", name: "Camera", color: 0xFFF43F5E, glyph: "C" },
                AppGridItem { id: "gallery", name: "Gallery", color: 0xFF8B5CF6, glyph: "G" },
                AppGridItem { id: "settings", name: "Settings", color: 0xFF64748B, glyph: "S" },
                AppGridItem { id: "files", name: "Files", color: 0xFFF59E0B, glyph: "F" },
                AppGridItem { id: "music", name: "Music", color: 0xFFD946EF, glyph: "M" },
                AppGridItem { id: "terminal", name: "Terminal", color: 0xFF1E293B, glyph: ">" },
                AppGridItem { id: "treble", name: "Treble OS", color: 0xFF6366F1, glyph: "U" },
                AppGridItem { id: "contacts", name: "Contacts", color: 0xFF14B8A6, glyph: "C" },
                AppGridItem { id: "clock", name: "Clock", color: 0xFFEF4444, glyph: "T" },
            ];

            let apps: &[AppGridItem] = if !state.grid_apps.is_empty() {
                state.grid_apps
            } else {
                &default_apps[..]
            };

            let dock_h = 100;
            let dock_y = h - dock_h - 40;
            let max_rows = (dock_y.saturating_sub(grid_top + 20)) / 115;
            let max_apps = max_rows * cols;

            for (idx, app) in apps.iter().take(max_apps).enumerate() {
                let row = idx / cols;
                let col = idx % cols;
                let cx = col * col_width + col_width / 2;
                let cy = grid_top + row * 115;

                let ix = cx.saturating_sub(icon_size / 2);
                let iy = cy.saturating_sub(icon_size / 2);
                draw_rounded_rect(buf, stride, w, h, ix, iy, icon_size, icon_size, 16, app.color);
                draw_text_centered(buf, stride, w, h, cx, cy - 10, app.glyph, 0xFFFFFFFF, 3);
                draw_text_centered(buf, stride, w, h, cx, cy + 42, app.name, 0xFFE2E8F0, 1);
            }

            // 8. Persistent Hotseat Dock at Bottom
            let dock_h = 100;
            let dock_y = h - dock_h - 40;
            let dock_w = w - 40;
            let dock_x = 20;

            draw_rounded_rect(buf, stride, w, h, dock_x, dock_y, dock_w, dock_h, 32, 0xFF182236);

            let dock_apps = [
                ("Phone", 0xFF10B981, "P"),
                ("Messages", 0xFF3B82F6, "M"),
                ("Apps", 0xFF475569, ":"),
                ("Browser", 0xFF06B6D4, "B"),
                ("Camera", 0xFFF43F5E, "C"),
            ];

            let dock_col_w = dock_w / dock_apps.len();
            for (i, (_name, color, glyph)) in dock_apps.iter().enumerate() {
                let cx = dock_x + i * dock_col_w + dock_col_w / 2;
                let cy = dock_y + dock_h / 2;
                let d_size = 54;
                draw_rounded_rect(
                    buf,
                    stride,
                    w,
                    h,
                    cx.saturating_sub(d_size / 2),
                    cy.saturating_sub(d_size / 2),
                    d_size,
                    d_size,
                    16,
                    *color,
                );
                draw_text_centered(buf, stride, w, h, cx, cy - 8, glyph, 0xFFFFFFFF, 2);
            }

            // 9. Gesture Navigation Bar (Pill at bottom)
            let nav_y = h - 20;
            let nav_w = 140;
            let nav_x = (w - nav_w) / 2;
            draw_rounded_rect(buf, stride, w, h, nav_x, nav_y, nav_w, 5, 2, 0xFFFFFFFF);
        }

        // 10. Virtual Keyboard (Gboard Style) when active
        if state.keyboard_active && !state.is_locked && !state.shade_open {
            let kb_h = 420;
            let kb_y = h - kb_h - 20;
            let kb_w = w - 24;
            let kb_x = 12;

            draw_rounded_rect(buf, stride, w, h, kb_x, kb_y, kb_w, kb_h, 24, 0xF0111827);
            draw_rect(buf, stride, w, h, kb_x + 10, kb_y + 1, kb_w - 20, 2, 0xFF334155);

            // Row 1: Q W E R T Y U I O P (10 keys)
            let row1 = ["Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P"];
            let r1_key_w = (kb_w - 30) / 10;
            let key_h = 65;
            let r1_y = kb_y + 25;
            for (i, k) in row1.iter().enumerate() {
                let kx = kb_x + 15 + i * r1_key_w;
                draw_rounded_rect(buf, stride, w, h, kx + 2, r1_y, r1_key_w - 4, key_h, 10, 0xFF334155);
                draw_text_centered(buf, stride, w, h, kx + r1_key_w / 2, r1_y + 18, k, 0xFFFFFFFF, 3);
            }

            // Row 2: A S D F G H J K L (9 keys)
            let row2 = ["A", "S", "D", "F", "G", "H", "J", "K", "L"];
            let r2_key_w = (kb_w - 60) / 9;
            let r2_y = r1_y + key_h + 12;
            let r2_offset = kb_x + 30;
            for (i, k) in row2.iter().enumerate() {
                let kx = r2_offset + i * r2_key_w;
                draw_rounded_rect(buf, stride, w, h, kx + 2, r2_y, r2_key_w - 4, key_h, 10, 0xFF334155);
                draw_text_centered(buf, stride, w, h, kx + r2_key_w / 2, r2_y + 18, k, 0xFFFFFFFF, 3);
            }

            // Row 3: [SHIFT] Z X C V B N M [DEL]
            let r3_y = r2_y + key_h + 12;
            let special_w = 95;
            let mid_w = (kb_w - 30 - special_w * 2) / 7;
            // Shift
            draw_rounded_rect(buf, stride, w, h, kb_x + 15, r3_y, special_w - 4, key_h, 10, 0xFF1E293B);
            draw_text_centered(buf, stride, w, h, kb_x + 15 + special_w / 2, r3_y + 22, "^", 0xFFFFFFFF, 3);

            let row3 = ["Z", "X", "C", "V", "B", "N", "M"];
            for (i, k) in row3.iter().enumerate() {
                let kx = kb_x + 15 + special_w + i * mid_w;
                draw_rounded_rect(buf, stride, w, h, kx + 2, r3_y, mid_w - 4, key_h, 10, 0xFF334155);
                draw_text_centered(buf, stride, w, h, kx + mid_w / 2, r3_y + 18, k, 0xFFFFFFFF, 3);
            }

            // Backspace / Del
            let del_x = kb_x + 15 + special_w + 7 * mid_w;
            draw_rounded_rect(buf, stride, w, h, del_x + 2, r3_y, special_w - 4, key_h, 10, 0xFF1E293B);
            draw_text_centered(buf, stride, w, h, del_x + special_w / 2, r3_y + 22, "<-", 0xFFFFFFFF, 2);

            // Row 4: [?123] [SPACE] [ENTER]
            let r4_y = r3_y + key_h + 12;
            let sym_w = 120;
            let enter_w = 140;
            let space_w = kb_w - 30 - sym_w - enter_w;

            draw_rounded_rect(buf, stride, w, h, kb_x + 15, r4_y, sym_w - 4, key_h, 10, 0xFF1E293B);
            draw_text_centered(buf, stride, w, h, kb_x + 15 + sym_w / 2, r4_y + 22, "Hide", 0xFF94A3B8, 2);

            let space_x = kb_x + 15 + sym_w;
            draw_rounded_rect(buf, stride, w, h, space_x + 2, r4_y, space_w - 4, key_h, 10, 0xFF334155);
            draw_text_centered(buf, stride, w, h, space_x + space_w / 2, r4_y + 22, "English", 0xFF94A3B8, 2);

            let enter_x = space_x + space_w;
            draw_rounded_rect(buf, stride, w, h, enter_x + 2, r4_y, enter_w - 4, key_h, 10, 0xFF3B82F6);
            draw_text_centered(buf, stride, w, h, enter_x + enter_w / 2, r4_y + 22, "Enter", 0xFFFFFFFF, 2);
        }

        // 11. Interactive Touch Ripple / Cursor Pointer
        if let Some((cx, cy)) = state.cursor_pos {
            if state.is_touching {
                // Vibrant glowing ripple when touching or clicking
                draw_glow_circle(buf, stride, w, h, cx, cy, 26, 0x00, 0xE5, 0xFF, 50);
                draw_rounded_rect(buf, stride, w, h, cx.saturating_sub(8), cy.saturating_sub(8), 16, 16, 8, 0xFFFFFFFF);
            } else {
                // Sleek, modern subtle pointer dot for cursor hovering
                draw_rounded_rect(buf, stride, w, h, cx.saturating_sub(5), cy.saturating_sub(5), 10, 10, 5, 0xAAFFFFFF);
                draw_rounded_rect(buf, stride, w, h, cx.saturating_sub(2), cy.saturating_sub(2), 4, 4, 2, 0xFF00E5FF);
            }
        }
    }
}

impl Drop for DrmKmsDevice {
    fn drop(&mut self) {
        if !self.mmap_ptr.is_null() && self.size > 0 {
            unsafe {
                libc::munmap(self.mmap_ptr as *mut libc::c_void, self.size);
            }
        }
        if self.fb_id > 0 {
            let fd = self.file.as_raw_fd();
            let mut fb_id = self.fb_id;
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_id);
            }
        }
        if self.dumb_handle > 0 {
            let fd = self.file.as_raw_fd();
            let mut destroy = DrmModeDestroyDumb {
                handle: self.dumb_handle,
            };
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
        }
    }
}

// --- Zero-Allocation Graphics Primitives ---

/// FNV-1a hash over the visible interactive state for the render-skip fast path.
fn interactive_state_hash(state: &DrmInteractiveState) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    };
    for b in state.time_str.bytes() {
        mix(b);
    }
    mix(state.is_locked as u8);
    mix(state.is_touching as u8);
    mix(state.search_active as u8);
    mix(state.keyboard_active as u8);
    mix(state.shade_open as u8);
    match state.cursor_pos {
        Some((x, y)) => {
            for b in x.to_ne_bytes() {
                mix(b);
            }
            for b in y.to_ne_bytes() {
                mix(b);
            }
        }
        None => mix(0xFF),
    }
    for b in state.search_query.bytes() {
        mix(b);
    }
    for (i, t) in state.quick_tiles_active.iter().enumerate() {
        if *t {
            mix(i as u8);
        }
    }
    match state.active_app {
        Some(a) => {
            mix(1);
            for b in a.bytes() {
                mix(b);
            }
        }
        None => mix(0),
    }
    for b in state.terminal_input.bytes() {
        mix(b);
    }
    mix(state.terminal_running as u8);
    mix(state.terminal_active_tab as u8);
    mix(state.terminal_tabs.len() as u8);
    for tab in state.terminal_tabs {
        mix(tab.id as u8);
        mix(tab.is_running as u8);
        mix(tab.is_active as u8);
        for b in tab.title.bytes() {
            mix(b);
        }
    }
    for b in state.terminal_lines.len().to_ne_bytes() {
        mix(b);
    }
    if let Some(last) = state.terminal_lines.last() {
        for b in last.bytes() {
            mix(b);
        }
    }
    mix(state.grid_apps.len() as u8);
    for app in state.grid_apps {
        for b in app.name.bytes() {
            mix(b);
        }
        for b in app.color.to_ne_bytes() {
            mix(b);
        }
    }
    h
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn draw_rect(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    color: u32,
) {
    let x_end = (x + rw).min(w);
    let y_end = (y + rh).min(h);
    for cy in y..y_end {
        let row = cy * stride;
        for cx in x..x_end {
            buf[row + cx] = color;
        }
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn draw_rounded_rect(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    radius: usize,
    color: u32,
) {
    // Clamp radius to half of the smallest dimension: prevents usize
    // underflow in `x + rw - radius` for small widgets (nav pill, dots).
    let radius = radius.min(rw / 2).min(rh / 2);
    let r2 = (radius * radius) as i32;
    let x_end = (x + rw).min(w);
    let y_end = (y + rh).min(h);

    for cy in y..y_end {
        let dy = if cy < y + radius {
            radius as i32 - (cy - y) as i32
        } else if cy >= y + rh - radius {
            (cy - (y + rh - radius)) as i32
        } else {
            0
        };

        let row = cy * stride;
        for cx in x..x_end {
            let dx = if cx < x + radius {
                radius as i32 - (cx - x) as i32
            } else if cx >= x + rw - radius {
                (cx - (x + rw - radius)) as i32
            } else {
                0
            };

            if dx * dx + dy * dy <= r2 {
                buf[row + cx] = color;
            }
        }
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn draw_glow_circle(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    cx: usize,
    cy: usize,
    radius: usize,
    r: u8,
    g: u8,
    b: u8,
    intensity: u8,
) {
    let r_min_x = cx.saturating_sub(radius);
    let r_max_x = (cx + radius).min(w);
    let r_min_y = cy.saturating_sub(radius);
    let r_max_y = (cy + radius).min(h);
    let rad_sq = (radius * radius) as f32;

    for y in r_min_y..r_max_y {
        let dy = (y as i32 - cy as i32) as f32;
        let row = y * stride;
        for x in r_min_x..r_max_x {
            let dx = (x as i32 - cx as i32) as f32;
            let d_sq = dx * dx + dy * dy;
            if d_sq < rad_sq {
                let falloff = 1.0 - (d_sq / rad_sq).sqrt();
                let alpha = (falloff * intensity as f32) as u32;
                let cur = buf[row + x];
                let cb = (cur & 0xFF) + ((b as u32 * alpha) >> 8);
                let cg = ((cur >> 8) & 0xFF) + ((g as u32 * alpha) >> 8);
                let cr = ((cur >> 16) & 0xFF) + ((r as u32 * alpha) >> 8);
                buf[row + x] = (0xFF << 24) | (cr.min(255) << 16) | (cg.min(255) << 8) | cb.min(255);
            }
        }
    }
}

fn draw_battery(buf: &mut [u32], stride: usize, w: usize, h: usize, x: usize, y: usize, pct: u32) {
    draw_rect(buf, stride, w, h, x, y, 24, 14, 0xFF8899A6);
    draw_rect(buf, stride, w, h, x + 2, y + 2, 20, 10, 0xFF000000);
    draw_rect(buf, stride, w, h, x + 24, y + 4, 3, 6, 0xFF8899A6);

    let fill_w = ((pct as usize * 18) / 100).max(2);
    let color = if pct > 20 { 0xFF10B981 } else { 0xFFEF4444 };
    draw_rect(buf, stride, w, h, x + 3, y + 3, fill_w, 8, color);
}

fn draw_wifi(buf: &mut [u32], stride: usize, w: usize, h: usize, x: usize, y: usize) {
    for r in &[12, 8, 4] {
        draw_glow_circle(buf, stride, w, h, x + 10, y + 14, *r, 255, 255, 255, 40);
    }
    draw_rounded_rect(buf, stride, w, h, x + 8, y + 10, 4, 4, 2, 0xFFFFFFFF);
}

fn draw_padlock(buf: &mut [u32], stride: usize, w: usize, h: usize, cx: usize, cy: usize) {
    draw_rounded_rect(buf, stride, w, h, cx - 14, cy - 20, 28, 20, 10, 0xFFE2E8F0);
    draw_rounded_rect(buf, stride, w, h, cx - 8, cy - 14, 16, 16, 8, 0xFF000000);
    draw_rounded_rect(buf, stride, w, h, cx - 18, cy - 6, 36, 26, 6, 0xFFE2E8F0);
}

// 5x7 Minimal Zero-Allocation Monospace Bitmap Font for Mobile UI
static FONT_5X7: [[u8; 5]; 96] = {
    let mut table = [[0u8; 5]; 96];
    table[0] = [0x00, 0x00, 0x00, 0x00, 0x00];  // Space
    table[1] = [0x00, 0x00, 0x5f, 0x00, 0x00];  // !
    table[2] = [0x00, 0x07, 0x00, 0x07, 0x00];  // "
    table[3] = [0x14, 0x7f, 0x14, 0x7f, 0x14];  // #
    table[4] = [0x24, 0x2a, 0x7f, 0x2a, 0x12];  // $
    table[5] = [0x23, 0x13, 0x08, 0x64, 0x62];  // %
    table[6] = [0x36, 0x49, 0x55, 0x22, 0x50];  // &
    table[7] = [0x00, 0x05, 0x03, 0x00, 0x00];  // '
    table[8] = [0x00, 0x1c, 0x22, 0x41, 0x00];  // (
    table[9] = [0x00, 0x41, 0x22, 0x1c, 0x00];  // )
    table[10] = [0x14, 0x08, 0x3e, 0x08, 0x14]; // *
    table[11] = [0x08, 0x08, 0x3e, 0x08, 0x08]; // +
    table[12] = [0x00, 0x50, 0x30, 0x00, 0x00]; // ,
    table[13] = [0x08, 0x08, 0x08, 0x08, 0x08]; // -
    table[14] = [0x00, 0x60, 0x60, 0x00, 0x00]; // .
    table[15] = [0x20, 0x10, 0x08, 0x04, 0x02]; // /
    table[16] = [0x3e, 0x51, 0x49, 0x45, 0x3e]; // 0
    table[17] = [0x00, 0x42, 0x7f, 0x40, 0x00]; // 1
    table[18] = [0x42, 0x61, 0x51, 0x49, 0x46]; // 2
    table[19] = [0x21, 0x41, 0x45, 0x4b, 0x31]; // 3
    table[20] = [0x18, 0x14, 0x12, 0x7f, 0x10]; // 4
    table[21] = [0x27, 0x45, 0x45, 0x45, 0x39]; // 5
    table[22] = [0x3c, 0x4a, 0x49, 0x49, 0x30]; // 6
    table[23] = [0x01, 0x71, 0x09, 0x05, 0x03]; // 7
    table[24] = [0x36, 0x49, 0x49, 0x49, 0x36]; // 8
    table[25] = [0x06, 0x49, 0x49, 0x29, 0x1e]; // 9
    table[26] = [0x00, 0x36, 0x36, 0x00, 0x00]; // :
    table[27] = [0x00, 0x56, 0x36, 0x00, 0x00]; // ;
    table[28] = [0x08, 0x14, 0x22, 0x41, 0x00]; // <
    table[29] = [0x14, 0x14, 0x14, 0x14, 0x14]; // =
    table[30] = [0x00, 0x41, 0x22, 0x14, 0x08]; // >
    table[31] = [0x02, 0x01, 0x51, 0x09, 0x06]; // ?
    table[32] = [0x32, 0x49, 0x79, 0x41, 0x3e]; // @
    table[33] = [0x7e, 0x11, 0x11, 0x11, 0x7e]; // A
    table[34] = [0x7f, 0x49, 0x49, 0x49, 0x36]; // B
    table[35] = [0x3e, 0x41, 0x41, 0x41, 0x22]; // C
    table[36] = [0x7f, 0x41, 0x41, 0x22, 0x1c]; // D
    table[37] = [0x7f, 0x49, 0x49, 0x49, 0x41]; // E
    table[38] = [0x7f, 0x09, 0x09, 0x09, 0x01]; // F
    table[39] = [0x3e, 0x41, 0x49, 0x49, 0x7a]; // G
    table[40] = [0x7f, 0x08, 0x08, 0x08, 0x7f]; // H
    table[41] = [0x00, 0x41, 0x7f, 0x41, 0x00]; // I
    table[42] = [0x20, 0x40, 0x41, 0x3f, 0x01]; // J
    table[43] = [0x7f, 0x08, 0x14, 0x22, 0x41]; // K
    table[44] = [0x7f, 0x40, 0x40, 0x40, 0x40]; // L
    table[45] = [0x7f, 0x02, 0x0c, 0x02, 0x7f]; // M
    table[46] = [0x7f, 0x04, 0x08, 0x10, 0x7f]; // N
    table[47] = [0x3e, 0x41, 0x41, 0x41, 0x3e]; // O
    table[48] = [0x7f, 0x09, 0x09, 0x09, 0x06]; // P
    table[49] = [0x3e, 0x41, 0x51, 0x21, 0x5e]; // Q
    table[50] = [0x7f, 0x09, 0x19, 0x29, 0x46]; // R
    table[51] = [0x46, 0x49, 0x49, 0x49, 0x31]; // S
    table[52] = [0x01, 0x01, 0x7f, 0x01, 0x01]; // T
    table[53] = [0x3f, 0x40, 0x40, 0x40, 0x3f]; // U
    table[54] = [0x1f, 0x20, 0x40, 0x20, 0x1f]; // V
    table[55] = [0x7f, 0x20, 0x18, 0x20, 0x7f]; // W
    table[56] = [0x63, 0x14, 0x08, 0x14, 0x63]; // X
    table[57] = [0x07, 0x08, 0x70, 0x08, 0x07]; // Y
    table[58] = [0x61, 0x51, 0x49, 0x45, 0x43]; // Z
    table[59] = [0x00, 0x7f, 0x41, 0x41, 0x00]; // [
    table[60] = [0x02, 0x04, 0x08, 0x10, 0x20]; // \
    table[61] = [0x00, 0x41, 0x41, 0x7f, 0x00]; // ]
    table[62] = [0x04, 0x02, 0x01, 0x02, 0x04]; // ^
    table[63] = [0x40, 0x40, 0x40, 0x40, 0x40]; // _
    table[64] = [0x00, 0x01, 0x02, 0x00, 0x00]; // `
    table[65] = [0x20, 0x54, 0x54, 0x54, 0x78]; // a
    table[66] = [0x7f, 0x48, 0x44, 0x44, 0x38]; // b
    table[67] = [0x38, 0x44, 0x44, 0x44, 0x20]; // c
    table[68] = [0x38, 0x44, 0x44, 0x48, 0x7f]; // d
    table[69] = [0x38, 0x54, 0x54, 0x54, 0x18]; // e
    table[70] = [0x08, 0x7e, 0x09, 0x01, 0x02]; // f
    table[71] = [0x0c, 0x52, 0x52, 0x52, 0x3e]; // g
    table[72] = [0x7f, 0x08, 0x04, 0x04, 0x78]; // h
    table[73] = [0x00, 0x44, 0x7d, 0x40, 0x00]; // i
    table[74] = [0x20, 0x40, 0x44, 0x3d, 0x00]; // j
    table[75] = [0x7f, 0x10, 0x28, 0x44, 0x00]; // k
    table[76] = [0x00, 0x41, 0x7f, 0x40, 0x00]; // l
    table[77] = [0x7c, 0x04, 0x18, 0x04, 0x78]; // m
    table[78] = [0x7c, 0x08, 0x04, 0x04, 0x78]; // n
    table[79] = [0x38, 0x44, 0x44, 0x44, 0x38]; // o
    table[80] = [0x7c, 0x14, 0x14, 0x14, 0x08]; // p
    table[81] = [0x08, 0x14, 0x14, 0x18, 0x7c]; // q
    table[82] = [0x7c, 0x08, 0x04, 0x04, 0x08]; // r
    table[83] = [0x48, 0x54, 0x54, 0x54, 0x20]; // s
    table[84] = [0x04, 0x3f, 0x44, 0x40, 0x20]; // t
    table[85] = [0x3c, 0x40, 0x40, 0x20, 0x7c]; // u
    table[86] = [0x1c, 0x20, 0x40, 0x20, 0x1c]; // v
    table[87] = [0x3c, 0x40, 0x30, 0x40, 0x3c]; // w
    table[88] = [0x44, 0x28, 0x10, 0x28, 0x44]; // x
    table[89] = [0x0c, 0x50, 0x50, 0x50, 0x3c]; // y
    table[90] = [0x44, 0x64, 0x54, 0x4c, 0x44]; // z
    table[91] = [0x00, 0x08, 0x36, 0x41, 0x00]; // {
    table[92] = [0x00, 0x00, 0x7f, 0x00, 0x00]; // |
    table[93] = [0x00, 0x41, 0x36, 0x08, 0x00]; // }
    table[94] = [0x08, 0x04, 0x08, 0x10, 0x08]; // ~
    table
};

#[allow(clippy::too_many_arguments)]
fn draw_text(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    mut x: usize,
    y: usize,
    text: &str,
    color: u32,
    scale: usize,
) {
    for b in text.bytes() {
        if (32..128).contains(&b) {
            let glyph = &FONT_5X7[(b - 32) as usize];
            for (col, &col_bits) in glyph.iter().enumerate() {
                for row in 0..7 {
                    if (col_bits & (1 << row)) != 0 {
                        let px = x + col * scale;
                        let py = y + row * scale;
                        draw_rect(buf, stride, w, h, px, py, scale, scale, color);
                    }
                }
            }
        }
        x += (5 + 1) * scale;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text_centered(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: usize,
    y: usize,
    text: &str,
    color: u32,
    scale: usize,
) {
    let char_w = 6 * scale;
    let total_w = text.len() * char_w;
    let x = center_x.saturating_sub(total_w / 2);
    draw_text(buf, stride, w, h, x, y, text, color, scale);
}
