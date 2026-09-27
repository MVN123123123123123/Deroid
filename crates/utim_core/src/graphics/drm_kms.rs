//! Direct Rendering Manager (DRM) Kernel Mode Setting (KMS) Hardware Scanout.
//! Provides zero-dependency, bare-metal hardware display presentation for Universal Treble Linux
//! via Linux DRM dumb buffers and CRTC modesetting. Adheres strictly to GEMINI.md systems rules.

// The framebuffer is addressed as (buf, stride, w, h) throughout: the render
// path is a bare pointer plus a stride, and wrapping that in a struct would
// have meant an allocation or a self-referential borrow on the hot path.
#![allow(clippy::too_many_arguments)]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::layout::{
    ShadeLayout,
    AppLayout, AppPanel, Keyboard, Layout, ICON_RADIUS, KB_ROW1, KB_ROW2, KB_ROW3_MID,
    LABEL_GAP, PANEL_PAD_FRACTION,
};
use super::png::RgbaImage;

// --- DRM KMS IOCTL Definitions (Standard Linux ABI) ---
const DRM_IOCTL_MODE_GETRESOURCES: libc::c_ulong = 0xc04064a0;
const DRM_IOCTL_MODE_GETCONNECTOR: libc::c_ulong = 0xc05064a7;
const DRM_IOCTL_MODE_GETENCODER: libc::c_ulong = 0xc01464a6;
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
struct DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
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
    frame_dirty: bool,
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

        // Pick the first *connected* connector, and follow its encoder to the
        // CRTC that can actually drive it. Index 0 of the resource list is not
        // guaranteed to be either.
        const DRM_MODE_CONNECTED: u32 = 1;
        let n_conn = (res.count_connectors as usize).min(connectors.len());
        let n_crtc = (res.count_crtcs as usize).min(crtcs.len());
        if n_conn == 0 || n_crtc == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "No DRM connectors or CRTCs found",
            ));
        }
        let mut picked: Option<(u32, u32)> = None;
        let mut picked_modes = [DrmModeModeInfo::default(); 32];
        let mut picked_count: usize = 0;
        // Phase 1: a connected connector with at least one mode.
        for &cid in &connectors[..n_conn] {
            let mut probe_modes = [DrmModeModeInfo::default(); 32];
            let mut encs = [0u32; 8];
            let mut probe = DrmModeGetConnector {
                connector_id: cid,
                modes_ptr: probe_modes.as_mut_ptr() as u64,
                count_modes: probe_modes.len() as u32,
                encoders_ptr: encs.as_mut_ptr() as u64,
                count_encoders: encs.len() as u32,
                ..Default::default()
            };
            if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut probe) } < 0 {
                continue;
            }
            if probe.connection != DRM_MODE_CONNECTED || probe.count_modes == 0 {
                continue;
            }
            let n_enc = (probe.count_encoders as usize).min(encs.len());
            let mut crtc_opt: Option<u32> = None;
            for &eid in &encs[..n_enc] {
                let mut enc = DrmModeGetEncoder {
                    encoder_id: eid,
                    ..Default::default()
                };
                if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &mut enc) } < 0 {
                    continue;
                }
                if enc.crtc_id != 0 {
                    crtc_opt = Some(enc.crtc_id);
                    break;
                }
                // Encoder is not bound: pick the first compatible CRTC from
                // the resource list via the possible_crtcs bitmask.
                for (idx, &ccid) in crtcs[..n_crtc].iter().enumerate() {
                    if idx < 32 && (enc.possible_crtcs >> idx) & 1 != 0 {
                        crtc_opt = Some(ccid);
                        break;
                    }
                }
                if crtc_opt.is_some() {
                    break;
                }
            }
            if let Some(c) = crtc_opt {
                let n = (probe.count_modes as usize).min(probe_modes.len());
                picked_modes.copy_from_slice(&probe_modes);
                picked_count = n;
                picked = Some((cid, c));
                break;
            }
        }
        // Phase 2: a connected connector with no modes (virtual display);
        // the caller below synthesises a fallback mode for it.
        if picked.is_none() {
            for &cid in &connectors[..n_conn] {
                let mut encs = [0u32; 8];
                let mut probe = DrmModeGetConnector {
                    connector_id: cid,
                    modes_ptr: 0,
                    count_modes: 0,
                    encoders_ptr: encs.as_mut_ptr() as u64,
                    count_encoders: encs.len() as u32,
                    ..Default::default()
                };
                if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut probe) } < 0 {
                    continue;
                }
                if probe.connection != DRM_MODE_CONNECTED {
                    continue;
                }
                let n_enc = (probe.count_encoders as usize).min(encs.len());
                let mut crtc_opt: Option<u32> = None;
                for &eid in &encs[..n_enc] {
                    let mut enc = DrmModeGetEncoder {
                        encoder_id: eid,
                        ..Default::default()
                    };
                    if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &mut enc) } < 0 {
                        continue;
                    }
                    if enc.crtc_id != 0 {
                        crtc_opt = Some(enc.crtc_id);
                        break;
                    }
                    for (idx, &ccid) in crtcs[..n_crtc].iter().enumerate() {
                        if idx < 32 && (enc.possible_crtcs >> idx) & 1 != 0 {
                            crtc_opt = Some(ccid);
                            break;
                        }
                    }
                    if crtc_opt.is_some() {
                        break;
                    }
                }
                // Last resort: first CRTC, but only for a connected connector.
                let c = crtc_opt.unwrap_or(crtcs[0]);
                picked = Some((cid, c));
                picked_count = 0;
                break;
            }
        }
        let (connector_id, crtc_id) = match picked {
            Some(p) => p,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no connected connector with a usable encoder",
                ))
            }
        };

        // 2. The mode for the picked connector: prefer the PREFERRED mode.
        let selected_mode = if picked_count > 0 {
            let mut best = picked_modes[0];
            for m in picked_modes[..picked_count].iter() {
                if m.mode_type & DRM_MODE_TYPE_PREFERRED != 0 {
                    best = *m;
                    break;
                }
            }
            best
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

        assert!(create_dumb.pitch.is_multiple_of(4), "pitch must be a multiple of 4 for XRGB8888");
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
            frame_dirty: false,
        })
    }

    /// Access mutable frame buffer pixel slice
    #[inline]
    pub fn buffer_mut(&mut self) -> &mut [u32] {
        let num_pixels = self.height as usize * (self.pitch as usize / 4);
        unsafe { std::slice::from_raw_parts_mut(self.mmap_ptr, num_pixels) }
    }

    /// True when the last [`Self::render_interactive_ui`] actually painted.
    /// `flush` is a no-op otherwise, so an idle shell issues no ioctl at all.
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.frame_dirty {
            return Ok(());
        }
        self.frame_dirty = false;
        let mut dirty = DrmModeFbDirtyCmd {
            fb_id: self.fb_id,
            flags: 0,
            color: 0,
            num_clips: 0,
            clips_ptr: 0,
        };
        if unsafe { libc::ioctl(self.file.as_raw_fd(), DRM_IOCTL_MODE_DIRTYFB, &mut dirty) } < 0 {
            let err = io::Error::last_os_error();
            self.frame_cache_valid = false;
            return Err(err);
        }
        Ok(())
    }

    /// Drop the cached frame hash so the next render repaints unconditionally.
    /// Call on any mode/display change or host-side resource reset.
    pub fn invalidate_frame_cache(&mut self) {
        self.frame_cache_valid = false;
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
    /// Decoded icon blitted over the coloured tile; `None` falls back to `glyph`.
    pub icon: Option<&'a RgbaImage>,
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
    /// Prompt prefix for the active line, resolved from the real session
    /// identity (see [`crate::session`]) so the caret offset stays correct.
    pub terminal_prompt: &'a str,
    pub terminal_running: bool,
    pub terminal_tabs: &'a [TerminalTabInfo<'a>],
    pub terminal_active_tab: usize,
    pub grid_apps: &'a [AppGridItem<'a>],
    pub dock_apps: &'a [AppGridItem<'a>],
    pub app_input: &'a str,
    pub app_input_focused: bool,
    pub keyboard_shift_active: bool,
    pub messages_list: &'a [String],
    pub app_drawer_open: bool,
    pub drawer_apps: &'a [AppGridItem<'a>],
    pub drawer_search: &'a str,
    pub home_page: usize,
    pub total_home_pages: usize,
    pub selected_icon_id: Option<&'a str>,
    // Modern Lawnchair 17 / Pixel Launcher Animations
    pub drawer_progress: f32,
    pub home_scroll_offset: f32,
    pub app_launch_progress: f32,
    pub app_launch_origin: Option<(f32, f32)>,
    pub app_launch_color: u32,
    pub touch_ripple: Option<(f32, f32, f32, f32)>,
    pub pressed_icon_id: Option<&'a str>,
    pub icon_press_scale: f32,
    pub palette: MaterialYouPalette,
    pub power_saver_mode: crate::compositor::power_sync::PowerSaverMode,
    pub super_extreme_state: Option<&'a crate::compositor::super_extreme::SuperExtremeState>,
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
            terminal_prompt: crate::session::session().prompt(),
            terminal_running: false,
            terminal_tabs: &[],
            terminal_active_tab: 0,
            grid_apps: &[],
            dock_apps: &[],
            app_input: "",
            app_input_focused: false,
            keyboard_shift_active: false,
            messages_list: &[],
            app_drawer_open: false,
            drawer_apps: &[],
            drawer_search: "",
            home_page: 0,
            total_home_pages: 1,
            selected_icon_id: None,
            drawer_progress: 0.0,
            home_scroll_offset: 0.0,
            app_launch_progress: 0.0,
            app_launch_origin: None,
            app_launch_color: 0xFF2563EB,
            touch_ripple: None,
            pressed_icon_id: None,
            icon_press_scale: 1.0,
            palette: MaterialYouPalette::default_dark(),
            power_saver_mode: crate::compositor::power_sync::PowerSaverMode::Off,
            super_extreme_state: None,
        }
    }
}

impl DrmKmsDevice {
    /// Backward-compatible wrapper for static rendering
    pub fn render_mobile_ui(&mut self, time_str: &str, is_locked: bool) -> bool {
        let state = DrmInteractiveState {
            time_str,
            is_locked,
            ..Default::default()
        };
        self.render_interactive_ui(&state)
    }

    /// Render the Universal Treble Mobile Launcher, SystemUI, or active App with live interactivity.
    /// Skips the full software redraw when the visible state hash is unchanged
    /// (damage-tracking fast path: saves ~2.6M px/frame on idle screens).
    /// Returns true when a repaint actually happened.
    pub fn render_interactive_ui(&mut self, state: &DrmInteractiveState) -> bool {
        let hash = interactive_state_hash(state);
        if self.frame_cache_valid && hash == self.frame_cache_hash {
            return false;
        }
        self.frame_cache_hash = hash;
        self.frame_cache_valid = true;

        let w = self.width as usize;
        let h = self.height as usize;
        let stride = (self.pitch / 4) as usize;
        let buf = self.buffer_mut();
        paint_frame(buf, stride, w, h, state);
        self.frame_dirty = true;
        true
    }
}

/// Compose a Super Extreme TTY Recovery frame into an ARGB8888 buffer.
/// Displays pure terminal recovery UI with black background, home-made & ASCII font,
/// front camera live ASCII video viewfinder, volume HUD, and on-screen TTY keyboard.
#[allow(clippy::too_many_lines)]
pub fn paint_super_extreme_frame(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    // 1. Pure black background
    for y in 0..h {
        let row = &mut buf[y * stride..y * stride + w];
        row.fill(0xFF000000);
    }

    let prev_family = super::font::active_family();
    super::font::set_active_family(super::font::FontFamily::Homemade);

    let wf = w as f32;
    let hf = h as f32;
    let cx = (wf * 0.5) as usize;
    let em1 = super::font::em_px_at(1, w);

    let Some(sex) = state.super_extreme_state else {
        draw_text_centered(buf, stride, w, h, cx, (hf * 0.5) as usize, "SUPER EXTREME TTY RECOVERY", 0xFF22C55E, 2);
        super::font::set_active_family(prev_family);
        return;
    };

    // Top Volume HUD Bar if active
    if sex.volume_hud.is_visible() {
        let bar = sex.volume_hud.format_bar(34);
        draw_rounded_rect_f(buf, stride, w, h, wf * 0.05, 8.0, wf * 0.90, em1 * 2.2, 4.0, 0xFF0F172A);
        draw_text_centered(buf, stride, w, h, cx, (8.0 + em1 * 0.4) as usize, &bar, 0xFF38BDF8, 1);
    }

    match sex.active_screen {
        crate::compositor::super_extreme::SuperExtremeScreen::Lock => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "* ANDROID RECOVERY *", 0xFFEF4444, 2);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.13) as usize, "SUPER EXTREME POWER SAVER", 0xFF94A3B8, 1);

            draw_text_centered(buf, stride, w, h, cx, (hf * 0.28) as usize, state.time_str, 0xFFFFFFFF, 4);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.38) as usize, "BATTERY: 8% [CRITICAL] | TTY MODE", 0xFFF59E0B, 1);

            // Option 1: Emergency Call
            let btn_w = wf * 0.42;
            let btn_h = hf * 0.08;
            let btn_y = hf * 0.81;

            draw_rounded_rect_f(buf, stride, w, h, wf * 0.06, btn_y, btn_w, btn_h, 6.0, 0xFF1E293B);
            draw_text_centered(buf, stride, w, h, (wf * 0.06 + btn_w * 0.5) as usize, (btn_y + btn_h * 0.32) as usize, "[ EMERGENCY ]", 0xFFEF4444, 1);

            // Option 2: Snap Photo (Front Camera)
            draw_rounded_rect_f(buf, stride, w, h, wf * 0.52, btn_y, btn_w, btn_h, 6.0, 0xFF1E293B);
            draw_text_centered(buf, stride, w, h, (wf * 0.52 + btn_w * 0.5) as usize, (btn_y + btn_h * 0.32) as usize, "[ SNAP PHOTO ]", 0xFF22C55E, 1);

            // Unlock prompt
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.93) as usize, "^ SWIPE UP TO UNLOCK ^", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::Password => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.10) as usize, "ENTER DEVICE PASSWORD / PIN", 0xFFE2E8F0, 2);

            let box_w = wf * 0.80;
            let box_h = hf * 0.07;
            draw_rounded_rect_f(buf, stride, w, h, wf * 0.10, hf * 0.20, box_w, box_h, 6.0, 0xFF0F172A);

            let masked = if sex.password_input.is_empty() {
                "[ ______ ]".to_string()
            } else {
                format!("[ {} ]", "* ".repeat(sex.password_input.len()))
            };
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.22) as usize, &masked, 0xFF22C55E, 2);

            if sex.password_error {
                draw_text_centered(buf, stride, w, h, cx, (hf * 0.30) as usize, "INCORRECT PIN - PLEASE TRY AGAIN", 0xFFEF4444, 1);
            }

            paint_tty_keyboard(buf, stride, w, h, wf, hf, em1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::CameraPreview => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.06) as usize, "FRONT CAMERA ASCII PREVIEW", 0xFF22C55E, 2);

            super::font::set_active_family(super::font::FontFamily::AsciiMono);
            let frame = &sex.camera_preview.current_frame;
            let grid_start_y = hf * 0.12;
            let cell_h = (hf * 0.65) / frame.rows as f32;
            let start_x = wf * 0.04;

            for r in 0..frame.rows {
                let row_str = match std::str::from_utf8(frame.row(r)) {
                    Ok(s) => s,
                    Err(_) => "",
                };
                let ry = grid_start_y + (r as f32 * cell_h);
                super::font::draw_run(buf, stride, w, h, start_x, ry, row_str, 0xFF22C55E, cell_h * 0.9, super::font::FontWeight::Regular);
            }
            super::font::set_active_family(super::font::FontFamily::Homemade);

            let snap_w = wf * 0.50;
            let snap_h = hf * 0.07;
            draw_rounded_rect_f(buf, stride, w, h, wf * 0.25, hf * 0.85, snap_w, snap_h, 6.0, 0xFF15803D);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.87) as usize, "[ O  SNAP PHOTO ]", 0xFFFFFFFF, 1);

            draw_text_centered(buf, stride, w, h, cx, (hf * 0.95) as usize, "[ < BACK TO LOCKSCREEN ]", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::EmergencyDialer => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "EMERGENCY CALLING (911/112/999)", 0xFFEF4444, 2);
            let disp = if sex.emergency_input.is_empty() {
                "[ DIAL NUMBER ]"
            } else {
                &sex.emergency_input
            };
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.18) as usize, disp, 0xFFFFFFFF, 2);

            paint_numeric_keypad(buf, stride, w, h, wf, hf, em1, "CALL");
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.94) as usize, "[ < CANCEL / BACK ]", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::Home => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "=== ANDROID RECOVERY HOME ===", 0xFF22C55E, 2);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.14) as usize, "SUPER EXTREME POWER SAVER", 0xFF94A3B8, 1);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.18) as usize, &format!("{} | BATTERY: 8% | CPU: 400MHz", state.time_str), 0xFFF59E0B, 1);

            let apps = [
                ("[1] > ALARM CLOCK", 0xFF38BDF8),
                ("[2] > PHONE CALL", 0xFF22C55E),
                ("[3] > SMS MESSAGES", 0xFFA855F7),
                ("[4] > SYSTEM SETTINGS", 0xFFE2E8F0),
            ];

            let start_y = hf * 0.35;
            let row_h = hf * 0.09;
            for (i, (label, color)) in apps.iter().enumerate() {
                let y0 = start_y + (i as f32 * row_h);
                let btn_w = wf * 0.84;
                let btn_h = row_h * 0.80;
                draw_rounded_rect_f(buf, stride, w, h, wf * 0.08, y0, btn_w, btn_h, 6.0, 0xFF1E293B);
                draw_text_centered(buf, stride, w, h, cx, (y0 + btn_h * 0.30) as usize, label, *color, 1);
            }

            draw_rounded_rect_f(buf, stride, w, h, wf * 0.10, hf * 0.88, wf * 0.80, hf * 0.06, 6.0, 0xFF334155);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.895) as usize, "[ HOLD POWER: RECOVERY MENU ]", 0xFFF8FAFC, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppAlarm => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "ALARM CLOCK (TTY)", 0xFF38BDF8, 2);
            let start_y = hf * 0.25;
            let row_h = hf * 0.12;
            for (i, a) in sex.alarms.iter().enumerate() {
                let y0 = start_y + (i as f32 * row_h);
                draw_rounded_rect_f(buf, stride, w, h, wf * 0.08, y0, wf * 0.84, row_h * 0.80, 6.0, 0xFF1E293B);
                let status_str = if a.enabled { "[ ON ]" } else { "[ OFF ]" };
                let color = if a.enabled { 0xFF22C55E } else { 0xFF64748B };
                let line = format!("{} {} {}", a.time_str, a.label, status_str);
                draw_text_centered(buf, stride, w, h, cx, (y0 + row_h * 0.28) as usize, &line, color, 1);
            }
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.90) as usize, "[ < BACK TO HOME ]", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppPhone => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "PHONE CALL (TTY)", 0xFF22C55E, 2);
            let disp = if sex.phone_input.is_empty() {
                "[ ENTER NUMBER ]"
            } else {
                &sex.phone_input
            };
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.18) as usize, disp, 0xFFFFFFFF, 2);
            paint_numeric_keypad(buf, stride, w, h, wf, hf, em1, "CALL");
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.94) as usize, "[ < BACK TO HOME ]", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppSms => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "SMS MESSAGES (TTY)", 0xFFA855F7, 2);
            let start_y = hf * 0.25;
            let row_h = hf * 0.12;
            for (i, msg) in sex.sms_messages.iter().enumerate() {
                let y0 = start_y + (i as f32 * row_h);
                draw_rounded_rect_f(buf, stride, w, h, wf * 0.06, y0, wf * 0.88, row_h * 0.82, 6.0, 0xFF1E293B);
                let sender_line = format!("FROM: {} ({})", msg.sender, msg.time);
                draw_text(buf, stride, w, h, (wf * 0.10) as usize, (y0 + row_h * 0.16) as usize, &sender_line, 0xFFF8FAFC, 1);
                draw_text(buf, stride, w, h, (wf * 0.10) as usize, (y0 + row_h * 0.44) as usize, msg.snippet, 0xFF94A3B8, 1);
            }
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.90) as usize, "[ < BACK TO HOME ]", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppSettings => {
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.08) as usize, "SYSTEM SETTINGS (TTY)", 0xFFE2E8F0, 2);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.20) as usize, "Power Saver: SUPER EXTREME", 0xFFF59E0B, 1);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.26) as usize, "Display Brightness: 10% (Fixed)", 0xFF94A3B8, 1);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.32) as usize, "Active Font: Home-Made + ASCII Mono", 0xFF94A3B8, 1);

            draw_rounded_rect_f(buf, stride, w, h, wf * 0.10, hf * 0.50, wf * 0.80, hf * 0.09, 6.0, 0xFF2563EB);
            draw_text_centered(buf, stride, w, h, cx, (hf * 0.535) as usize, "[ RETURN TO NORMAL MODE ]", 0xFFFFFFFF, 1);

            draw_text_centered(buf, stride, w, h, cx, (hf * 0.90) as usize, "[ < BACK TO HOME ]", 0xFF94A3B8, 1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::PowerMenu => {
            let menu_w = wf * 0.84;
            let menu_h = hf * 0.58;
            let menu_x = wf * 0.08;
            let menu_y = hf * 0.22;

            draw_rounded_rect_f(buf, stride, w, h, menu_x, menu_y, menu_w, menu_h, 8.0, 0xFF0F172A);
            draw_text_centered(buf, stride, w, h, cx, (menu_y + hf * 0.04) as usize, "RECOVERY POWER MENU", 0xFFEF4444, 2);

            let options = [
                ("[1] Turn back to Normal Mode", 0xFF38BDF8),
                ("[2] Reboot System", 0xFFF8FAFC),
                ("[3] Reboot to Bootloader", 0xFF94A3B8),
                ("[4] Reboot to Recovery", 0xFF94A3B8),
                ("[5] Power Off System", 0xFFEF4444),
                ("[6] Cancel", 0xFF64748B),
            ];

            let row_h = hf * 0.07;
            let start_y = menu_y + hf * 0.09;
            for (i, (label, col)) in options.iter().enumerate() {
                let y0 = start_y + (i as f32 * row_h);
                draw_rounded_rect_f(buf, stride, w, h, menu_x + wf * 0.04, y0, menu_w - wf * 0.08, row_h * 0.80, 4.0, 0xFF1E293B);
                draw_text(buf, stride, w, h, (menu_x + wf * 0.08) as usize, (y0 + row_h * 0.28) as usize, label, *col, 1);
            }
        }
    }

    if let Some(ref msg) = sex.last_action_message {
        draw_text_centered(buf, stride, w, h, cx, (hf * 0.97) as usize, msg, 0xFF22C55E, 1);
    }

    super::font::set_active_family(prev_family);
}

fn paint_tty_keyboard(buf: &mut [u32], stride: usize, w: usize, h: usize, wf: f32, hf: f32, _em1: f32) {
    let kb_top = hf * 0.60;
    let kb_h = hf * 0.38;
    let row_h = kb_h / 4.0;

    let digits = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
    let col_w10 = wf / 10.0;
    for (i, d) in digits.iter().enumerate() {
        let x0 = i as f32 * col_w10;
        let mut b = [0u8; 4];
        let s = d.encode_utf8(&mut b);
        draw_rounded_rect_f(buf, stride, w, h, x0 + 2.0, kb_top + 2.0, col_w10 - 4.0, row_h - 4.0, 4.0, 0xFF1E293B);
        draw_text_centered(buf, stride, w, h, (x0 + col_w10 * 0.5) as usize, (kb_top + row_h * 0.3) as usize, s, 0xFFF8FAFC, 1);
    }

    let chars1 = ['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P'];
    let y1 = kb_top + row_h;
    for (i, c) in chars1.iter().enumerate() {
        let x0 = i as f32 * col_w10;
        let mut b = [0u8; 4];
        let s = c.encode_utf8(&mut b);
        draw_rounded_rect_f(buf, stride, w, h, x0 + 2.0, y1 + 2.0, col_w10 - 4.0, row_h - 4.0, 4.0, 0xFF1E293B);
        draw_text_centered(buf, stride, w, h, (x0 + col_w10 * 0.5) as usize, (y1 + row_h * 0.3) as usize, s, 0xFFF8FAFC, 1);
    }

    let chars2 = ['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L'];
    let y2 = kb_top + row_h * 2.0;
    let pad = wf * 0.05;
    let col_w9 = (wf * 0.90) / 9.0;
    for (i, c) in chars2.iter().enumerate() {
        let x0 = pad + (i as f32 * col_w9);
        let mut b = [0u8; 4];
        let s = c.encode_utf8(&mut b);
        draw_rounded_rect_f(buf, stride, w, h, x0 + 2.0, y2 + 2.0, col_w9 - 4.0, row_h - 4.0, 4.0, 0xFF1E293B);
        draw_text_centered(buf, stride, w, h, (x0 + col_w9 * 0.5) as usize, (y2 + row_h * 0.3) as usize, s, 0xFFF8FAFC, 1);
    }

    let y3 = kb_top + row_h * 3.0;
    let labels = ["ESC", "Z", "X", "C", "V", "B", "N", "M", "<", "OK"];
    for (i, l) in labels.iter().enumerate() {
        let x0 = i as f32 * col_w10;
        let bg = if i == 0 { 0xFF475569 } else if i == 8 { 0xFF991B1B } else if i == 9 { 0xFF166534 } else { 0xFF1E293B };
        draw_rounded_rect_f(buf, stride, w, h, x0 + 2.0, y3 + 2.0, col_w10 - 4.0, row_h - 4.0, 4.0, bg);
        draw_text_centered(buf, stride, w, h, (x0 + col_w10 * 0.5) as usize, (y3 + row_h * 0.3) as usize, l, 0xFFF8FAFC, 1);
    }
}

fn paint_numeric_keypad(buf: &mut [u32], stride: usize, w: usize, h: usize, wf: f32, hf: f32, _em1: f32, enter_label: &str) {
    let pad_top = hf * 0.45;
    let pad_h = hf * 0.42;
    let row_h = pad_h / 4.0;
    let pad_w = wf * 0.80;
    let pad_x = wf * 0.10;
    let col_w = pad_w / 3.0;

    let keys = [
        ["1", "2", "3"],
        ["4", "5", "6"],
        ["7", "8", "9"],
        ["<", "0", enter_label],
    ];

    for (r, row) in keys.iter().enumerate() {
        let y0 = pad_top + (r as f32 * row_h);
        for (c, key_str) in row.iter().enumerate() {
            let x0 = pad_x + (c as f32 * col_w);
            let bg = if *key_str == enter_label { 0xFF166534 } else if *key_str == "<" { 0xFF991B1B } else { 0xFF1E293B };
            draw_rounded_rect_f(buf, stride, w, h, x0 + 4.0, y0 + 4.0, col_w - 8.0, row_h - 8.0, 6.0, bg);
            draw_text_centered(buf, stride, w, h, (x0 + col_w * 0.5) as usize, (y0 + row_h * 0.35) as usize, key_str, 0xFFFFFFFF, 2);
        }
    }
}

/// Compose one full shell frame into an ARGB8888 buffer.
///
/// Kept free of DRM state so the exact production draw path can be replayed
/// offscreen (see `super::screenshot`) and diffed in tests.
#[allow(clippy::too_many_lines)]
pub fn paint_frame(buf: &mut [u32], stride: usize, w: usize, h: usize, state: &DrmInteractiveState) {
    if w == 0 || h == 0 {
        return;
    }

    if state.power_saver_mode == crate::compositor::power_sync::PowerSaverMode::SuperExtreme {
        paint_super_extreme_frame(buf, stride, w, h, state);
        return;
    }

    // 1. Palette-driven background, then the system status bar on top of it.
    draw_background(buf, stride, w, h, state);
    draw_status_bar(buf, stride, w, h, state, 0);

    if state.is_locked {
        // Lock screen: a large Material You clock, a padlock above it, the
        // date under it, and the unlock affordance pinned to the bottom. Every
        // dimension is a fraction of the panel so it scales with the display.
        let wf = w as f32;
        let hf = h as f32;
        let cx = (wf * 0.5) as usize;
        // Clock occupies the upper third, with room for lock and date.
        let cap = (hf * 0.085).max(wf * 0.09);
        let clock_cy = (hf * 0.30) as usize;
        let lock_size = cap * 0.34;
        draw_lock_glyph(
            buf,
            stride,
            w,
            h,
            cx,
            (clock_cy as f32 - cap * 0.5 - lock_size * 1.9) as usize,
            lock_size,
            state.palette.on_surface_variant,
        );
        draw_material_you_clock(
            buf, stride, w, h, cx, clock_cy, state.time_str, state.palette.primary, cap as usize,
        );
        draw_text_centered_clipped(
            buf,
            stride,
            w,
            h,
            cx,
            (clock_cy as f32 + cap * 0.5 + hf * 0.016) as usize,
            wf * 0.9,
            "Tuesday, Sep 22",
            state.palette.on_surface_variant,
            2,
            FontWeight::Medium,
        );
        // Bottom affordance.
        let hint_y = h - (hf * 0.075) as usize;
        let em1 = super::font::em_px_at(1, w);
        let pill_h = (em1 * 2.2).max(hf * 0.020);
        let pill_w = (wf * 0.46).max(super::font::measure("Click or swipe up to unlock", em1) + em1 * 2.0);
        draw_rounded_rect_f(
            buf, stride, w, h, wf * 0.5 - pill_w * 0.5, hint_y as f32 - pill_h * 0.5,
            pill_w, pill_h, pill_h * 0.5, state.palette.surface_container,
        );
        draw_text_centered(
            buf, stride, w, h, cx, (hint_y as f32 - em1 * 0.31) as usize,
            "Click or swipe up to unlock", state.palette.on_surface_variant, 1,
        );
    } else if state.shade_open {
        // 3. Quick settings and notifications.
        //
        // The shade is a scrim over the workspace plus a `ShadeLayout`-driven
        // stack: header clock, tile grid, brightness slider and notification
        // cards. Those are the same rects the input path hit-tests, so a tile
        // is tappable exactly where it is drawn.
        let sl = ShadeLayout::new(w as f32, h as f32);
        let wf = w as f32;
        let hf = h as f32;
        // The shade occupies the full height below `sl.top`, so this is a
        // PANEL, not a scrim: on Android the quick-settings panel is opaque
        // and only the area *outside* it is scrimmed. Painting it with a
        // translucent colour (as this used to, 0xEE) cost a per-pixel alpha
        // blend over all 2.59 Mpx - measured at 10.2 ms of a 15.0 ms frame
        // on the x86 host, for a 7% difference nobody can see under an
        // opaque panel. Filling opaquely takes the vectorised `draw_rect`
        // path instead, and matches the platform.
        draw_rect_f(
            buf, stride, w, h, 0.0, sl.top, wf, hf - sl.top,
            state.palette.surface_container,
        );

        // Header: display clock on the left, date and build on the right.
        let em1 = super::font::em_px_at(1, w);
        let pad = wf * PANEL_PAD_FRACTION;
        let clock_w = clock_run_width(state.time_str, sl.clock_size, w);
        draw_material_you_clock(
            buf, stride, w, h, (pad + clock_w * 0.5) as usize, (sl.date_y - em1 * 0.9) as usize,
            state.time_str, state.palette.primary, sl.clock_size as usize,
        );
        let date_x = pad + clock_w + em1;
        draw_text_clipped(
            buf, stride, w, h, date_x, sl.date_y, wf - date_x - pad,
            "Tue, Sep 22  |  Universal Treble GSI", state.palette.on_surface_variant, 1,
            FontWeight::Medium,
        );

        // Tile grid.
        let tile_names = [
            "Wi-Fi", "Mobile Data", "Bluetooth", "Flashlight",
            "Auto-rotate", "Airplane mode", "Battery Saver", "Hotspot",
        ];
        let label_pad = sl.tiles.tile.1 * 0.22;
        let tile_em = super::font::em_px_at(1, w);
        for (idx, name) in tile_names.iter().enumerate() {
            let c = sl.tiles.cell(idx);
            let on = state.quick_tiles_active[idx];
            let bg = if on {
                state.palette.primary
            } else {
                state.palette.surface_container_high
            };
            let fg = if on {
                state.palette.on_primary
            } else {
                state.palette.on_surface_variant
            };
            draw_rounded_rect_f(
                buf, stride, w, h, c.x, c.y, c.w, c.h, c.radius, bg,
            );
            draw_text_clipped(
                buf, stride, w, h, c.x + label_pad, c.y + label_pad, c.w - label_pad * 2.0,
                name, fg, 1, FontWeight::Bold,
            );
            // Active dot instead of an ON/OFF word: less noise, same meaning.
            if on {
                draw_circle_glyph(
                    buf, stride, w, h, c.x + c.w - label_pad, c.y + c.h - label_pad,
                    tile_em * 0.22, fg,
                );
            }
        }

        // Brightness slider: track plus a filled level.
        let br = sl.brightness;
        draw_rounded_rect_f(
            buf, stride, w, h, br.x, br.y, br.w, br.h, br.radius,
            state.palette.surface_container_high,
        );
        let fill = br.w * 0.78;
        draw_rounded_rect_f(
            buf, stride, w, h, br.x, br.y, fill, br.h, br.radius, state.palette.primary,
        );
        // Thumb.
        draw_circle_glyph(
            buf, stride, w, h, br.x + fill, br.center_y(), br.h * 0.26,
            state.palette.on_primary,
        );
        draw_text_clipped(
            buf, stride, w, h, br.x + br.h * 0.6, br.center_y() - tile_em * 0.31,
            br.w - br.h * 1.2, "Brightness  78%", state.palette.on_primary, 1, FontWeight::Medium,
        );

        // Notifications.
        draw_text_weighted(
            buf, stride, w, h, sl.tiles.origin.0 as usize, sl.notif_title_y as usize,
            "NOTIFICATIONS", state.palette.on_surface_variant, 1, FontWeight::Bold,
        );
        let cards = [
            ("UTIM PID 1 & UTLC Wayland", "Interactive compositor, < 8ms input response"),
            ("Direct DRM KMS Scanout", "Native scanout via /dev/dri/card0"),
        ];
        for (i, (title, body)) in cards.iter().enumerate() {
            let c = sl.notifs[i];
            draw_rounded_rect_f(
                buf, stride, w, h, c.x, c.y, c.w, c.h, c.radius,
                state.palette.surface_container_high,
            );
            let tx = c.x + label_pad;
            let tw = c.w - label_pad * 2.0;
            draw_text_clipped(
                buf, stride, w, h, tx, c.y + c.h * 0.20, tw, title,
                state.palette.on_surface, 1, FontWeight::Bold,
            );
            draw_text_clipped(
                buf, stride, w, h, tx, c.y + c.h * 0.20 + tile_em * 1.1, tw, body,
                state.palette.on_surface_variant, 1, FontWeight::Regular,
            );
        }

        // Pull handle to dismiss.
        let hd = sl.handle;
        draw_rounded_rect_f(
            buf, stride, w, h, hd.x, hd.y, hd.w, hd.h, hd.radius, state.palette.outline,
        );
    } else if let Some(app_name) = state.active_app {
        // 4. Active Application Window View
        //
        // The top bar, its buttons and the content card all come from
        // `AppLayout`, which is the same value the input path uses, so the
        // back/close affordances cannot drift from their hit areas.
        let panel = match app_name {
            "Browser" => AppPanel::Browser,
            "Terminal" => AppPanel::Terminal,
            "Messages" => AppPanel::Messages,
            "Settings" => AppPanel::Settings,
            "Phone" => AppPanel::Phone,
            _ => AppPanel::Other,
        };
        let tabs_n = state.terminal_tabs.len().max(1);
        let al = AppLayout::new(w as f32, h as f32, panel, tabs_n);
        let bar = al.bar;

        draw_rounded_rect(
            buf, stride, w, h,
            bar.x as usize, bar.y as usize, bar.w as usize, bar.h as usize,
            bar.radius as usize, state.palette.surface_container,
        );
        // Back button.
        let em2 = super::font::em_px_at(2, w);
        let btn_text_y = al.back.center_y() - em2 * 0.30;
        draw_rounded_rect(
            buf, stride, w, h,
            al.back.x as usize, al.back.y as usize, al.back.w as usize, al.back.h as usize,
            (al.back.h * 0.25) as usize, state.palette.surface_container_high,
        );
        draw_text(buf, stride, w, h, (al.back.x + al.back.h * 0.35) as usize, btn_text_y as usize, "<", state.palette.on_surface, 2);
        draw_text(
            buf, stride, w, h, (al.back.x + al.back.h * 0.95) as usize, btn_text_y as usize,
            "Back", state.palette.on_surface, 2,
        );

        // App title, clipped so a long name cannot run into a button.
        let title_x = al.back.x + al.back.w;
        let title_w = (al.close.x - title_x - al.back.h * 0.3).max(0.0);
        draw_text_centered_clipped(
            buf, stride, w, h,
            (title_x + title_w * 0.5) as usize,
            (bar.center_y() - em2 * 0.32) as usize,
            title_w,
            app_name, state.palette.on_surface, 2, FontWeight::Bold,
        );

        // Close button.
        draw_rounded_rect(
            buf, stride, w, h,
            al.close.x as usize, al.close.y as usize, al.close.w as usize, al.close.h as usize,
            (al.close.h * 0.25) as usize, 0xFFEF4444,
        );
        let cx_c = al.close.center_x();
        let cy_c = al.close.center_y();
        let r = al.close.h * 0.17;
        draw_line(buf, stride, w, h, cx_c - r, cy_c - r, cx_c + r, cy_c + r, 0xFFFFFFFF);
        draw_line(buf, stride, w, h, cx_c + r, cy_c - r, cx_c - r, cy_c + r, 0xFFFFFFFF);

        // App content container. The keyboard shortens it, so the drawn card
        // and the scrolled content band move together.
        let kb_h = if state.keyboard_active {
            Keyboard::new(w as f32, h as f32).frame.h
        } else {
            0.0
        };
        let content_y = al.scroll_top;
        let content_h = (h as f32 - kb_h - h as f32 * 0.030 - content_y).max(0.0);
        draw_rounded_rect(
            buf, stride, w, h,
            bar.x as usize, content_y as usize, bar.w as usize, content_h as usize,
            (bar.h * 0.32) as usize, state.palette.outline_variant,
        );

        if app_name == "Terminal" {
            // Interactive Linux shell with a tabbed multi-terminal bar. The
            // strip geometry is `AppLayout::tab_rect` / `add_tab_rect`, which
            // the input path hit-tests, so tabs cannot overlap or drift.
            let default_single_tab = [TerminalTabInfo {
                id: 1,
                title: "Tab 1: bash",
                is_running: state.terminal_running,
                is_active: true,
            }];
            let tabs = if state.terminal_tabs.is_empty() {
                &default_single_tab[..]
            } else {
                state.terminal_tabs
            };
            let tab_al = AppLayout::new(w as f32, h as f32, AppPanel::Terminal, tabs.len());
            let t_em = super::font::em_px_at(1, w);

            for (i, tab) in tabs.iter().enumerate().take(4) {
                let r = tab_al.tab_rect(i);
                let is_active = tab.is_active || (tabs.len() == 1 && i == 0);
                let bg = if is_active {
                    state.palette.primary
                } else {
                    state.palette.surface_container_high
                };
                let fg = if is_active { state.palette.on_primary } else { state.palette.on_surface_variant };
                draw_rounded_rect(
                    buf, stride, w, h,
                    r.x as usize, r.y as usize, r.w as usize, r.h as usize, r.radius as usize, bg,
                );

                // Running indicator, then a title clipped to what is left.
                let dot_r = r.h * 0.13;
                let mut tx = r.x + r.h * 0.28;
                if tab.is_running {
                    draw_rounded_rect(
                        buf, stride, w, h,
                        tx as usize, (r.center_y() - dot_r) as usize,
                        (dot_r * 2.0) as usize, (dot_r * 2.0) as usize, dot_r as usize,
                        0xFF10B981,
                    );
                    tx += dot_r * 3.4;
                }
                let close = tab_al.tab_close_zone(i);
                let title_w = (close.x - tx - r.h * 0.12).max(0.0);
                draw_text_clipped(
                    buf, stride, w, h, tx, r.center_y() - t_em * 0.30,
                    title_w, tab.title, fg, 1, FontWeight::Medium,
                );

                // Close affordance only on the active tab, and only when
                // there is more than one tab to close.
                if is_active && tabs.len() > 1 {
                    let cxr = close.center_x();
                    let cyr = r.center_y();
                    let cr = r.h * 0.13;
                    draw_line(buf, stride, w, h, cxr - cr, cyr - cr, cxr + cr, cyr + cr, fg);
                    draw_line(buf, stride, w, h, cxr + cr, cyr - cr, cxr - cr, cyr + cr, fg);
                }
            }

            // Add-tab button, exactly where `add_tab_rect` says it is.
            if let Some(add) = tab_al.add_tab_rect() {
                draw_rounded_rect(
                    buf, stride, w, h,
                    add.x as usize, add.y as usize, add.w as usize, add.h as usize, add.radius as usize,
                    state.palette.surface_container_high,
                );
                let ar = add.h * 0.22;
                let acx = add.center_x();
                let acy = add.center_y();
                draw_line(buf, stride, w, h, acx - ar, acy, acx + ar, acy, state.palette.primary);
                draw_line(buf, stride, w, h, acx, acy - ar, acx, acy + ar, state.palette.primary);
            }

            // Divider line below the tab strip, then the shell banner.
            let strip_bottom = tab_al.tabs.y + tab_al.tabs.h;
            draw_rect(
                buf, stride, w, h,
                (bar.x + bar.h * 0.25) as usize, (strip_bottom + h as f32 * 0.004) as usize,
                (bar.w - bar.h * 0.5) as usize, (h as f32 * 0.0015).max(1.0) as usize,
                state.palette.outline_variant,
            );

            let mut line_y = strip_bottom + h as f32 * 0.010;
            draw_text(buf, stride, w, h, 36, line_y as usize, "Universal Treble Linux 1.0 (Debian Sid ARM64)", 0xFF38BDF8, 3);
            line_y += h as f32 * 0.016;
            draw_text(buf, stride, w, h, 36, line_y as usize, "Linux 6.1.23-android14-4-00257 (Android GKI)", 0xFF94A3B8, 2);
            line_y += h as f32 * 0.011;
            draw_text(buf, stride, w, h, 36, line_y as usize, "UTIM PID 1 init | UTLC Wayland Compositor", 0xFF94A3B8, 2);
            line_y += h as f32 * 0.011;
            draw_text(buf, stride, w, h, 36, line_y as usize, "Debian Sid ARM64 GNU/Linux - Multi-Tab Terminal Active", 0xFF64748B, 2);
            line_y += h as f32 * 0.016;

            // Terminal body: scale 3 with a 1.18 line box.
            let term_em = super::font::em_px_at(3, w);
            let line_h = (term_em * 1.18).round();
            let header_used = line_y - content_y;
            let available_h = (content_h - header_used - h as f32 * 0.020).max(0.0);
            let max_lines = ((available_h / line_h) as usize).saturating_sub(1);

            // Auto-scroll viewport: show the most recent lines so the active prompt is always visible
            let visible_lines = if state.terminal_lines.len() > max_lines {
                &state.terminal_lines[state.terminal_lines.len() - max_lines..]
            } else {
                state.terminal_lines
            };

            for line in visible_lines {
                if line_y + line_h <= content_y + content_h - h as f32 * 0.014 {
                    draw_text_clipped(
                        buf, stride, w, h, 36.0, line_y, bar.w - 36.0 * 2.0, line, 0xFFE2E8F0, 3,
                        FontWeight::Regular,
                    );
                    line_y += line_h;
                }
            }

            // Active prompt line with typed characters and blinking cursor (Scale 3)
            if line_y + line_h <= content_y + content_h {
                if state.terminal_running {
                    draw_text(buf, stride, w, h, 36, line_y as usize, "[running... (Ctrl+C to stop)]", 0xFFF59E0B, 3);
                } else {
                    // The prompt describes the identity the commands run as
                    // (root is dropped to the unprivileged session account).
                    let prompt = state.terminal_prompt;
                    let prompt_w = super::font::measure(prompt, term_em);
                    let baseline = line_y + term_em * 0.30;
                    draw_text(buf, stride, w, h, 36, baseline as usize, prompt, 0xFF10B981, 3);
                    // The typed line is clipped to the content card, and the
                    // caret tracks the real measured advance.
                    let input_x = 36.0 + prompt_w;
                    let avail = (bar.x + bar.w - 36.0 - input_x).max(0.0);
                    draw_text_clipped(
                        buf, stride, w, h, input_x, baseline, avail, state.terminal_input,
                        0xFFFFFFFF, 3, FontWeight::Regular,
                    );
                    let caret_x = input_x + super::font::measure(state.terminal_input, term_em);
                    if caret_x < 36.0 + avail {
                        draw_rect(
                            buf, stride, w, h, caret_x as usize, baseline as usize,
                            (term_em * 0.09).round().max(2.0) as usize, (term_em * 0.72) as usize,
                            0xFF10B981,
                        );
                    }
                }
            }
        } else if app_name == "Settings" {
            // Settings: a top search field over a filtered list of system
            // section cards. The field is the same rect the input path
            // hit-tests, and every card is clipped to the content card.
            let (list, list_h) = draw_app_search_field(
                buf, stride, w, h, &bar, content_y, al, state, "Search settings...",
            );
            let cards = [
                ("Network & Internet", "Wi-Fi, Mobile, Hotspot, VPN"),
                ("Connected Devices", "Bluetooth, Android HAL bridge"),
                ("Display & Graphics", "1080x2400 @ 120Hz Direct DRM KMS"),
                ("Sound & Multimedia", "PipeWire spa-droid Audio"),
                ("Storage", "4.00 GB ext4 System GSI Image"),
                ("Battery", "98% - Mobile Power Governor active"),
                ("About Phone", "Universal Treble Linux (Android 14 GKI)"),
            ];
            let query = state.app_input;
            let mut y = list;
            let card_h = (list_h / cards.len() as f32 * 0.82).min(h as f32 * 0.030);
            let step = card_h + h as f32 * 0.006;
            let x = bar.x + bar.h * 0.25;
            let rw = bar.w - bar.h * 0.5;
            let title_em = super::font::em_px_at(1, w);
            for (title, desc) in cards {
                if !query.is_empty()
                    && !ascii_contains_ci(title, query)
                    && !ascii_contains_ci(desc, query)
                {
                    continue;
                }
                if y + card_h > list + list_h {
                    break;
                }
                draw_rounded_rect_f(
                    buf, stride, w, h, x, y, rw, card_h, card_h * 0.24,
                    state.palette.surface_container_high,
                );
                let tx = x + card_h * 0.45;
                let tw = rw - card_h * 0.9;
                draw_text_clipped(
                    buf, stride, w, h, tx, y + card_h * 0.16, tw, title,
                    state.palette.on_surface, 1, FontWeight::Bold,
                );
                draw_text_clipped(
                    buf, stride, w, h, tx, y + card_h * 0.16 + title_em, tw, desc,
                    state.palette.on_surface_variant, 1, FontWeight::Regular,
                );
                y += step;
            }
        } else if app_name == "Browser" || app_name == "Web" || app_name.contains("Browser") || app_name == "Firefox" {
            // Mobile browser. The omnibox, viewport and toolbar are all sized
            // from the content card, so nothing can be drawn outside it.
            let card_x = bar.x + bar.h * 0.25;
            let card_w = bar.w - bar.h * 0.5;
            let inset = card_w * 0.022;
            let url_x = card_x + inset;
            let url_w = card_w - inset * 2.0;
            let bar_top = content_y + h as f32 * 0.008;
            let bar_h = h as f32 * 0.030;
            let toolbar_h = h as f32 * 0.034;
            let page_y = bar_top + bar_h + h as f32 * 0.008;
            let page_h = ((content_y + content_h) - page_y - toolbar_h - h as f32 * 0.010).max(0.0);

            let ring = (w as f32 * 0.003).max(1.0);
            let url_border = if state.app_input_focused {
                state.palette.primary
            } else {
                state.palette.outline
            };
            draw_rounded_rect_f(
                buf, stride, w, h, url_x - ring, bar_top - ring, url_w + ring * 2.0,
                bar_h + ring * 2.0, bar_h * 0.5 + ring, url_border,
            );
            draw_rounded_rect_f(
                buf, stride, w, h, url_x, bar_top, url_w, bar_h, bar_h * 0.5,
                state.palette.surface_container_high,
            );

            // Lock badge, drawn as a real padlock silhouette.
            let lock_r = bar_h * 0.20;
            let lock_cx = url_x + bar_h * 0.55;
            let lock_cy = bar_top + bar_h * 0.5;
            draw_rounded_rect_f(
                buf, stride, w, h, lock_cx - lock_r * 1.15, lock_cy - lock_r * 1.7,
                lock_r * 2.3, lock_r * 2.2, lock_r * 0.6, 0xFF10B981,
            );
            draw_rounded_rect_f(
                buf, stride, w, h, lock_cx - lock_r * 0.75, lock_cy - lock_r * 0.5,
                lock_r * 1.5, lock_r * 1.1, lock_r * 0.3, state.palette.surface_container_high,
            );

            // URL, clipped to what the trailing badges leave.
            let badge_w = bar_h * 0.95;
            let trail = bar_h * 0.55;
            let b_disp = if !state.app_input.is_empty() {
                state.app_input
            } else if state.app_input_focused {
                "Search or type web address"
            } else {
                "https://www.google.com"
            };
            let b_col = if !state.app_input.is_empty() {
                state.palette.on_surface
            } else {
                state.palette.on_surface_variant
            };
            let b_em = super::font::em_px_at(2, w);
            let b_text_h = b_em * 0.62;
            let b_tx = url_x + bar_h * 1.10;
            let b_w = (url_x + url_w - trail - b_tx).max(0.0);
            let b_ty = bar_top + (bar_h - b_text_h) * 0.5 - b_em * 0.14;
            draw_text_clipped(buf, stride, w, h, b_tx, b_ty, b_w, b_disp, b_col, 2, FontWeight::Regular);
            if state.app_input_focused {
                let cur_x = b_tx + super::font::measure(b_disp, b_em) + ring;
                if cur_x < b_tx + b_w {
                    draw_rect(
                        buf, stride, w, h, cur_x as usize, b_ty as usize,
                        ring.max(2.0) as usize, b_text_h as usize, state.palette.primary,
                    );
                }
            }

            // Tab-count badge and reload glyph on the trailing edge.
            let badge_x = url_x + url_w - trail - badge_w;
            draw_rounded_rect_f(
                buf, stride, w, h, badge_x, bar_top + (bar_h - badge_w) * 0.5, badge_w, badge_w,
                badge_w * 0.28, state.palette.surface_container,
            );
            draw_text_centered(
                buf, stride, w, h, (badge_x + badge_w * 0.5) as usize,
                (bar_top + (bar_h - badge_w) * 0.5 + badge_w * 0.16) as usize,
                "1", state.palette.on_surface_variant, 1,
            );
            let rr = badge_w * 0.26;
            let rcx = url_x + url_w - trail * 0.5;
            let rcy = bar_top + bar_h * 0.5;
            // Refresh: an arc approximated by four ticks plus an arrow head.
            for i in 0..8 {
                let ang = i as f32 * std::f32::consts::TAU / 8.0;
                let px = rcx + rr * ang.cos();
                let py = rcy + rr * ang.sin();
                draw_rect_f(buf, stride, w, h, px - 1.0, py - 1.0, 2.0, 2.0, state.palette.on_surface_variant);
            }
            draw_line(buf, stride, w, h, rcx + rr * 0.7, rcy - rr * 0.7, rcx + rr * 1.15, rcy - rr * 0.7, state.palette.on_surface_variant);
            draw_line(buf, stride, w, h, rcx + rr * 1.15, rcy - rr * 0.7, rcx + rr * 1.15, rcy - rr * 0.25, state.palette.on_surface_variant);

            // Viewport.
            draw_rounded_rect_f(
                buf, stride, w, h, card_x, page_y, card_w, page_h, bar.h * 0.4,
                state.palette.outline_variant,
            );
            let body_x = card_x + inset;
            let body_w = card_w - inset * 2.0;

            if !state.app_input.is_empty() {
                // Search results view: header, then two result cards.
                let head_h = h as f32 * 0.028;
                draw_rounded_rect_f(
                    buf, stride, w, h, body_x, page_y + inset, body_w, head_h, head_h * 0.24,
                    state.palette.surface_container_high,
                );
                let head_em = super::font::em_px_at(1, w);
                let label = "Results for";
                let q_w = super::font::measure(state.app_input, head_em);
                let lbl_w = super::font::measure(label, head_em);
                let hy = page_y + inset + (head_h - head_em * 0.62) * 0.5;
                draw_text_weighted(
                    buf, stride, w, h, (body_x + inset) as usize, hy as usize,
                    label, state.palette.on_surface_variant, 1, FontWeight::Regular,
                );
                draw_text_weighted(
                    buf, stride, w, h, (body_x + inset + lbl_w + head_em * 0.2) as usize, hy as usize,
                    state.app_input, state.palette.primary, 1, FontWeight::Bold,
                );
                let _ = q_w;

                let card_top = page_y + inset + head_h + inset;
                let ch = ((page_y + page_h - card_top) * 0.5 - inset).max(0.0);
                for (i, (title, url, snippet)) in [
                    (state.app_input, "https://www.google.com/search", "Top match and official verified web destination."),
                    ("Wikipedia - Free Encyclopedia", "https://en.wikipedia.org/wiki", "Overview, history, documentation, and references."),
                ]
                .iter()
                .enumerate()
                {
                    let cy = card_top + i as f32 * (ch + inset);
                    draw_rounded_rect_f(
                        buf, stride, w, h, body_x, cy, body_w, ch, ch * 0.14,
                        state.palette.surface_container_high,
                    );
                    let t_em = super::font::em_px_at(1, w);
                    draw_text_clipped(
                        buf, stride, w, h, body_x + inset, cy + ch * 0.14, body_w - inset * 2.0,
                        title, state.palette.primary, 1, FontWeight::Bold,
                    );
                    draw_text_clipped(
                        buf, stride, w, h, body_x + inset, cy + ch * 0.14 + t_em, body_w - inset * 2.0,
                        url, state.palette.on_surface_variant, 1, FontWeight::Regular,
                    );
                    draw_text_clipped(
                        buf, stride, w, h, body_x + inset, cy + ch * 0.14 + t_em * 2.0, body_w - inset * 2.0,
                        snippet, state.palette.on_surface_variant, 1, FontWeight::Regular,
                    );
                }
            } else {
                // Start page: wordmark, in-page search, shortcut grid.
                let word_em = super::font::em_px_at(4, w);
                let word = "Google";
                let word_w = super::font::measure(word, word_em);
                let center_x = card_x + card_w * 0.5;
                let g_y = page_y + page_h * 0.10;
                // Multi-colour wordmark, one glyph at a time.
                let cols = [0xFF4285F4u32, 0xFFEA4335, 0xFFFBBC05, 0xFF4285F4, 0xFF34A853, 0xFFEA4335];
                let mut wx = center_x - word_w * 0.5;
                for (i, b) in word.bytes().enumerate() {
                    let adv = super::font::char_advance(b, word_em);
                    super::font::draw_glyph(
                        buf, stride, w, h, wx, g_y, b, cols[i % cols.len()], word_em, FontWeight::Medium,
                    );
                    wx += adv;
                }

                let s_h = h as f32 * 0.030;
                let s_w = (body_w * 0.7).min(word_w * 1.9);
                let s_x = center_x - s_w * 0.5;
                let s_y = g_y + word_em * 1.05;
                draw_rounded_rect_f(
                    buf, stride, w, h, s_x, s_y, s_w, s_h, s_h * 0.5,
                    state.palette.surface_container_high,
                );
                let s_em = super::font::em_px_at(1, w);
                let s_tx = s_x + s_h * 0.30;
                draw_text_weighted(
                    buf, stride, w, h, s_tx as usize, (s_y + (s_h - s_em * 0.62) * 0.5) as usize,
                    "Search or type web address", state.palette.on_surface_variant, 1, FontWeight::Regular,
                );
                let _ = s_em;
                let _ = s_tx;

                // Shortcut grid: three columns of tiles inside the viewport.
                let sc_start = s_y + s_h + h as f32 * 0.020;
                let shortcuts = [
                    ("Google", 0xFF4285F4u32),
                    ("YouTube", 0xFFFF0000),
                    ("Wikipedia", 0xFF475569),
                    ("Reddit", 0xFFFF4500),
                    ("GitHub", 0xFF24292F),
                    ("Weather", 0xFF0284C7),
                ];
                let sc_cols = 3;
                let sc_col_w = body_w / sc_cols as f32;
                let sc_size = (sc_col_w * 0.44).min((page_y + page_h - sc_start) * 0.30);
                for (idx, (title, color)) in shortcuts.iter().enumerate() {
                    let cc = idx % sc_cols;
                    let cr = idx / sc_cols;
                    let cx = body_x + sc_col_w * (cc as f32 + 0.5);
                    let cy = sc_start + sc_size * 0.5 + sc_size * 1.75 * cr as f32;
                    if cy + sc_size * 0.95 > page_y + page_h {
                        break;
                    }
                    draw_rounded_rect_f(
                        buf, stride, w, h, cx - sc_size * 0.5, cy - sc_size * 0.5, sc_size, sc_size,
                        sc_size * 0.5, *color,
                    );
                    let mut fb = [0u8; 4];
                    let first: &str = match title.chars().next() {
                        Some(c) => {
                            let n = c.len_utf8();
                            c.encode_utf8(&mut fb);
                            std::str::from_utf8(&fb[..n]).unwrap_or("?")
                        }
                        None => "?",
                    };
                    draw_text_centered(
                        buf, stride, w, h, cx as usize, (cy - sc_size * 0.30) as usize,
                        first, 0xFFFFFFFF, 2,
                    );
                    draw_text_centered_clipped(
                        buf, stride, w, h, cx as usize, (cy + sc_size * 0.62) as usize,
                        sc_col_w * 0.94, title, state.palette.on_surface_variant, 1, FontWeight::Regular,
                    );
                }
            }

            // Bottom toolbar: back, forward, home, tabs, menu.
            let tb_y = page_y + page_h + h as f32 * 0.004;
            draw_rounded_rect_f(
                buf, stride, w, h, card_x, tb_y, card_w, toolbar_h, toolbar_h * 0.28,
                state.palette.surface_container_high,
            );
            let tb_slots = ["<", ">", "H", "1", ":"];
            let tb_slot_w = card_w / tb_slots.len() as f32;
            let tb_em = super::font::em_px_at(2, w);
            for (t_idx, symbol) in tb_slots.iter().enumerate() {
                let tx = card_x + tb_slot_w * (t_idx as f32 + 0.5);
                let t_col = if t_idx == 2 {
                    state.palette.primary
                } else {
                    state.palette.on_surface_variant
                };
                draw_text_centered(
                    buf, stride, w, h, tx as usize, (tb_y + (toolbar_h - tb_em * 0.62) * 0.5) as usize,
                    symbol, t_col, 2,
                );
            }
        } else if app_name == "Messages" {
            // Messaging: carrier header, chat bubbles, and the composer row
            // whose field and send button come from `AppLayout`.
            let card_x = bar.x + bar.h * 0.25;
            let card_w = bar.w - bar.h * 0.5;
            let inset = card_w * 0.022;
            let head_h = h as f32 * 0.028;
            let head_y = content_y + inset;
            draw_rounded_rect_f(
                buf, stride, w, h, card_x, head_y, card_w, head_h, head_h * 0.24,
                state.palette.surface_container_high,
            );
            draw_text_centered_clipped(
                buf, stride, w, h, card_x as usize, (head_y + head_h * 0.20) as usize,
                card_w - inset * 2.0, "Treble Carrier (SIM 1 - 4G LTE Active)",
                state.palette.primary, 1, FontWeight::Bold,
            );

            // Composer: the exact rects the input path hit-tests.
            let comp = al.input;
            let composer_top = comp.y - h as f32 * 0.010;
            let bubble_h = (h as f32 * 0.026).max(comp.h * 0.9);
            let bubble_gap = h as f32 * 0.006;
            let max_bubble_w = card_w - inset * 2.0;
            let m_em = super::font::em_px_at(1, w);

            // Chat list, laid out upward from the composer.
            let mut bubble_y = composer_top - bubble_gap - bubble_h;
            for msg in state.messages_list.iter().rev() {
                if bubble_y < head_y + head_h + bubble_gap {
                    break;
                }
                let msg_w = (super::font::measure(msg, m_em) + m_em * 0.9)
                    .max(bubble_h * 0.8)
                    .min(max_bubble_w);
                let bx = card_x + card_w - inset - msg_w;
                draw_rounded_rect_f(
                    buf, stride, w, h, bx, bubble_y, msg_w, bubble_h, bubble_h * 0.28,
                    state.palette.primary,
                );
                draw_text_clipped(
                    buf, stride, w, h, bx + m_em * 0.45, bubble_y + (bubble_h - m_em * 0.62) * 0.5,
                    msg_w - m_em * 0.9, msg, state.palette.on_primary, 1, FontWeight::Regular,
                );
                bubble_y -= bubble_h + bubble_gap;
            }

            // System greeting, pinned under the header.
            let greet = "Treble: Welcome! Tap below to type a message.";
            draw_rounded_rect_f(
                buf, stride, w, h, card_x + inset, head_y + head_h + bubble_gap, max_bubble_w, bubble_h,
                bubble_h * 0.28, state.palette.surface_container_high,
            );
            draw_text_clipped(
                buf, stride, w, h, card_x + inset * 2.0,
                head_y + head_h + bubble_gap + (bubble_h - m_em * 0.62) * 0.5,
                max_bubble_w - m_em * 0.9, greet, state.palette.on_surface, 1, FontWeight::Regular,
            );

            // Composer field + send button, both from `AppLayout`.
            let ring = (w as f32 * 0.003).max(1.0);
            let c_border = if state.app_input_focused {
                state.palette.primary
            } else {
                state.palette.outline
            };
            draw_rounded_rect_f(
                buf, stride, w, h, comp.x - ring, comp.y - ring, comp.w + ring * 2.0,
                comp.h + ring * 2.0, comp.radius + ring, c_border,
            );
            draw_rounded_rect_f(
                buf, stride, w, h, comp.x, comp.y, comp.w, comp.h, comp.radius,
                state.palette.surface_container_high,
            );
            let m_disp = if state.app_input.is_empty() {
                "Type a message..."
            } else {
                state.app_input
            };
            let m_col = if state.app_input.is_empty() {
                state.palette.on_surface_variant
            } else {
                state.palette.on_surface
            };
            let m_tx = comp.x + comp.h * 0.40;
            let m_ty = comp.center_y() - m_em * 0.31;
            draw_text_clipped(
                buf, stride, w, h, m_tx, m_ty, comp.w - comp.h * 0.75, m_disp, m_col, 1,
                FontWeight::Regular,
            );
            if state.app_input_focused {
                let cur_x = m_tx + super::font::measure(m_disp, m_em) + ring;
                if cur_x < comp.x + comp.w - comp.h * 0.3 {
                    draw_rect(
                        buf, stride, w, h, cur_x as usize, m_ty as usize,
                        ring.max(2.0) as usize, (m_em * 0.62) as usize, state.palette.primary,
                    );
                }
            }
            let send = al.send;
            draw_rounded_rect_f(
                buf, stride, w, h, send.x, send.y, send.w, send.h, send.h * 0.28,
                state.palette.primary,
            );
            draw_text_centered_clipped(
                buf, stride, w, h, send.center_x() as usize, (send.center_y() - m_em * 0.31) as usize,
                send.w * 0.9, "Send", state.palette.on_primary, 1, FontWeight::Bold,
            );
        } else if app_name == "Phone" {
            // Dialer: number field, RIL status, call button.
            let card_x = bar.x + bar.h * 0.25;
            let card_w = bar.w - bar.h * 0.5;
            let inset = card_w * 0.022;
            let p_em = super::font::em_px_at(2, w);
            let num_h = h as f32 * 0.032;
            let num_y = content_y + inset;
            let num_x = card_x + inset;
            let num_w = card_w - inset * 2.0;
            let ring = (w as f32 * 0.003).max(1.0);
            let num_border = if state.app_input_focused {
                state.palette.primary
            } else {
                state.palette.outline
            };
            draw_rounded_rect_f(
                buf, stride, w, h, num_x - ring, num_y - ring, num_w + ring * 2.0,
                num_h + ring * 2.0, num_h * 0.28 + ring, num_border,
            );
            draw_rounded_rect_f(
                buf, stride, w, h, num_x, num_y, num_w, num_h, num_h * 0.28,
                state.palette.surface_container_high,
            );
            let p_disp = if state.app_input.is_empty() {
                "Enter phone number..."
            } else {
                state.app_input
            };
            let p_col = if state.app_input.is_empty() {
                state.palette.on_surface_variant
            } else {
                state.palette.on_surface
            };
            let p_tx = num_x + num_h * 0.42;
            let p_ty = num_y + (num_h - p_em * 0.62) * 0.5 - p_em * 0.14;
            draw_text_clipped(
                buf, stride, w, h, p_tx, p_ty, num_w - num_h * 0.75, p_disp, p_col, 2,
                FontWeight::Regular,
            );
            if state.app_input_focused {
                let cur_x = p_tx + super::font::measure(p_disp, p_em) + ring;
                if cur_x < num_x + num_w - num_h * 0.3 {
                    draw_rect(
                        buf, stride, w, h, cur_x as usize, p_ty as usize,
                        ring.max(2.0) as usize, (p_em * 0.62) as usize, state.palette.primary,
                    );
                }
            }

            // RIL status and the call button, centred in the content card.
            let status_y = num_y + num_h + h as f32 * 0.030;
            let s_em = super::font::em_px_at(1, w);
            draw_text_centered_weighted(
                buf, stride, w, h, (card_x + card_w * 0.5) as usize, status_y as usize,
                "Universal Cellular RIL Bridge", state.palette.on_surface_variant, 1, FontWeight::Medium,
            );
            let call_h = (h as f32 * 0.036).max(w.min(h) as f32 * 0.10);
            let call_w = call_h * 3.4;
            let call_x = card_x + (card_w - call_w) * 0.5;
            let call_y = status_y + s_em * 1.6 + h as f32 * 0.012;
            draw_rounded_rect_f(
                buf, stride, w, h, call_x, call_y, call_w, call_h, call_h * 0.5, 0xFF10B981,
            );
            draw_text_centered(
                buf, stride, w, h, (call_x + call_w * 0.5) as usize,
                (call_y + (call_h - p_em * 0.62) * 0.5) as usize, "Call", 0xFFFFFFFF, 2,
            );
        } else if app_name == "Contacts" {
            // Contacts: search field over a scrollable row list.
            let (list, list_h) = draw_app_search_field(
                buf, stride, w, h, &bar, content_y, al, state, "Search contacts...",
            );
            let contacts = [
                ("Emergency Services", "112 / 911"),
                ("Voice Mailbox", "*86"),
                ("Treble Support", "+1 800 555 0199"),
            ];
            draw_app_row_list(
                buf, stride, w, h, &bar, list, list_h, &contacts, state,
            );
        } else if app_name == "Files" {
            let (list, list_h) = draw_app_search_field(
                buf, stride, w, h, &bar, content_y, al, state, "Filter files (/root)...",
            );
            let dirs = [
                ("Documents", "Directory"),
                ("Downloads", "Directory"),
                ("Pictures", "Directory"),
                ("Music", "Directory"),
            ];
            draw_app_row_list(buf, stride, w, h, &bar, list, list_h, &dirs, state);
        } else {
            // Generic app screen: search field, wordmark and an action chip.
            let (list, list_h) = draw_app_search_field(
                buf, stride, w, h, &bar, content_y, al, state, "Search or enter text...",
            );
            let cx = bar.center_x();
            let mark = super::font::em_px_at(4, w);
            let mark_y = list + h as f32 * 0.030;
            draw_text_centered(
                buf, stride, w, h, cx as usize, mark_y as usize, app_name, state.palette.primary, 4,
            );
            let sub_y = mark_y + mark * 0.95;
            draw_text_centered_clipped(
                buf, stride, w, h, cx as usize, sub_y as usize, bar.w * 0.86,
                "Universal Treble Linux Mobile Application", state.palette.on_surface_variant, 1,
                FontWeight::Regular,
            );
            let btn_h = (h as f32 * 0.030).max(w.min(h) as f32 * 0.085);
            let btn_w = btn_h * 4.0;
            let btn_x = cx - btn_w * 0.5;
            let btn_y = sub_y + h as f32 * 0.020;
            if btn_y + btn_h < list + list_h {
                draw_rounded_rect_f(
                    buf, stride, w, h, btn_x, btn_y, btn_w, btn_h, btn_h * 0.5,
                    state.palette.primary,
                );
                let em = super::font::em_px_at(1, w);
                draw_text_centered_weighted(
                    buf, stride, w, h, cx as usize, (btn_y + (btn_h - em * 0.62) * 0.5) as usize,
                    "Action Ready", state.palette.on_primary, 1, FontWeight::Bold,
                );
            }
        }

        // Gesture navigation pill, from the same layout the home screen uses.
        let nav = Layout::plain(w as f32, h as f32).nav_pill;
        draw_rounded_rect_f(
            buf, stride, w, h, nav.x, nav.y, nav.w, nav.h, nav.radius, state.palette.on_surface,
        );
    } else {
        // 5. Foundational Layer: Home Screen
        //
        // Every rectangle below comes out of the shared `Layout`, the same
        // value the input path hit-tests against, so a drawn cell and a
        // tappable cell are the same cell by construction.
        let has_selection = state.selected_icon_id.is_some();
        let l = Layout::new(w as f32, h as f32, has_selection);
        let wf = w as f32;
        let hf = h as f32;

        // 5a. Clock widget: display-weight digits over a date line.
        draw_material_you_clock(
            buf,
            stride,
            w,
            h,
            w / 2,
            l.clock_y as usize + (l.clock_h * 0.5) as usize,
            state.time_str,
            state.palette.primary,
            l.clock_h as usize,
        );
        let date_y = l.clock_y + l.clock_h + hf * 0.008;
        draw_text_centered_weighted(
            buf,
            stride,
            w,
            h,
            w / 2,
            date_y as usize,
            "Tue, Sep 22  |  28 C Sunny",
            state.palette.on_surface_variant,
            if hf >= 1200.0 { 2 } else { 1 },
            FontWeight::Medium,
        );

        // 5b. Search pill: outlined when idle, filled and accented when active.
        let s = l.search;
        let pill_bg = if state.search_active {
            state.palette.surface_container_high
        } else {
            state.palette.surface_container
        };
        let pill_fg = if state.search_active {
            state.palette.primary
        } else {
            state.palette.outline
        };
        let border = l.w * 0.004;
        draw_rounded_rect_f(
            buf, stride, w, h,
            s.x - border, s.y - border, s.w + border * 2.0, s.h + border * 2.0,
            s.radius + border, pill_fg,
        );
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            s.x as usize,
            s.y as usize,
            s.w as usize,
            s.h as usize,
            s.radius as usize,
            pill_bg,
        );

        // 5c. Search glyph, query text and caret, all on the pill's own row.
        let glyph = l.search_glyph_w;
        let glyph_x = s.x + s.h * 0.42;
        let text_x = glyph_x + glyph + s.h * 0.28;
        let text_y = s.center_y() - (super::font::em_px_at(2, w) * 0.30);
        let text_h = super::font::em_px_at(2, w) * 0.62;
        let text_max = s.x + s.w - s.h * 0.55 - text_x;
        draw_text(
            buf,
            stride,
            w,
            h,
            glyph_x as usize,
            (s.center_y() - text_h * 0.62) as usize,
            "G",
            0xFF4285F4,
            2,
        );
        let query: &str = if state.search_active {
            if state.search_query.is_empty() {
                "Type to search..."
            } else {
                state.search_query
            }
        } else {
            "Search apps, web..."
        };
        let q_color = if state.search_active && !state.search_query.is_empty() {
            state.palette.on_surface
        } else {
            state.palette.on_surface_variant
        };
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            text_x,
            text_y,
            text_max,
            query,
            q_color,
            2,
            FontWeight::Regular,
        );
        if state.search_active {
            let caret_x = text_x + text_width_at(query, 2, w) as f32 + border * 2.0;
            if caret_x < s.x + s.w - s.h * 0.3 {
                draw_rect(
                    buf,
                    stride,
                    w,
                    h,
                    caret_x as usize,
                    text_y as usize,
                    (l.w * 0.004).max(2.0) as usize,
                    text_h as usize,
                    state.palette.primary,
                );
            }
        }

        // 5d. Edit-mode action chips. Their band is what pushes the grid down.
        if has_selection {
            for (btn, label, tone) in [
                (l.remove_chip, "Remove from Home", ChipTone::Destructive),
                (
                    l.move_chip,
                    if state.home_page == 0 {
                        "Move to Page 2"
                    } else {
                        "Move to Page 1"
                    },
                    ChipTone::Primary,
                ),
            ] {
                let radius = btn.h * 0.30;
                let (bg, fg) = match tone {
                    ChipTone::Destructive => (0xFF7F1D1D, 0xFFFFFFFF),
                    ChipTone::Primary => (state.palette.surface_container_high, state.palette.on_surface),
                };
                let ring = if tone == ChipTone::Destructive { 0xFFEF4444 } else { state.palette.primary };
                let t = (l.w * 0.003).max(1.0);
                draw_rect(
                    buf,
                    stride,
                    w,
                    h,
                    (btn.x - t) as usize,
                    (btn.y - t) as usize,
                    (btn.w + t * 2.0) as usize,
                    (btn.h + t * 2.0) as usize,
                    ring,
                );
                draw_rounded_rect(
                    buf,
                    stride,
                    w,
                    h,
                    btn.x as usize,
                    btn.y as usize,
                    btn.w as usize,
                    btn.h as usize,
                    radius as usize,
                    bg,
                );
                let em = super::font::em_px_at(1, w);
                draw_text_centered_weighted(
                    buf,
                    stride,
                    w,
                    h,
                    btn.center_x() as usize,
                    (btn.center_y() - em * 0.30) as usize,
                    label,
                    fg,
                    1,
                    FontWeight::Medium,
                );
            }
        }

        // 5e. Workspace grid. Icons carry the press spring, the label sits in
        // the gap the layout reserved for it, and both move with the page.
        let max_apps = l.max_rows * l.grid_cols;
        let scroll = state.home_scroll_offset;

        for (idx, app) in state.grid_apps.iter().take(max_apps).enumerate() {
            let icon = l.grid_icon(idx);
            let (cx, cy) = (icon.center_x() + scroll, icon.center_y());
            if cx + icon.w < -4.0 || cx - icon.w > wf + 4.0 {
                continue;
            }
            // Tactile press compression (0.92x) with a spring rebound.
            let pressed = state.pressed_icon_id == Some(app.id);
            let k = if pressed { state.icon_press_scale.clamp(0.5, 1.5) } else { 1.0 };
            let size = (icon.w * k).round();
            let radius = (icon.radius * k).round();
            let x = (cx - size * 0.5).round() as i32;
            let y = (cy - size * 0.5).round() as i32;

            if state.selected_icon_id == Some(app.id) {
                let pad = l.icon_size * 0.10 * k;
                let inner = (pad * 0.5).max(1.0);
                draw_rounded_rect_i32(
                    buf, stride, w, h,
                    (x as f32 - pad).round() as i32, (y as f32 - pad).round() as i32,
                    (size + pad * 2.0).round() as usize, (size + pad * 2.0).round() as usize,
                    (radius + pad) as usize, state.palette.primary,
                );
                draw_rounded_rect_i32(
                    buf, stride, w, h,
                    (x as f32 - pad + inner).round() as i32,
                    (y as f32 - pad + inner).round() as i32,
                    (size + pad * 2.0 - inner * 2.0).round() as usize,
                    (size + pad * 2.0 - inner * 2.0).round() as usize,
                    (radius + pad - inner) as usize, state.palette.surface,
                );
            }

            draw_rounded_rect_i32(
                buf, stride, w, h, x, y, size as usize, size as usize, radius as usize, app.color,
            );
            match app.icon {
                Some(icon_img) => {
                    draw_icon_bitmap_i32(
                        buf, stride, w, h, x, y, size as usize, size as usize, radius as usize, icon_img,
                    );
                }
                None => {
                    let em = super::font::em_px_at(2, w);
                    draw_text_centered_i32(
                        buf, stride, w, h, cx as i32, (cy - em * 0.30) as i32, app.glyph, 0xFFFFFFFF, 2,
                    );
                }
            }
            // Label: centred under the icon, in the reserved gap.
            let label_y = icon.y + icon.h + icon.h * LABEL_GAP;
            let label_w = l.col_pitch * 0.94;
            draw_text_centered_clipped_i32(
                buf,
                stride,
                w,
                h,
                cx as i32,
                label_y,
                label_w,
                app.name,
                state.palette.on_surface,
                l.label_scale,
                FontWeight::Medium,
            );
        }

        // 5f. Page indicator: inert dots plus a sliding active pill.
        let total_pages = state.total_home_pages.max(1);
        let dots = l.page_dots;
        let dot_r = dots.h * 0.5;
        let pitch = if total_pages > 1 { dots.w / (total_pages as f32 - 1.0) } else { 0.0 };
        let dot_w = dot_r * 2.0;
        let x0 = dots.center_x() - pitch * (total_pages as f32 - 1.0) * 0.5;
        let dot_color = (0x66 << 24) | (state.palette.on_surface & 0x00FFFFFF);
        for p in 0..total_pages {
            let dx = (x0 + p as f32 * pitch).round() as usize;
            draw_rounded_rect(
                buf, stride, w, h, dx, dots.y as usize, dot_w as usize, dots.h as usize, dot_r as usize, dot_color,
            );
        }
        // Fractional position: page index minus the scroll fraction of a page.
        let frac = (state.home_page as f32 - scroll / wf).clamp(0.0, (total_pages - 1) as f32);
        let ax = (x0 + frac * pitch - dot_w * 0.5).round() as i32;
        let pill_w = dot_w * 3.0;
        draw_rounded_rect_i32(
            buf, stride, w, h, ax, dots.y as i32, pill_w as usize, dots.h as usize, dot_r as usize, state.palette.primary,
        );

        // 5g. Hotseat.
        draw_rounded_rect(
            buf, stride, w, h,
            l.dock.x as usize, l.dock.y as usize, l.dock.w as usize, l.dock.h as usize,
            l.dock.radius as usize, state.palette.surface_container,
        );
        let fallback_dock = [
            AppGridItem { id: "phone", name: "Phone", color: 0xFF10B981, glyph: "P", icon: None },
            AppGridItem { id: "messages", name: "Messages", color: 0xFF3B82F6, glyph: "M", icon: None },
            AppGridItem { id: "apps", name: "Apps", color: 0xFF475569, glyph: ":", icon: None },
            AppGridItem { id: "browser", name: "Browser", color: 0xFF06B6D4, glyph: "B", icon: None },
            AppGridItem { id: "camera", name: "Camera", color: 0xFFF43F5E, glyph: "C", icon: None },
        ];
        let dock_apps: &[AppGridItem] = if state.dock_apps.is_empty() {
            &fallback_dock[..]
        } else {
            state.dock_apps
        };
        // The rendered slot count follows the apps present, so the icons stay
        // centred even when the hotseat is not full.
        let slots = dock_apps.len().min(l.dock_slots).max(1);
        for (i, app) in dock_apps.iter().take(slots).enumerate() {
            let pitch = l.dock.w / slots as f32;
            let cx = l.dock.x + pitch * (i as f32 + 0.5);
            let cy = l.dock.center_y();
            let pressed = state.pressed_icon_id == Some(app.id);
            let k = if pressed { state.icon_press_scale.clamp(0.5, 1.5) } else { 1.0 };
            let size = (l.dock_icon * k).round();
            let radius = (l.dock_icon * ICON_RADIUS * k).round();
            let x = (cx - size * 0.5).round() as i32;
            let y = (cy - size * 0.5).round() as i32;
            draw_rounded_rect_i32(buf, stride, w, h, x, y, size as usize, size as usize, radius as usize, app.color);
            match app.icon {
                Some(icon_img) => {
                    draw_icon_bitmap_i32(buf, stride, w, h, x, y, size as usize, size as usize, radius as usize, icon_img);
                }
                None => {
                    let em = super::font::em_px_at(2, w);
                    draw_text_centered_i32(
                        buf, stride, w, h, cx as i32, (cy - em * 0.30) as i32, app.glyph, 0xFFFFFFFF, 2,
                    );
                }
            }
        }

        // 5h. Gesture navigation pill, drawn last so overlays can cover it.
        draw_rounded_rect(
            buf, stride, w, h,
            l.nav_pill.x as usize, l.nav_pill.y as usize, l.nav_pill.w as usize, l.nav_pill.h as usize,
            l.nav_pill.radius as usize, state.palette.on_surface,
        );

        // 5i. App drawer overlay: frosted sheet sliding up over the workspace.
        if state.app_drawer_open || state.drawer_progress > 0.001 {
            let prog = if state.drawer_progress > 0.001 {
                state.drawer_progress.clamp(0.0, 1.0)
            } else if state.app_drawer_open {
                1.0
            } else {
                0.0
            };
            let off = ((1.0 - prog) * hf).round() as i32;
            if off < h as i32 {
                // The sheet below is opaque and covers rows off..h, so only
                // the strip above it is ever seen through the glass. When
                // the drawer is fully open (off == 0) there is nothing to
                // frost and the whole pass would be dead work.
                if off > 0 {
                    apply_frosted_blur_region(buf, stride, w, 0, off as usize);
                }
                let sheet = state.palette.surface_container;
                draw_rect_f(
                    buf, stride, w, h, 0.0, off.max(0) as f32, w as f32, (h as f32 - off as f32).max(0.0),
                    sheet,
                );

                draw_status_bar(buf, stride, w, h, state, off);

                // Pull handle.
                let hd = l.drawer_handle;
                draw_rounded_rect(
                    buf, stride, w, h,
                    hd.x as usize, (off as f32 + hd.y) as usize, hd.w as usize, hd.h as usize,
                    hd.radius as usize, state.palette.outline,
                );

                // Drawer search pill.
                let ds = l.drawer_search;
                let ds_y = off as f32 + ds.y;
                let ds_x = ds.x;
                let ds_w = ds.w;
                let ds_border = (0x55 << 24) | (state.palette.primary & 0x00FFFFFF);
                let ds_bg = (0xF0 << 24) | (state.palette.surface_container_high & 0x00FFFFFF);
                let t = (l.w * 0.003).max(1.0);
                draw_rounded_rect_f(
                    buf, stride, w, h,
                    ds_x - t, ds_y - t, ds_w + t * 2.0, ds.h + t * 2.0, ds.radius + t, ds_border,
                );
                draw_rounded_rect(
                    buf, stride, w, h,
                    ds_x as usize, ds_y as usize, ds_w as usize, ds.h as usize, ds.radius as usize, ds_bg,
                );
                let d_em = super::font::em_px_at(2, w);
                let d_text_h = d_em * 0.62;
                let d_glyph_x = ds_x + ds.h * 0.40;
                let d_text_x = d_glyph_x + ds.h * 0.30 + d_em * 0.30;
                let d_text_y = ds_y + (ds.h - d_text_h) * 0.5 - d_em * 0.20;
                draw_text(buf, stride, w, h, d_glyph_x as usize, d_text_y as usize, "G", 0xFF4285F4, 1);
                let dstext: &str = if state.drawer_search.is_empty() {
                    "Search all apps"
                } else {
                    state.drawer_search
                };
                let dcol = if state.drawer_search.is_empty() {
                    state.palette.on_surface_variant
                } else {
                    state.palette.on_surface
                };
                let d_clear_w = if state.drawer_search.is_empty() { 0.0 } else { ds.h * 0.62 };
                draw_text_clipped(
                    buf, stride, w, h, d_text_x, d_text_y,
                    ds_x + ds.w - ds.h * 0.35 - d_clear_w - d_text_x, dstext, dcol, 2, FontWeight::Regular,
                );
                if !state.drawer_search.is_empty() {
                    let cx = ds_x + ds.w - ds.h * 0.66;
                    draw_circle_glyph(buf, stride, w, h, cx, ds_y + ds.h * 0.5, ds.h * 0.22, state.palette.on_surface_variant);
                    let arm = ds.h * 0.16;
                    draw_line(buf, stride, w, h, cx + arm * 0.6, ds_y + ds.h * 0.5 + arm * 0.6, cx + arm, ds_y + ds.h * 0.5 + arm, state.palette.on_surface_variant);
                }

                // Section header with the right-aligned app count.
                let hy = off as f32 + l.drawer_header_y;
                draw_text_weighted(
                    buf, stride, w, h, l.drawer_search.x as usize, hy as usize,
                    "ALL APPLICATIONS", state.palette.on_surface_variant, 1, FontWeight::Bold,
                );
                let mut count_buf = [0u8; 16];
                let count_str = format_apps_count(&mut count_buf, state.drawer_apps.len());
                let cw = text_width_at(count_str, 1, w) as f32;
                draw_text_weighted(
                    buf, stride, w, h, (ds_x + ds_w - cw) as usize, hy as usize,
                    count_str, state.palette.outline, 1, FontWeight::Medium,
                );

                // Full app grid, clipped to the drawer's own band.
                let max_apps = l.drawer_rows * l.grid_cols;
                let d_label_em = super::font::em_px(l.drawer_label_scale);
                for (idx, app) in state.drawer_apps.iter().take(max_apps).enumerate() {
                    let cell = l.drawer_icon_cell(idx);
                    let (cx, cy) = (cell.center_x(), off as f32 + cell.center_y());
                    let pressed = state.pressed_icon_id == Some(app.id);
                    let k = if pressed { state.icon_press_scale.clamp(0.5, 1.5) } else { 1.0 };
                    let size = (cell.w.min(cell.h) * k).round();
                    let radius = (cell.radius * k).round();
                    let x = (cx - size * 0.5).round() as i32;
                    let y = (cy - size * 0.5).round() as i32;
                    if state.selected_icon_id == Some(app.id) {
                        let pad = cell.h * 0.10 * k;
                        draw_rounded_rect_i32(
                            buf, stride, w, h,
                            (x as f32 - pad).round() as i32, (y as f32 - pad).round() as i32,
                            (size + pad * 2.0).round() as usize, (size + pad * 2.0).round() as usize,
                            (radius + pad) as usize, state.palette.primary,
                        );
                    }
                    draw_rounded_rect_i32(
                        buf, stride, w, h, x, y, size as usize, size as usize, radius as usize, app.color,
                    );
                    match app.icon {
                        Some(icon_img) => {
                            draw_icon_bitmap_i32(
                                buf, stride, w, h, x, y, size as usize, size as usize, radius as usize, icon_img,
                            );
                        }
                        None => {
                            draw_text_centered_i32(
                                buf, stride, w, h, cx as i32, (cy - d_label_em * 0.30) as i32,
                                app.glyph, 0xFFFFFFFF, 2,
                            );
                        }
                    }
                    let label_y = off as f32 + cell.y + cell.h + cell.h * LABEL_GAP;
                    draw_text_centered_clipped_i32(
                        buf, stride, w, h, cx as i32, label_y, l.col_pitch * 0.94,
                        app.name, state.palette.on_surface, l.drawer_label_scale, FontWeight::Medium,
                    );
                }

                // The drawer's own nav pill sits above the blur sheet.
                draw_rounded_rect(
                    buf, stride, w, h,
                    l.nav_pill.x as usize, l.nav_pill.y as usize, l.nav_pill.w as usize, l.nav_pill.h as usize,
                    l.nav_pill.radius as usize, state.palette.on_surface,
                );
            }
        }
    }

    // 10. Virtual keyboard, when active.
    //
    // Key rects come straight from `Keyboard`, the same struct the input path
    // hit-tests, so a key that is drawn is exactly a key that can be pressed.
    if state.keyboard_active && !state.is_locked && !state.shade_open {
        let kb = Keyboard::new(w as f32, h as f32);
        let f = kb.frame;
        let k_em = super::font::em_px_at(2, w);
        let k_text_h = k_em * 0.62;

        // Sheet, with a hairline along the top edge.
        draw_rounded_rect_f(
            buf, stride, w, h, f.x, f.y, f.w, f.h, f.radius,
            state.palette.surface_container,
        );
        draw_rect_f(
            buf, stride, w, h, f.x, f.y, f.w, (h as f32 * 0.0012).max(1.0),
            state.palette.outline,
        );

        // Modifier keys, then the character rows.
        let shift_on = state.keyboard_shift_active;
        let (shift_bg, shift_fg) = if shift_on {
            (state.palette.primary, state.palette.on_primary)
        } else {
            (state.palette.surface_container_high, state.palette.on_surface)
        };
        fn key(
            buf: &mut [u32], stride: usize, w: usize, h: usize, r: &super::layout::Rect, bg: u32,
        ) {
            draw_rounded_rect_f(buf, stride, w, h, r.x, r.y, r.w, r.h, r.radius, bg);
        }
        #[allow(clippy::too_many_arguments)]
        fn legend(
            buf: &mut [u32], stride: usize, w: usize, h: usize, r: &super::layout::Rect,
            label: &str, col: u32, scale: usize,
        ) {
            let em = super::font::em_px_at(scale, w);
            draw_text_centered(
                buf, stride, w, h, r.center_x() as usize,
                (r.center_y() - em * 0.31) as usize, label, col, scale,
            );
        }

        key(buf, stride, w, h, &kb.row3_shift, shift_bg);
        legend(buf, stride, w, h, &kb.row3_shift, if shift_on { "V" } else { "^" }, shift_fg, 1);
        key(buf, stride, w, h, &kb.row3_backspace, state.palette.surface_container_high);
        // Backspace: a left-pointing wedge plus the delete bar.
        let bx = kb.row3_backspace.center_x();
        let by = kb.row3_backspace.center_y();
        let br = kb.row3_backspace.h * 0.17;
        for i in 0..=4 {
            let t = i as f32 / 4.0;
            let px = bx - br * 1.5 + t * br * 1.4;
            let dy = br * (1.0 - (t * 2.0 - 1.0).abs());
            draw_line(
                buf, stride, w, h, px, by - dy, px, by + dy,
                state.palette.on_surface_variant,
            );
        }
        draw_line(
            buf, stride, w, h, bx - br * 0.1, by, bx + br * 1.4, by,
            state.palette.on_surface_variant,
        );

        for i in 0..KB_ROW1 {
            let r = kb.row1_at(i);
            key(buf, stride, w, h, &r, state.palette.surface_container_high);
            let ch = super::layout::ROW1[i];
            let mut lb = [0u8; 4];
            let label: &str = if shift_on {
                legend_label(&mut lb, ch)
            } else {
                let n = ch.len_utf8();
                ch.encode_utf8(&mut lb);
                std::str::from_utf8(&lb[..n]).unwrap_or("?")
            };
            legend(buf, stride, w, h, &r, label, state.palette.on_surface, 2);
        }
        for i in 0..KB_ROW2 {
            let r = kb.row2_at(i);
            key(buf, stride, w, h, &r, state.palette.surface_container_high);
            let ch = super::layout::ROW2[i];
            let mut lb = [0u8; 4];
            let label: &str = if shift_on {
                legend_label(&mut lb, ch)
            } else {
                let n = ch.len_utf8();
                ch.encode_utf8(&mut lb);
                std::str::from_utf8(&lb[..n]).unwrap_or("?")
            };
            legend(buf, stride, w, h, &r, label, state.palette.on_surface, 2);
        }
        for i in 0..KB_ROW3_MID {
            let r = kb.row3_mid[i];
            key(buf, stride, w, h, &r, state.palette.surface_container_high);
            let ch = super::layout::ROW3[i];
            let mut lb = [0u8; 4];
            let label: &str = if shift_on {
                legend_label(&mut lb, ch)
            } else {
                let n = ch.len_utf8();
                ch.encode_utf8(&mut lb);
                std::str::from_utf8(&lb[..n]).unwrap_or("?")
            };
            legend(buf, stride, w, h, &r, label, state.palette.on_surface, 2);
        }

        // Bottom row: hide, space with a language label, enter.
        key(buf, stride, w, h, &kb.row4_hide, state.palette.surface_container_high);
        legend(
            buf, stride, w, h, &kb.row4_hide, "Hide", state.palette.on_surface_variant, 1,
        );
        key(buf, stride, w, h, &kb.row4_space, state.palette.surface_container_high);
        legend(
            buf, stride, w, h, &kb.row4_space, "English", state.palette.on_surface_variant, 1,
        );
        key(buf, stride, w, h, &kb.row4_enter, state.palette.primary);
        legend(buf, stride, w, h, &kb.row4_enter, "Enter", state.palette.on_primary, 1);
        let _ = k_text_h;
    }

    // 11. Pointer, drawn above every surface.
    if let Some((cx, cy)) = state.cursor_pos {
        let l = Layout::plain(w as f32, h as f32);
        if state.is_touching {
            // Pressed: a filled dot in the accent colour.
            draw_circle_glyph(
                buf, stride, w, h, cx as f32, cy as f32, l.w * 0.008,
                state.palette.primary,
            );
        } else {
            // Hovering: a ring, so it never hides what it points at.
            let r = l.w * 0.006;
            draw_circle_glyph(
                buf, stride, w, h, cx as f32, cy as f32, r,
                (0xAA << 24) | (state.palette.on_surface & 0x00FF_FFFF),
            );
            draw_circle_glyph(
                buf, stride, w, h, cx as f32, cy as f32, r * 0.42, state.palette.primary,
            );
        }
    }

    // 12. Material You touch ripple.
    //
    // The state layer of Material 3: a translucent circle in the on-surface
    // colour that expands from the touch point and fades out. `radius` grows
    // on a time constant and `alpha` falls with it, which is what makes it
    // read as a ripple rather than a blinking dot.
    if let Some((rx, ry, radius, alpha)) = state.touch_ripple {
        if alpha > 0.01 && radius > 1.0 {
            let a = (alpha * 0.32).clamp(0.0, 1.0);
            let layer = ((a * 255.0) as u32) << 24 | (state.palette.on_surface & 0x00FF_FFFF);
            // Ring border plus a soft interior, both on the same state layer.
            draw_circle_glyph(buf, stride, w, h, rx, ry, radius, layer);
            draw_circle_glyph(
                buf, stride, w, h, rx, ry, radius * 0.86, layer,
            );
        }
    }

    // 13. App Launch Expansion Animation (Lawnchair 17 / Pixel Launcher app opening transition)
    // 12. App launch container transform.
    //
    // The expanding card grows out of the icon that was tapped: it starts at
    // the icon's own size and corner radius and ends covering the panel, which
    // is Launcher3's container transform. Progress comes from the spring in
    // `state.app_launch_progress`, so the motion has overshoot and settles.
    if let Some((ox, oy)) = state.app_launch_origin {
        let t = state.app_launch_progress.clamp(0.0, 1.0);
        if t > 0.001 && t < 0.999 {
            let l = Layout::plain(w as f32, h as f32);
            // Interpolate size, centre and radius together, easing the first
            // third so the card reads as leaving the icon rather than growing.
            let e = t * t * (3.0 - 2.0 * t);
            let start = l.icon_size;
            let cur_w = start + (w as f32 - start) * e;
            let cur_h = start + (h as f32 - start) * e;
            let cur_x = (ox - cur_w * 0.5).clamp(0.0, (w as f32 - cur_w).max(0.0));
            let cur_y = (oy - cur_h * 0.5).clamp(0.0, (h as f32 - cur_h).max(0.0));
            let radius = l.icon_radius + (l.nav_pill.radius.max(16.0) - l.icon_radius) * e;
            // The card is the app's window from the first frame, so it is
            // mostly opaque throughout; only the last stretch fades in the
            // surface tint behind it.
            let alpha = ((0.62 + 0.38 * e) * 255.0) as u32;
            let app_rgb = state.app_launch_color & 0x00FF_FFFF;
            // A hairline in the primary colour keeps the card edge crisp
            // while it is still small enough to read as a chip. The ring is
            // a stroke (four thin strips), not a second full fill: the inner
            // card would otherwise overwrite 100% of it.
            let edge = (0x66 << 24) | (state.palette.primary & 0x00FF_FFFF);
            let ring = (l.w * 0.004).max(1.0).min(cur_w * 0.5).min(cur_h * 0.5);
            let color = (alpha << 24) | app_rgb;
            draw_rounded_rect_f(
                buf, stride, w, h, cur_x, cur_y, cur_w, cur_h, radius, color,
            );
            draw_rect_f(buf, stride, w, h, cur_x, cur_y, cur_w, ring, edge);
            draw_rect_f(buf, stride, w, h, cur_x, cur_y + cur_h - ring, cur_w, ring, edge);
            draw_rect_f(buf, stride, w, h, cur_x, cur_y, ring, cur_h, edge);
            draw_rect_f(buf, stride, w, h, cur_x + cur_w - ring, cur_y, ring, cur_h, edge);
        }
    }

    // 14. Volume HUD overlay if active
    if let Some(sex) = state.super_extreme_state {
        if sex.volume_hud.is_visible() {
            let cx = (w as f32 * 0.5) as usize;
            let em1 = super::font::em_px_at(1, w);
            let bar = sex.volume_hud.format_bar(34);
            draw_rounded_rect_f(buf, stride, w, h, w as f32 * 0.05, 8.0, w as f32 * 0.90, em1 * 2.2, 4.0, 0xFF0F172A);
            draw_text_centered(buf, stride, w, h, cx, (8.0 + em1 * 0.4) as usize, &bar, 0xFF38BDF8, 1);
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
    macro_rules! mix {
        ($v:expr) => {{
            h ^= $v as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }};
    }
    for b in state.time_str.bytes() {
        mix!(b);
    }
    // The palette is part of the visible state: a wallpaper change has to
    // invalidate the frame cache or the shell keeps the old theme.
    mix!(state.palette.surface);
    mix!(state.palette.surface_container);
    mix!(state.palette.surface_container_high);
    mix!(state.palette.primary);
    mix!(state.palette.on_primary);
    mix!(state.palette.primary_container);
    mix!(state.palette.on_primary_container);
    mix!(state.palette.secondary);
    mix!(state.palette.tertiary);
    mix!(state.palette.on_surface);
    mix!(state.palette.on_surface_variant);
    mix!(state.palette.outline);
    mix!(state.palette.outline_variant);
    mix!(state.is_locked as u8);
    mix!(state.is_touching as u8);
    mix!(state.search_active as u8);
    mix!(state.keyboard_active as u8);
    mix!(state.shade_open as u8);
    match state.cursor_pos {
        Some((x, y)) => {
            for b in x.to_ne_bytes() {
                mix!(b);
            }
            for b in y.to_ne_bytes() {
                mix!(b);
            }
        }
        None => mix!(0xFF),
    }
    for b in state.search_query.bytes() {
        mix!(b);
    }
    for (i, t) in state.quick_tiles_active.iter().enumerate() {
        if *t {
            mix!(i as u8);
        }
    }
    match state.active_app {
        Some(a) => {
            mix!(1);
            for b in a.bytes() {
                mix!(b);
            }
        }
        None => mix!(0),
    }
    for b in state.terminal_input.bytes() {
        mix!(b);
    }
    for b in state.terminal_prompt.bytes() {
        mix!(b);
    }
    mix!(state.terminal_running as u8);
    mix!(state.terminal_active_tab as u64);
    mix!(state.terminal_tabs.len() as u64);
    for tab in state.terminal_tabs {
        mix!(tab.id as u64);
        mix!(tab.is_running as u8);
        mix!(tab.is_active as u8);
        for b in tab.title.bytes() {
            mix!(b);
        }
    }
    for b in state.terminal_lines.len().to_ne_bytes() {
        mix!(b);
    }
    if let Some(last) = state.terminal_lines.last() {
        for b in last.bytes() {
            mix!(b);
        }
    }
    mix_app_row(&mut h, state.grid_apps);
    mix_app_row(&mut h, state.dock_apps);
    for b in state.app_input.bytes() {
        mix!(b);
    }
    mix!(state.app_input_focused as u8);
    mix!(state.keyboard_shift_active as u8);
    mix!(state.app_drawer_open as u8);
    mix!(state.home_page as u64);
    mix!(state.total_home_pages as u64);
    for b in state.drawer_search.bytes() {
        mix!(b);
    }
    if let Some(sel) = state.selected_icon_id {
        mix!(1);
        for b in sel.bytes() {
            mix!(b);
        }
    } else {
        mix!(0);
    }
    mix_app_row(&mut h, state.drawer_apps);
    mix!(state.messages_list.len() as u64);
    if let Some(last) = state.messages_list.last() {
        for b in last.bytes() {
            mix!(b);
        }
    }
    mix!((state.drawer_progress * 1000.0) as u32);
    mix!((state.home_scroll_offset * 100.0) as i32);
    mix!((state.app_launch_progress * 1000.0) as u32);
    mix!(state.app_launch_color);
    if let Some((ox, oy)) = state.app_launch_origin {
        mix!(1);
        mix!(ox as u32);
        mix!(oy as u32);
    } else {
        mix!(0);
    }
    if let Some((rx, ry, radius, alpha)) = state.touch_ripple {
        mix!(1);
        mix!(rx as u32);
        mix!(ry as u32);
        mix!((radius * 10.0) as u32);
        mix!((alpha * 100.0) as u32);
    } else {
        mix!(0);
    }
    if let Some(pid) = state.pressed_icon_id {
        mix!(1);
        for b in pid.bytes() {
            mix!(b);
        }
        mix!((state.icon_press_scale * 1000.0) as u32);
    } else {
        mix!(0);
    }
    mix!(state.power_saver_mode as u8);
    if let Some(ref sex) = state.super_extreme_state {
        mix!(sex.active_screen as u8);
        mix!(sex.password_input.len());
        mix!(sex.emergency_input.len());
        mix!(sex.phone_input.len());
        mix!(sex.volume_hud.volume_percent);
        mix!(sex.volume_hud.is_visible() as u8);
        mix!(sex.camera_preview.frame_counter);
        mix!(sex.power_menu_selected);
        for a in &sex.alarms {
            mix!(a.enabled as u8);
        }
    }
    h
}

/// Zero-allocation, in-place frosted glass over a scanline region.
///
/// A 3x3 box average plus a palette-tinted veil, evaluated entirely in place:
/// no scratch buffer, no allocation, and it darkens rather than washes out so
/// text behind the glass stays readable. Tile size is fixed at 3px which is
/// cheap enough to run over a 2.6Mpx frame inside the render budget.
pub fn apply_frosted_blur_region(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    y_start: usize,
    y_end: usize,
) {
    const TILE: usize = 3;
    // Rows are clamped to what the buffer actually holds: the caller passes a
    // region in pixels, and `stride` gives the addressable row count.
    let stride = stride.max(1);
    let w = w.min(stride);
    let rows = buf.len() / stride;
    let y_start = y_start.min(y_end).min(rows);
    let y_end = y_end.min(rows);

    // Interior tiles always have exactly TILE*TILE = 9 samples, so their
    // divisor is the compile-time constant 72 and the three divisions become
    // multiply+shift instead of `udiv` (a non-pipelined 20-40 cycle
    // instruction). At 1080 px wide that is 99% of all tiles; only the
    // two leftmost and two rightmost columns and the top/bottom rows are
    // partial, and those keep the general path below. Loop shape is
    // identical to the general path so the codegen is the same.
    let full_y_end = y_start + ((y_end - y_start) / TILE) * TILE;
    let full_x_end = (w / TILE) * TILE;
    for by in (y_start..full_y_end).step_by(TILE) {
        for bx in (0..full_x_end).step_by(TILE) {
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            for y in by..by + TILE {
                let row = y * stride;
                for x in bx..bx + TILE {
                    let p = buf[row + x];
                    r += (p >> 16) & 0xFF;
                    g += (p >> 8) & 0xFF;
                    b += p & 0xFF;
                }
            }
            // (sum*5)/72 + 1; sum <= 9*255 so the clamp is the only guard.
            let o = |sum: u32| -> u32 { (sum * 5 / 72 + 1).min(255) };
            let pixel = (0xFF << 24) | (o(r) << 16) | (o(g) << 8) | o(b);
            for y in by..by + TILE {
                let row = y * stride;
                buf[row + bx..row + bx + TILE].fill(pixel);
            }
        }
    }

    // Partial tiles: the region edges, where n < 9 and the divisor is a
    // runtime value.
    for by in (y_start..y_end).step_by(TILE) {
        let y1 = (by + TILE).min(y_end);
        for bx in (0..w).step_by(TILE) {
            let x1 = (bx + TILE).min(w);
            if y1 - by == TILE && x1 - bx == TILE {
                continue; // handled above
            }
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            let mut n = 0u32;
            for y in by..y1 {
                let row = y * stride;
                for x in bx..x1 {
                    let p = buf[row + x];
                    r += (p >> 16) & 0xFF;
                    g += (p >> 8) & 0xFF;
                    b += p & 0xFF;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            // Veil toward the deep surface so the glass reads as frosted and
            // contrast is preserved instead of being averaged into mush.
            // out(sum) = sum*5/(8n) + 11n/(8n); the veil is 1 for every n >= 1,
            // so it is computed once per tile instead of once per channel.
            let den = (n * 8).max(1);
            let veil = (0x0B * n) / den;
            let out = |sum: u32| -> u32 { ((sum * 5) / den + veil).min(255) };
            let pixel = (0xFF << 24) | (out(r) << 16) | (out(g) << 8) | out(b);
            for y in by..y1 {
                let row = y * stride;
                buf[row + bx..row + x1].fill(pixel);
            }
        }
    }
}

/// Frost the whole frame.
pub fn apply_frosted_blur(buf: &mut [u32], stride: usize, w: usize, h: usize) {
    apply_frosted_blur_region(buf, stride, w, 0, h);
}

/// Mix an app row (name, colour, icon presence and a sampled icon signature)
/// into the frame hash so a freshly decoded icon triggers exactly one redraw.
fn mix_app_row(h: &mut u64, apps: &[AppGridItem<'_>]) {
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut mix = |v: u64| {
        *h ^= v;
        *h = h.wrapping_mul(FNV_PRIME);
    };
    mix(apps.len() as u64);
    for app in apps {
        for b in app.id.bytes() {
            mix(b as u64);
        }
        for b in app.name.bytes() {
            mix(b as u64);
        }
        for b in app.glyph.bytes() {
            mix(b as u64);
        }
        mix(app.color as u64);
        match app.icon {
            None => mix(0x55),
            Some(img) => {
                mix(0xAA);
                mix(img.width as u64);
                mix(img.height as u64);
                // Sample ~61 bytes instead of the whole bitmap: enough to tell
                // two icons apart while keeping the per-frame cost negligible.
                let len = img.pixels.len();
                let step = (len / 61).max(1);
                let mut i = 0;
                while i < len {
                    mix(img.pixels[i] as u64);
                    i += step;
                }
            }
        }
    }
}

/// Blit a decoded RGBA icon into the `x,y .. x+tw,y+th` tile: bilinearly
/// scaled, composited premultiplied over the tile behind it, and clipped to the
/// same rounded-rectangle mask so the icon corners follow the tile corners.
#[inline]
#[allow(clippy::too_many_arguments)]
fn draw_icon_bitmap_i32(
    buf: &mut [u32],
    stride: usize,
    fw: usize,
    fh: usize,
    x: i32,
    y: i32,
    tw: usize,
    th: usize,
    radius: usize,
    img: &RgbaImage,
) {
    if x + tw as i32 <= 0 || x >= fw as i32 || y + th as i32 <= 0 || y >= fh as i32 {
        return;
    }
    let iw = img.width as usize;
    let ih = img.height as usize;
    if iw == 0 || ih == 0 || tw == 0 || th == 0 || img.pixels.len() < iw * ih * 4 {
        return;
    }
    let radius = radius.min(tw / 2).min(th / 2);
    let r2 = (radius * radius) as i32;
    let x_start = x.max(0) as usize;
    let y_start = y.max(0) as usize;
    let x_end = ((x + tw as i32) as usize).min(fw);
    let y_end = ((y + th as i32) as usize).min(fh);
    let src = img.pixels.as_slice();

    // Fixed-point source sampling, computed incrementally.
    //
    // The previous version recomputed a 64-bit multiply-and-divide per axis
    // per pixel and then did three more 64-bit divisions to un-premultiply,
    // which cost more than the rest of the frame combined. The step is
    // constant for a tile, so it is computed once and accumulated; the
    // un-premultiply uses one reciprocal.
    let step_x = (iw as i64 * 65536) / tw as i64;
    let hi_x = ((iw as i64 - 1) << 16).max(0);
    let hi_y = ((ih as i64 - 1) << 16).max(0);

    // 1:1 fast path.
    //
    // Icons are pre-scaled to the panel's icon size when they enter the cache,
    // so the common case needs no resampling at all: a straight alpha blend
    // over the rounded-rect span. This is several times faster than the
    // bilinear path and is what a launcher full of icons actually hits.
    let identity = tw == iw && th == ih;

    for cy in y_start..y_end {
        let (mut lo, mut hi) = match rounded_span(
            cy as i32 - y,
            x,
            tw as i32,
            th as i32,
            radius as i32,
            r2,
        ) {
            Some(span) => span,
            None => continue,
        };
        lo = lo.max(x_start as i32);
        hi = hi.min(x_end as i32);
        if hi <= lo {
            continue;
        }
        let cy_i = cy as i32;
        // Vertical taps, clamped once per row.
        let fy0 = ((2 * (cy_i - y) as i64 + 1) * ih as i64 * 65536) / (2 * th as i64) - 32768;
        let fy0 = fy0.clamp(0, hi_y);
        let sy0 = (fy0 >> 16) as usize;
        let wy0 = (fy0 & 0xFFFF) as u32;
        let sy1 = (sy0 + 1).min(ih - 1);
        let row = cy * stride;
        let y_base_a = sy0 * iw * 4;
        let y_base_b = sy1 * iw * 4;
        let wa = 65536u64 - wy0 as u64;
        let wb = wy0 as u64;
        // Horizontal source coordinate restarts each row at the span's left edge.
        let mut fx = ((2 * (lo - x) as i64 + 1) * iw as i64 * 65536) / (2 * tw as i64) - 32768;

        if identity {
            for cx in lo..hi {
                let o = y_base_a + (cx - x) as usize * 4;
                let a = src[o + 3] as u32;
                if a == 0 {
                    continue;
                }
                let dst = buf[row + cx as usize];
                if a == 255 {
                    buf[row + cx as usize] =
                        (0xFF << 24) | ((src[o] as u32) << 16) | ((src[o + 1] as u32) << 8)
                            | src[o + 2] as u32;
                    continue;
                }
                let inv = 255 - a;
                let ch = |c: u8, bg: u32| -> u32 { (c as u32 * a + bg * inv + 127) / 255 };
                buf[row + cx as usize] = (0xFF << 24)
                    | (ch(src[o], (dst >> 16) & 0xFF) << 16)
                    | (ch(src[o + 1], (dst >> 8) & 0xFF) << 8)
                    | ch(src[o + 2], dst & 0xFF);
            }
            continue;
        }

        for cx in lo..hi {
            let fx_c = fx.clamp(0, hi_x);
            fx += step_x;
            let sx0 = (fx_c >> 16) as usize;
            let wx0 = (fx_c & 0xFFFF) as u32;
            let sx1 = (sx0 + 1).min(iw - 1);

            // Bilinear taps in premultiplied space: avoids halos when the icon
            // has transparent edges and lets the composite stay integer-only.
            let wxa = 65536u64 - wx0 as u64;
            let wxb = wx0 as u64;
            let t00 = wa * wxa;
            let t01 = wa * wxb;
            let t10 = wb * wxa;
            let t11 = wb * wxb;

            let o00 = y_base_a + sx0 * 4;
            let o01 = y_base_a + sx1 * 4;
            let o10 = y_base_b + sx0 * 4;
            let o11 = y_base_b + sx1 * 4;

            let a00 = src[o00 + 3] as u64;
            let a01 = src[o01 + 3] as u64;
            let a10 = src[o10 + 3] as u64;
            let a11 = src[o11 + 3] as u64;

            let a_acc = a00 * t00 + a01 * t01 + a10 * t10 + a11 * t11;
            // Taps are weighted in 2-D, so the weights sum to 2^32.
            let alpha = ((a_acc + (1 << 31)) >> 32).min(255) as u32;
            if alpha == 0 {
                continue;
            }
            // One reciprocal instead of three divisions.
            let inv_a = 1.0 / a_acc as f32;
            let dst = buf[row + cx as usize];
            let inv = 255 - alpha;
            // The source is straight alpha, so each tap is premultiplied before
            // it is weighted; the sum is un-premultiplied again below.
            let blend = |c: u64, bg: u32| -> u32 {
                let pm = (c as f32 * inv_a) as u32;
                ((pm.min(255) * alpha + bg * inv + 127) / 255).min(255)
            };
            let premul = |ch: usize| -> u64 {
                src[o00 + ch] as u64 * a00 * t00
                    + src[o01 + ch] as u64 * a01 * t01
                    + src[o10 + ch] as u64 * a10 * t10
                    + src[o11 + ch] as u64 * a11 * t11
            };
            buf[row + cx as usize] = (0xFF << 24)
                | (blend(premul(0), (dst >> 16) & 0xFF) << 16)
                | (blend(premul(1), (dst >> 8) & 0xFF) << 8)
                | blend(premul(2), dst & 0xFF);
        }
    }
}

/// Test-only wrapper over [`draw_icon_bitmap_i32`] with integer geometry.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn draw_icon_bitmap(
    buf: &mut [u32],
    stride: usize,
    fw: usize,
    fh: usize,
    x: usize,
    y: usize,
    tw: usize,
    th: usize,
    radius: usize,
    img: &RgbaImage,
) {
    draw_icon_bitmap_i32(buf, stride, fw, fh, x as i32, y as i32, tw, th, radius, img);
}

#[inline(always)]
fn blend_alpha(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let inv = 255 - a;
    let fr = (fg >> 16) & 0xFF;
    let fg_ = (fg >> 8) & 0xFF;
    let fb = fg & 0xFF;
    let br = (bg >> 16) & 0xFF;
    let bg_ = (bg >> 8) & 0xFF;
    let bb = bg & 0xFF;
    let r = (fr * a + br * inv + 127) / 255;
    let g = (fg_ * a + bg_ * inv + 127) / 255;
    let b = (fb * a + bb * inv + 127) / 255;
    (0xFF << 24) | (r << 16) | (g << 8) | b
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
    let alpha = (color >> 24) & 0xFF;
    if alpha == 0 { return; }

    for cy in y..y_end {
        let row = cy * stride;
        if alpha == 255 {
            // One bounds check per row, then a vectorised fill.
            buf[row + x..row + x_end].fill(color);
        } else {
            for cx in x..x_end {
                buf[row + cx] = blend_alpha(buf[row + cx], color, alpha as u8);
            }
        }
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn draw_rounded_rect_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    rw: usize,
    rh: usize,
    radius: usize,
    color: u32,
) {
    if x + rw as i32 <= 0 || x >= w as i32 || y + rh as i32 <= 0 || y >= h as i32 {
        return;
    }
    // Clamp radius to half of the smallest dimension: prevents integer
    // underflow in `x + rw - radius` for small widgets (nav pill, dots).
    let radius = radius.min(rw / 2).min(rh / 2);
    let r2 = (radius * radius) as i32;
    let x_start = x.max(0) as usize;
    let y_start = y.max(0) as usize;
    let x_end = ((x + rw as i32) as usize).min(w);
    let y_end = ((y + rh as i32) as usize).min(h);
    let alpha = (color >> 24) & 0xFF;
    if alpha == 0 { return; }

    for cy in y_start..y_end {
        let (mut lo, mut hi) = match rounded_span(
            cy as i32 - y,
            x,
            rw as i32,
            rh as i32,
            radius as i32,
            r2,
        ) {
            Some(span) => span,
            None => continue,
        };
        lo = lo.max(x_start as i32);
        hi = hi.min(x_end as i32);
        if hi <= lo {
            continue;
        }
        let row = cy * stride;
        if alpha == 255 {
            // Opaque interior: a vectorised span fill, no per-pixel corner test.
            buf[row + lo as usize..row + hi as usize].fill(color);
        } else {
            for cx in lo..hi {
                buf[row + cx as usize] = blend_alpha(buf[row + cx as usize], color, alpha as u8);
            }
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
    draw_rounded_rect_i32(buf, stride, w, h, x as i32, y as i32, rw, rh, radius, color);
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
    if radius == 0 {
        return;
    }
    let r_min_y = cy.saturating_sub(radius);
    let r_max_y = (cy + radius).min(h);
    let rad_sq = (radius as i64) * (radius as i64);
    let dxc = cx as i32;
    let dyc = cy as i32;
    // Integer falloff. `inv_rsq` is 2^32/r^2, so the per-pixel weight is one
    // 64-bit multiply and a shift: no softfloat, no divide in the inner loop.
    let inv_rsq = (1u64 << 32) / rad_sq as u64;
    let inten = intensity as u64;

    for y in r_min_y..r_max_y {
        let dy = y as i32 - dyc;
        let dy2 = (dy as i64) * (dy as i64);
        if dy2 > rad_sq {
            continue;
        }
        // Half-chord of the circle on this row, so the inner loop only walks
        // the disc instead of its bounding box.
        let half = isqrt((rad_sq - dy2) as i32);
        let x0 = (dxc - half).max(0) as usize;
        let x1 = (dxc + half + 1).min(w as i32) as usize;
        let row = y * stride;
        // d2 walks the squared distance incrementally: d2 += step; step += 2.
        let dx0 = x0 as i32 - dxc;
        let mut d2 = dx0 as i64 * dx0 as i64 + dy2;
        let mut step = 2 * dx0 as i64 + 1;
        for x in x0..x1 {
            let a = if d2 >= rad_sq {
                0u32
            } else {
                (((rad_sq - d2) as u64 * inten * inv_rsq) >> 32).min(255) as u32
            };
            if a != 0 {
                let cur = buf[row + x];
                let cb = (cur & 0xFF) + ((b as u32 * a) >> 8);
                let cg = ((cur >> 8) & 0xFF) + ((g as u32 * a) >> 8);
                let cr = ((cur >> 16) & 0xFF) + ((r as u32 * a) >> 8);
                buf[row + x] = (0xFF << 24)
                    | (cr.min(255) << 16)
                    | (cg.min(255) << 8)
                    | cb.min(255);
            }
            d2 += step;
            step += 2;
        }
    }

}






// ---------------------------------------------------------------------------
// Typography
//
// All text is rasterised by the vector engine in `super::font`: real outlines,
// proportional advances, a Material 3 weight axis and analytic anti-aliasing.
// `scale` keeps the historical UI scale semantics (1 = caption, 2 = body,
// 3 = display) and is multiplied by the panel's density, so the same call site
// produces the same optical size on every display.
// ---------------------------------------------------------------------------

/// Typography font weight for the Lawnchair 17 / Material Design 3 type scale.
pub use super::font::FontWeight;

/// Em size in pixels for a UI scale factor on a `panel_w`-wide panel.
#[inline]
pub fn text_size(scale: usize, panel_w: usize) -> f32 {
    super::font::em_px_at(scale, panel_w)
}

/// Exact rendered width of a text string at a UI scale, in pixels (rounded).
///
/// Measured on the reference panel; multiply by the panel's density factor
/// when a pixel-exact position is needed, or use the vector engine directly.
pub fn text_width(text: &str, scale: usize) -> usize {
    super::font::measure(text, super::font::em_px(scale)).round() as usize
}

/// [`text_width`] for a specific panel width.
#[inline]
pub fn text_width_at(text: &str, scale: usize, panel_w: usize) -> usize {
    super::font::measure(text, super::font::em_px_at(scale, panel_w)).round() as usize
}

/// Zero-allocation, stack-only formatter for app count strings (e.g., "15 APPS").
pub fn format_apps_count(buf: &mut [u8; 16], count: usize) -> &str {
    let mut num = count;
    let mut digits = [0u8; 10];
    let mut len = 0;
    if num == 0 {
        digits[0] = b'0';
        len = 1;
    } else {
        while num > 0 && len < 10 {
            digits[len] = b'0' + (num % 10) as u8;
            num /= 10;
            len += 1;
        }
    }
    let mut out_len = 0;
    for i in (0..len).rev() {
        buf[out_len] = digits[i];
        out_len += 1;
    }
    for &b in b" APPS" {
        buf[out_len] = b;
        out_len += 1;
    }
    std::str::from_utf8(&buf[..out_len]).unwrap_or("0 APPS")
}

/// Draw a text run with its ascender line at `y` and left edge at `x`.
#[inline]
pub fn draw_text_weighted_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    if x >= w as i32 || y >= h as i32 {
        return;
    }
    super::font::draw_run(
        buf,
        stride,
        w,
        h,
        x as f32,
        y as f32,
        text,
        color,
        super::font::em_px_at(scale, w),
        weight,
    );
}

/// Draw a text run with its ascender line at `y` and left edge at `x`.
#[inline]
pub fn draw_text_weighted(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    draw_text_weighted_i32(buf, stride, w, h, x as i32, y as i32, text, color, scale, weight);
}

/// Draw a text run horizontally centred on `center_x`, ascender line at `y`.
#[inline]
pub fn draw_text_centered_weighted_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: i32,
    y: i32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    let size = super::font::em_px_at(scale, w);
    let x = center_x as f32 - super::font::measure(text, size) * 0.5;
    super::font::draw_run(buf, stride, w, h, x, y as f32, text, color, size, weight);
}

/// Draw a text run horizontally centred on `center_x`, ascender line at `y`.
#[inline]
pub fn draw_text_centered_weighted(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: usize,
    y: usize,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    draw_text_centered_weighted_i32(buf, stride, w, h, center_x as i32, y as i32, text, color, scale, weight);
}

/// Draw a regular weight text run with its ascender line at `y`.
#[inline]
pub fn draw_text_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    scale: usize,
) {
    draw_text_weighted_i32(buf, stride, w, h, x, y, text, color, scale, FontWeight::Regular);
}

/// Draw a regular weight text run with its ascender line at `y`.
#[inline]
pub fn draw_text(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    text: &str,
    color: u32,
    scale: usize,
) {
    draw_text_weighted(buf, stride, w, h, x, y, text, color, scale, FontWeight::Regular);
}

/// Draw a regular weight text run centred on `center_x`, ascender line at `y`.
#[inline]
pub fn draw_text_centered_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: i32,
    y: i32,
    text: &str,
    color: u32,
    scale: usize,
) {
    draw_text_centered_weighted_i32(buf, stride, w, h, center_x, y, text, color, scale, FontWeight::Regular);
}

/// Draw a regular weight text run centred on `center_x`, ascender line at `y`.
#[inline]
pub fn draw_text_centered(
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
    draw_text_centered_weighted(buf, stride, w, h, center_x, y, text, color, scale, FontWeight::Regular);
}

/// Draw a text run truncated with an ellipsis so it never leaves `max_width`.
///
/// Truncation is measured on the vector engine's own advances, so the drawn
/// width and the measured width agree and no glyph is ever cut in half.
pub fn draw_text_clipped(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    max_width: f32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    if max_width <= 0.0 {
        return;
    }
    let size = super::font::em_px_at(scale, w);
    let full = super::font::measure(text, size);
    if full <= max_width {
        super::font::draw_run(buf, stride, w, h, x, y, text, color, size, weight);
        return;
    }
    // Reserve room for the ellipsis, then take the longest prefix that fits.
    let ell = "..";
    let ell_w = super::font::measure(ell, size);
    let budget = max_width - ell_w;
    if budget <= 0.0 {
        return;
    }
    let mut used = 0.0f32;
    let mut end = 0;
    for (i, b) in text.bytes().enumerate() {
        let adv = super::font::char_advance(b, size);
        if used + adv > budget {
            break;
        }
        used += adv;
        end = i + 1;
        // The walk is byte-wise (char_advance takes a u8) but the slice must
        // land on a char boundary, or a split multi-byte char panics.
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
    }
    if end == 0 {
        return;
    }
    let head = &text[..end];
    super::font::draw_run(buf, stride, w, h, x, y, head, color, size, weight);
    super::font::draw_run(buf, stride, w, h, x + used, y, ell, color, size, weight);
}

/// [`draw_text_clipped`] with an integer origin.
#[inline]
pub fn draw_text_clipped_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: f32,
    max_width: f32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    draw_text_clipped(buf, stride, w, h, x as f32, y, max_width, text, color, scale, weight);
}

/// [`draw_text_clipped`] centred on `center_x` and truncated to `max_width`.
pub fn draw_text_centered_clipped_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: i32,
    y: f32,
    max_width: f32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    if max_width <= 0.0 {
        return;
    }
    let size = super::font::em_px_at(scale, w);
    let full = super::font::measure(text, size);
    let width = if full <= max_width {
        full
    } else {
        // Match draw_text_clipped's prefix walk so the centre matches what
        // is actually drawn instead of max_width.
        let ell_w = super::font::measure("..", size);
        let budget = max_width - ell_w;
        if budget <= 0.0 {
            return;
        }
        let mut used = 0.0f32;
        let mut end = 0usize;
        for (i, b) in text.bytes().enumerate() {
            let adv = super::font::char_advance(b, size);
            if used + adv > budget {
                break;
            }
            used += adv;
            end = i + 1;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
        }
        if end == 0 {
            return;
        }
        used + ell_w
    };
    draw_text_clipped(
        buf, stride, w, h, center_x as f32 - width * 0.5, y, max_width, text, color, scale, weight,
    );
}

/// [`draw_text_centered_clipped_i32`] with an integer centre.
#[inline]
pub fn draw_text_centered_clipped(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: usize,
    y: usize,
    max_width: f32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    draw_text_centered_clipped_i32(
        buf, stride, w, h, center_x as i32, y as f32, max_width, text, color, scale, weight,
    );
}

/// Test probe: draw a rounded rect with integer geometry.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_rect_probe(
    buf: &mut [u32],
    stride: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    radius: usize,
    color: u32,
) {
    draw_rounded_rect(
        buf,
        stride,
        stride,
        (stride * 16).min(4096),
        x,
        y,
        rw,
        rh,
        radius,
        color,
    );
}

/// Rounded rectangle from float geometry.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn draw_rounded_rect_f(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    rw: f32,
    rh: f32,
    radius: f32,
    color: u32,
) {
    let xi = x.floor().max(0.0) as i32;
    let yi = y.floor().max(0.0) as i32;
    let x1 = (x + rw).ceil().min(w as f32) as i32;
    let y1 = (y + rh).ceil().min(h as f32) as i32;
    draw_rounded_rect_i32(
        buf, stride, w, h, xi, yi, (x1 - xi).max(0) as usize, (y1 - yi).max(0) as usize,
        radius.max(0.0).min((x1 - xi) as f32).min((y1 - yi) as f32) as usize, color,
    );
}

/// Filled rectangle from float geometry.
#[inline]
pub fn draw_rect_f(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    rw: f32,
    rh: f32,
    color: u32,
) {
    let xi = x.floor().max(0.0) as i32;
    let yi = y.floor().max(0.0) as i32;
    let x1 = (x + rw).ceil().min(w as f32) as i32;
    let y1 = (y + rh).ceil().min(h as f32) as i32;
    if x1 <= xi || y1 <= yi {
        return;
    }
    draw_rect(
        buf, stride, w, h, xi as usize, yi as usize, (x1 - xi) as usize, (y1 - yi) as usize, color,
    );
}

/// A line of the given stroke width, drawn as a rotated capsule.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn draw_line(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: u32,
) {
    // Axis-aligned runs are the common case (glyph strokes, dividers).
    if (x0 - x1).abs() < 0.5 {
        draw_rect_f(
            buf, stride, w, h, x0 - 0.5, y0.min(y1), 1.0, (y1 - y0).abs() + 1.0, color,
        );
        return;
    }
    if (y0 - y1).abs() < 0.5 {
        draw_rect_f(
            buf, stride, w, h, x0.min(x1), y0 - 0.5, (x1 - x0).abs() + 1.0, 1.0, color,
        );
        return;
    }
    // Diagonals: integer Bresenham DDA from rounded endpoints. One add per
    // step, no float divide in the loop, no per-pixel draw_rect_f call.
    let mut x = x0.round() as i32;
    let mut y = y0.round() as i32;
    let x1i = x1.round() as i32;
    let y1i = y1.round() as i32;
    let dx = (x1i - x).abs();
    let dy = -((y1i - y).abs());
    let sx = if x < x1i { 1 } else { -1 };
    let sy = if y < y1i { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put_px(buf, stride, w, h, x, y, color);
        if x == x1i && y == y1i {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

#[inline]
fn put_px(buf: &mut [u32], stride: usize, w: usize, h: usize, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let idx = y as usize * stride + x as usize;
    if idx >= buf.len() {
        return;
    }
    let a = (color >> 24) & 0xFF;
    if a == 255 {
        buf[idx] = color;
    } else if a != 0 {
        buf[idx] = blend_alpha(buf[idx], color, a as u8);
    }
}

/// Horizontal span of a rounded rectangle on scanline `cy`, as `[lo, hi)`.
///
/// Computing the span once per row (one integer square root) instead of a
/// corner test per pixel is what keeps a launcher full of icon tiles inside
/// the frame budget. Returns `None` for rows the shape does not cover.
#[inline]
fn rounded_span(
    cy: i32,
    x: i32,
    rw: i32,
    rh: i32,
    radius: i32,
    r2: i32,
) -> Option<(i32, i32)> {
    // `rh`, not `rw`: a rect taller than it is wide must still have its lower
    // rows filled, which a shared extent silently dropped.
    let dy = if cy < radius {
        radius - cy
    } else if cy >= rh - radius {
        cy - (rh - radius)
    } else {
        0
    };
    if dy * dy > r2 {
        return None;
    }
    let d = if dy == 0 {
        radius
    } else {
        isqrt(r2 - dy * dy)
    };
    Some((radius - d + x, rw - radius + d + x))
}

/// Integer square root, floor. Newton with an integer seed.
#[inline]
fn isqrt(v: i32) -> i32 {
    if v <= 0 {
        return 0;
    }
    let mut x = (v as f64).sqrt() as i32;
    if x < 1 {
        x = 1;
    }
    // One or two correction steps; the float seed is within one.
    while x * x > v && x > 1 {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= v {
        x += 1;
    }
    x
}

/// A filled disc, used for dots, close affordances and ripple centres.
#[inline]
pub fn draw_circle_glyph(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    cx: f32,
    cy: f32,
    r: f32,
    color: u32,
) {
    if r <= 0.0 {
        return;
    }
    let y0 = (cy - r).floor().max(0.0) as i32;
    let y1 = (cy + r).ceil().min(h as f32) as i32;
    let x0 = (cx - r).floor().max(0.0) as i32;
    let x1 = (cx + r).ceil().min(w as f32) as i32;
    // Outer coverage radius: a pixel carries ink iff d < r + 0.5.
    // Per-row half-chords clip the inner loop to the disc instead of the
    // bounding box, and the fully-covered interior skips sqrt entirely.
    let rout = r + 0.5;
    let rin = r - 0.5;
    for py in y0..y1 {
        let dy = py as f32 + 0.5 - cy;
        let half2 = rout * rout - dy * dy;
        if half2 <= 0.0 {
            continue;
        }
        let half = half2.sqrt();
        let mut lo = (cx - half - 0.5).ceil() as i32;
        let mut hi = (cx + half - 0.5).floor() as i32 + 1;
        lo = lo.max(x0);
        hi = hi.min(x1);
        if hi <= lo {
            continue;
        }
        // Fully opaque interior: d <= r - 0.5 implies a == 1.
        if rin > 0.0 && dy.abs() <= rin {
            let half_in = (rin * rin - dy * dy).sqrt();
            let loi = (cx - half_in - 0.5).ceil() as i32;
            let hii = (cx + half_in - 0.5).floor() as i32 + 1;
            let ilo = loi.max(lo);
            let ihi = hii.min(hi);
            for px in ilo..ihi {
                let i = py as usize * stride + px as usize;
                buf[i] = super::font::blend_over(buf[i], color, 255);
            }
            // Edge band on both sides still needs analytic coverage.
            for px in lo..ilo {
                let dx = px as f32 + 0.5 - cx;
                let d = (dx * dx + dy * dy).sqrt();
                let a = (r + 0.5 - d).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                let i = py as usize * stride + px as usize;
                buf[i] = super::font::blend_over(buf[i], color, (a * 255.0 + 0.5) as u8);
            }
            for px in ihi..hi {
                let dx = px as f32 + 0.5 - cx;
                let d = (dx * dx + dy * dy).sqrt();
                let a = (r + 0.5 - d).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                let i = py as usize * stride + px as usize;
                buf[i] = super::font::blend_over(buf[i], color, (a * 255.0 + 0.5) as u8);
            }
        } else {
            for px in lo..hi {
                let dx = px as f32 + 0.5 - cx;
                let d = (dx * dx + dy * dy).sqrt();
                let a = (r + 0.5 - d).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                let i = py as usize * stride + px as usize;
                buf[i] = super::font::blend_over(buf[i], color, (a * 255.0 + 0.5) as u8);
            }
        }
    }
}

/// Split a packed RGB triple out of an ARGB colour.
#[inline]
fn rgb_of(c: u32) -> (u8, u8, u8) {
    (((c >> 16) & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, (c & 0xFF) as u8)
}

/// Width the display clock occupies, matching [`draw_material_you_clock`].
fn clock_run_width(time_str: &str, digit_h: f32, panel_w: usize) -> f32 {
    use super::font::{em_px_at, measure, CAP_HEIGHT};
    let k = (panel_w as f32 / super::font::REFERENCE_PANEL_W).clamp(0.5, 2.0);
    let size = (digit_h * k / (CAP_HEIGHT / 1000.0)).min(em_px_at(4, panel_w));
    let tracking = size * 0.04;
    measure(time_str, size) + tracking * (time_str.len() as f32 - 1.0).max(0.0)
}

/// A padlock: shackle arc over a body, sized from `size` (the body height).
#[allow(clippy::too_many_arguments)]
fn draw_lock_glyph(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    cx: usize,
    cy: usize,
    size: f32,
    color: u32,
) {
    if size <= 0.0 {
        return;
    }
    let body_w = size * 0.86;
    let body_h = size;
    let body_y = cy as f32 + size * 0.30;
    // Shackle: three sides of a rounded rect sitting on the body.
    let sh_w = body_w * 0.58;
    let sh_h = size * 0.62;
    let sh_x = cx as f32 - sh_w * 0.5;
    let sh_y = cy as f32;
    let t = size * 0.15;
    draw_rounded_rect_f(
        buf, stride, w, h, sh_x, sh_y, sh_w, sh_h, sh_w * 0.5, color,
    );
    // Punch the middle of the shackle out again.
    draw_rounded_rect_f(
        buf, stride, w, h, sh_x + t, sh_y + t, sh_w - t * 2.0, sh_h, sh_w * 0.4,
        state_surface_dark(),
    );
    draw_rounded_rect_f(
        buf, stride, w, h, cx as f32 - body_w * 0.5, body_y, body_w, body_h, size * 0.22, color,
    );
}

/// Background colour to punch through to when drawing the lock shackle.
#[inline]
fn state_surface_dark() -> u32 {
    0xFF0B0F19
}

/// ASCII upper case rendered into a caller-supplied buffer, so the shift
/// legends stay on the stack like the rest of the keyboard path.
#[inline]
fn legend_label(buf: &mut [u8; 4], c: char) -> &str {
    let mut tmp = [0u8; 4];
    let n = c.encode_utf8(&mut tmp).len();
    tmp[..n].make_ascii_uppercase();
    buf[..n].copy_from_slice(&tmp[..n]);
    std::str::from_utf8(&buf[..n]).unwrap_or("?")
}

/// Stack-only ASCII case-insensitive substring search, so per-frame Settings
/// filtering allocates nothing (the card strings are fixed ASCII).
#[inline]
fn ascii_contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    for i in 0..=(h.len() - n.len()) {
        let mut ok = true;
        for j in 0..n.len() {
            let mut a = h[i + j];
            let mut b = n[j];
            if a.is_ascii_uppercase() {
                a += 32;
            }
            if b.is_ascii_uppercase() {
                b += 32;
            }
            if a != b {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

/// Draw a search field at the top of an app's content card.
///
/// Returns the y at which a row list can start and the height available to it.
#[allow(clippy::too_many_arguments)]
fn draw_app_search_field(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    bar: &super::layout::Rect,
    content_y: f32,
    al: AppLayout,
    state: &DrmInteractiveState,
    placeholder: &str,
) -> (f32, f32) {
    // Field height is the standard touch target; the gutter keeps it clear of
    // the app bar above and the list below.
    let f_h = al.input.h;
    let inset = f_h * 0.30;
    let f_x = bar.x + inset;
    let f_w = (bar.w - inset * 2.0).max(0.0);
    let f_y = content_y + inset;
    let ring = (w as f32 * 0.003).max(1.0);
    let border = if state.app_input_focused {
        state.palette.primary
    } else {
        state.palette.outline
    };
    draw_rounded_rect_f(
        buf, stride, w, h, f_x - ring, f_y - ring, f_w + ring * 2.0, f_h + ring * 2.0,
        f_h * 0.28 + ring, border,
    );
    draw_rounded_rect_f(
        buf, stride, w, h, f_x, f_y, f_w, f_h, f_h * 0.28, state.palette.surface_container_high,
    );
    let em = super::font::em_px_at(2, w);
    let disp = if state.app_input.is_empty() { placeholder } else { state.app_input };
    let col = if state.app_input.is_empty() {
        state.palette.on_surface_variant
    } else {
        state.palette.on_surface
    };
    let tx = f_x + f_h * 0.42;
    let ty = f_y + (f_h - em * 0.62) * 0.5 - em * 0.14;
    draw_text_clipped(
        buf, stride, w, h, tx, ty, f_w - f_h * 0.8, disp, col, 2, FontWeight::Regular,
    );
    if state.app_input_focused {
        let cur_x = tx + super::font::measure(disp, em) + ring;
        if cur_x < f_x + f_w - f_h * 0.3 {
            draw_rect(
                buf, stride, w, h, cur_x as usize, ty as usize, ring.max(2.0) as usize,
                (em * 0.62) as usize, state.palette.primary,
            );
        }
    }
    let list_top = f_y + f_h + inset;
    let list_h = (content_y + al.scroll_bottom - list_top).max(0.0);
    (list_top, list_h)
}

/// Draw a two-line row list inside an app content card.
fn draw_app_row_list(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    bar: &super::layout::Rect,
    top: f32,
    avail_h: f32,
    rows: &[(&str, &str)],
    state: &DrmInteractiveState,
) {
    let row_h = (h as f32 * 0.026).max(w.min(h) as f32 * 0.058);
    let step = row_h + h as f32 * 0.006;
    let x = bar.x + bar.h * 0.25;
    let rw = bar.w - bar.h * 0.5;
    let title_em = super::font::em_px_at(1, w);
    let mut y = top;
    for (name, detail) in rows {
        if y + row_h > top + avail_h {
            break;
        }
        draw_rounded_rect_f(
            buf, stride, w, h, x, y, rw, row_h, row_h * 0.24, state.palette.surface_container_high,
        );
        let tx = x + row_h * 0.45;
        let tw = rw - row_h * 0.9;
        draw_text_clipped(
            buf, stride, w, h, tx, y + row_h * 0.16, tw, name, state.palette.on_surface, 1,
            FontWeight::Bold,
        );
        draw_text_clipped(
            buf, stride, w, h, tx, y + row_h * 0.16 + title_em, tw, detail,
            state.palette.on_surface_variant, 1, FontWeight::Regular,
        );
        y += step;
    }
}

/// The launcher background, palette driven.
fn draw_background(buf: &mut [u32], stride: usize, w: usize, h: usize, state: &DrmInteractiveState) {
    let base = state.palette.surface;
    let sr = ((base >> 16) & 0xFF) as f32;
    let sg = ((base >> 8) & 0xFF) as f32;
    let sb = (base & 0xFF) as f32;
    for y in 0..h {
        let t = y as f32 / h as f32;
        // Darken toward the bottom so the dock and grid read as "on top".
        let k = 1.0 - t * 0.42;
        let r = ((sr * k).round() as u32).min(255);
        let g = ((sg * k).round() as u32).min(255);
        let b = ((sb * k).round() as u32).min(255);
        // A slice fill is a vectorised store; the manual loop was costing
        // milliseconds on a 2.6 Mpx panel.
        buf[y * stride..y * stride + w].fill((0xFF << 24) | (r << 16) | (g << 8) | b);
    }
    // Two soft accent glows, from the palette's primary and tertiary.
    let (pr, pg, pb) = rgb_of(state.palette.primary);
    let (tr, tg, tb) = rgb_of(state.palette.tertiary);
    draw_glow_circle(buf, stride, w, h, w * 8 / 10, h / 8, w * 5 / 18, pr, pg, pb, 26);
    draw_glow_circle(buf, stride, w, h, w * 2 / 10, h * 8 / 10, w * 6 / 18, tr, tg, tb, 20);
}

/// The system status bar: clock on the left, radios and battery on the right.
fn draw_status_bar(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
    y_offset: i32,
) {
    let l = Layout::plain(w as f32, h as f32);
    let bar_h = l.status_bar_h;
    let em = super::font::em_px_at(1, w);
    let y = (l.status_bar_h * 0.5 - em * 0.34) as i32 + y_offset;
    let pad = l.w * PANEL_PAD_FRACTION;
    draw_text_weighted_i32(
        buf, stride, w, h, pad as i32, y, state.time_str, state.palette.on_surface, 1,
        FontWeight::Medium,
    );
    let icon_r = l.w - pad;
    let by = l.status_bar_h * 0.5 - bar_h * 0.28 + y_offset as f32;
    draw_rounded_rect_f(
        buf, stride, w, h, icon_r - bar_h * 1.35, by, bar_h * 1.05, bar_h * 0.56,
        bar_h * 0.16, state.palette.on_surface_variant,
    );
    draw_rounded_rect_f(
        buf, stride, w, h, icon_r - bar_h * 1.23, by + bar_h * 0.09, bar_h * 0.81, bar_h * 0.38,
        bar_h * 0.10, state.palette.surface,
    );
    // Fill level inside the battery.
    draw_rounded_rect_f(
        buf, stride, w, h, icon_r - bar_h * 1.23, by + bar_h * 0.09, bar_h * 0.81 * 0.78,
        bar_h * 0.38, bar_h * 0.10, 0xFF10B981,
    );
    if state.quick_tiles_active[0] {
        draw_circle_glyph(
            buf, stride, w, h, icon_r - bar_h * 2.05, l.status_bar_h * 0.5 + y_offset as f32,
            bar_h * 0.14, state.palette.on_surface,
        );
    }
    let rat = if state.quick_tiles_active[1] { "5G" } else { "OFF" };
    let rat_col = if state.quick_tiles_active[1] {
        state.palette.primary
    } else {
        state.palette.outline
    };
    draw_text_weighted_i32(
        buf, stride, w, h, (icon_r - bar_h * 2.9) as i32, y, rat, rat_col, 1, FontWeight::Bold,
    );
}

/// Tone of an edit-mode chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChipTone {
    Destructive,
    Primary,
}

/// Lawnchair 17 / Android DynamicAnimation Spring Physics Configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringConfig {
    pub stiffness: f32,
    pub damping_ratio: f32,
    pub value_threshold: f32,
}

impl SpringConfig {
    pub const STIFFNESS_HIGH: f32 = 10000.0;
    pub const STIFFNESS_MEDIUM: f32 = 1500.0;
    pub const STIFFNESS_LOW: f32 = 200.0;
    pub const STIFFNESS_VERY_LOW: f32 = 50.0;

    pub const DAMPING_RATIO_HIGH_BOUNCY: f32 = 0.2;
    pub const DAMPING_RATIO_MEDIUM_BOUNCY: f32 = 0.5;
    pub const DAMPING_RATIO_LOW_BOUNCY: f32 = 0.75;
    pub const DAMPING_RATIO_NO_BOUNCY: f32 = 1.0;

    /// App drawer open/close drag and fling spring configuration.
    ///
    /// Legacy hand-tuned profile, kept verbatim because `utlc` links against
    /// it; it is *not* one of the sourced Lawnchair rows below.
    pub fn drawer() -> Self {
        Self {
            stiffness: 300.0,
            damping_ratio: 0.85,
            value_threshold: 0.001,
        }
    }

    /// Home screen page swipe & boundary overscroll spring configuration.
    ///
    /// Legacy hand-tuned profile, kept verbatim; not a sourced row.
    pub fn page_swipe() -> Self {
        Self {
            stiffness: 280.0,
            damping_ratio: 0.75,
            value_threshold: 0.5,
        }
    }

    /// Touch press bounce compression & release spring configuration.
    ///
    /// Legacy hand-tuned profile, kept verbatim; not a sourced row.
    pub fn icon_bounce() -> Self {
        Self {
            stiffness: 450.0,
            damping_ratio: Self::DAMPING_RATIO_MEDIUM_BOUNCY, // Bouncy tactile rebound (0.92x -> 1.013x -> 1.0x)
            value_threshold: 0.002,
        }
    }

    /// App launch expansion animation spring configuration.
    ///
    /// Legacy hand-tuned profile, kept verbatim; not a sourced row.
    pub fn app_launch() -> Self {
        Self {
            stiffness: 320.0,
            damping_ratio: 0.90, // Smooth expansion from icon to fullscreen
            value_threshold: 0.001,
        }
    }

    // ---------------------------------------------------------------------
    // Sourced Lawnchair 17 / AndroidX rows.
    //
    // Every constant below is transcribed from the AOSP/Lawnchair source named
    // in the doc comment; nothing here is hand-tuned. `value_threshold` is the
    // launcher's own rest threshold in the units the row animates (0.002 for
    // scale-space springs, 0.5 px for pixel-space springs).
    // ---------------------------------------------------------------------

    /// Unspecialised rebound: `SpringForce.STIFFNESS_MEDIUM` (1500) with
    /// `SpringForce.DAMPING_RATIO_MEDIUM_BOUNCY` (0.5) -- the global defaults
    /// (`SpringForce.java:44,64`), which is what Launcher3 hands every spring
    /// that does not name its own force.
    pub const fn icon_rebound() -> Self {
        Self {
            stiffness: 1500.0,
            damping_ratio: 0.5,
            value_threshold: 0.002,
        }
    }

    /// Folder open/close transform: `FolderSpringAnimatorSet.kt:52-53`.
    pub const fn folder_morph() -> Self {
        Self {
            stiffness: 380.0,
            damping_ratio: 0.8,
            value_threshold: 0.002,
        }
    }

    /// Folder scrim dim: `FolderSpringAnimatorSet.kt:56-57` and the
    /// `EdgeEffect`/scrim springs it builds at `:340-378`.
    pub const fn folder_scrim() -> Self {
        Self {
            stiffness: 380.0,
            damping_ratio: 0.98,
            value_threshold: 0.002,
        }
    }

    /// Folder container alpha: `FolderSpringAnimatorSet.kt:54-55`, applied to
    /// the alpha property at `:262-290`.
    pub const fn folder_alpha() -> Self {
        Self {
            stiffness: 1600.0,
            damping_ratio: 0.9,
            value_threshold: 0.002,
        }
    }

    /// Launcher drawer reveal: `config.xml:116-117`
    /// (`config_dragSpringAnimationStiffness` / `...DampingRatio`).
    pub const fn drawer_reveal() -> Self {
        Self {
            stiffness: 150.0,
            damping_ratio: 0.7,
            value_threshold: 0.002,
        }
    }

    /// Recents task dismiss: `quickstep/res/values/config.xml:65-66`
    /// (`config_taskDismissSpringAnimationStiffness` / `...DampingRatio`).
    pub const fn task_dismiss() -> Self {
        Self {
            stiffness: 850.0,
            damping_ratio: 0.65,
            value_threshold: 0.5,
        }
    }

    /// Vertical neighbour settle. `RecentsDismissUtils.kt:1382` adds 0.15 to the
    /// damping ratio for every further neighbour, so a stack of 5 settles
    /// progressively softer and does not all snap on the same frame.
    /// Clamped at 1.0 -- `zeta >= 1` is critically damped or worse, and
    /// Android's `SpringAnimationBuilder` rejects `zeta >= 1` outright
    /// (`:107-109`), so an unclamped profile would never animate there.
    pub const fn task_dismiss_with_hops(hops: u8) -> Self {
        let base = Self::task_dismiss();
        // Written as a branch rather than `.min(1.0)`: `f32::min` is not const.
        let zeta = base.damping_ratio + hops as f32 * 0.15;
        Self {
            damping_ratio: if zeta > 1.0 { 1.0 } else { zeta },
            ..base
        }
    }

    /// Recents grid reflow: `quickstep/res/values/config.xml:67-68`
    /// (`config_taskReflowSpringAnimationStiffness` / `...DampingRatio`).
    pub const fn grid_reflow() -> Self {
        Self {
            stiffness: 2800.0,
            damping_ratio: 0.8,
            value_threshold: 0.5,
        }
    }

    /// Task card lift-off ("magnetic detach"): `TaskViewDismissTouchController
    /// .kt:414` builds its spring with these constants.
    pub const fn magnetic_detach() -> Self {
        Self {
            stiffness: 800.0,
            damping_ratio: 0.95,
            value_threshold: 0.5,
        }
    }

    /// Spring-loaded / hint-scale pulse: `config.xml:120-121`
    /// (`config_scaleSpringStiffness` / `config_scaleSpringDampingRatio`).
    pub const fn spring_loaded() -> Self {
        Self {
            stiffness: 200.0,
            damping_ratio: 0.7,
            value_threshold: 0.002,
        }
    }

    /// Recents swipe-up rect scale: `config.xml:86-87`
    /// (`config_swipeUpRectScaleSpringStiffness` / `...DampingRatio`).
    /// Animate with [`SpringSimulation::step_scaled`] -- see
    /// [`RECENTS_SCALE_MULTIPLIER`].
    pub const fn recents_scale() -> Self {
        Self {
            stiffness: 200.0,
            damping_ratio: 0.75,
            value_threshold: 0.002,
        }
    }

    /// Recents dismiss side effects: `quickstep/res/values/config.xml:69-70`
    /// (`config_dismissEffectAnimationStiffness` / `...DampingRatio`).
    /// `zeta == 1.0` is Android's "no bounce" row.
    pub const fn dismiss_effects() -> Self {
        Self {
            stiffness: 1600.0,
            damping_ratio: 1.0,
            value_threshold: 0.002,
        }
    }

    /// Desktop-mode workspace slide: `quickstep/res/values/dimens.xml:601-602`
    /// (`workspace_slide_spring_stiffness` / `..._damping_ratio`).
    pub const fn desktop_slide() -> Self {
        Self {
            stiffness: 380.0,
            damping_ratio: 0.8,
            value_threshold: 0.5,
        }
    }

    /// Icon swipe translation: `IconGestureListener.kt:75-76` builds its
    /// `SpringForce` with `STIFFNESS_HIGH` and `DAMPING_RATIO_NO_BOUNCY`.
    pub const fn icon_swipe_offset() -> Self {
        Self {
            stiffness: 10000.0,
            damping_ratio: 1.0,
            value_threshold: 0.5,
        }
    }

    /// Icon swipe post-fling settle: `IconGestureListener.kt:148-149`, again
    /// `STIFFNESS_HIGH` but with the medium stiffness and no bounce.
    pub const fn icon_swipe_postfling() -> Self {
        Self {
            stiffness: 1500.0,
            damping_ratio: 1.0,
            value_threshold: 0.5,
        }
    }

    /// Recents attach (task-to-container) alpha: `RecentsAtomicAnimationFactory
    /// .java:61-62` sets its `SpringForce` from these two constants.
    pub const fn recents_attach_alpha() -> Self {
        Self {
            stiffness: 250.0,
            damping_ratio: 0.8,
            value_threshold: 0.002,
        }
    }

    /// Taskbar translation: `TaskbarTranslationController.java:112-113`.
    pub const fn taskbar_translation() -> Self {
        Self {
            stiffness: 200.0,
            damping_ratio: 0.5,
            value_threshold: 0.5,
        }
    }

    /// Home screen stretch edge: `StretchEdgeEffect.java:92,97` specifies a
    /// natural *frequency* (omega = 24.657 rad/s), not a stiffness, so
    /// convert with `k = omega^2`: `24.657^2 = 607.967649` rad^2/s^2. Using
    /// 24.657 as a stiffness directly would be a 24.7x stiffer edge.
    /// (`StretchEdgeEffect` ships no explicit rest threshold, so this row uses
    /// the table default of 0.002.)
    pub const fn stretch_edge() -> Self {
        const OMEGA: f32 = 24.657;
        Self {
            stiffness: OMEGA * OMEGA,
            damping_ratio: 0.98,
            value_threshold: 0.002,
        }
    }
}

/// Spring animator state tracking value, velocity, and equilibrium.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringSimulation {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub config: SpringConfig,
}

/// Closed-form damped-oscillator solution, shared by [`SpringSimulation::step`]
/// and [`SpringSimulation::settle_duration`] so the integrator and the
/// duration estimate can never disagree.
///
/// Returns `(displacement, velocity)` at time `t` given the initial
/// `(displacement, velocity)`, all in `f64` so the decay keeps full precision
/// even when the stored state is `f32`. The three regimes mirror Android's
/// `SpringForce.getValue`: overdamped, critically damped, underdamped.
#[inline]
fn spring_response(
    stiffness: f64,
    damping_ratio: f64,
    displacement: f64,
    velocity: f64,
    t: f64,
) -> (f64, f64) {
    let natural_freq = stiffness.sqrt();

    if damping_ratio > 1.0 {
        // Overdamped: sum of two decaying exponentials.
        let gamma_plus = -damping_ratio * natural_freq
            + natural_freq * (damping_ratio * damping_ratio - 1.0).sqrt();
        let gamma_minus =
            -damping_ratio * natural_freq - natural_freq * (damping_ratio * damping_ratio - 1.0).sqrt();
        let coeff_b = (gamma_minus * displacement - velocity) / (gamma_minus - gamma_plus);
        let coeff_a = displacement - coeff_b;
        let d = coeff_a * (gamma_minus * t).exp() + coeff_b * (gamma_plus * t).exp();
        let v = coeff_a * gamma_minus * (gamma_minus * t).exp()
            + coeff_b * gamma_plus * (gamma_plus * t).exp();
        (d, v)
    } else if (damping_ratio - 1.0).abs() < 1e-4 {
        // Critically damped: (A + B t) e^(-w t).
        let coeff_a = displacement;
        let coeff_b = velocity + natural_freq * displacement;
        let decay = (-natural_freq * t).exp();
        let d = (coeff_a + coeff_b * t) * decay;
        let v = (coeff_b - natural_freq * (coeff_a + coeff_b * t)) * decay;
        (d, v)
    } else {
        // Underdamped: decaying sinusoid.
        let damped_freq = natural_freq * (1.0 - damping_ratio * damping_ratio).sqrt();
        let cos_coeff = displacement;
        let sin_coeff = (1.0 / damped_freq) * (damping_ratio * natural_freq * displacement + velocity);
        let decay = (-damping_ratio * natural_freq * t).exp();
        let cos_val = (damped_freq * t).cos();
        let sin_val = (damped_freq * t).sin();
        let d = decay * (cos_coeff * cos_val + sin_coeff * sin_val);
        // d/dt of the line above: -gamma*d + decay*(damped_freq * (sin_coeff *
        // cos_val - cos_coeff * sin_val)).
        let v = d * (-natural_freq * damping_ratio)
            + decay * (-damped_freq * cos_coeff * sin_val + damped_freq * sin_coeff * cos_val);
        (d, v)
    }
}

/// The window of a segment during which a monotone magnitude stays under its
/// rest threshold, resolved to within `tol`.
///
/// While a segment of the response is split at every point where `|d|` or
/// `|v|` can turn, each magnitude is monotone on that segment, so the times it
/// stays under a threshold form one interval. `first` is the earliest point in
/// that interval the search can certify and `last` the latest, each within
/// `tol` of the true boundary -- both are *inside* the interval, so
/// `max(first_d, first_v)` is itself a time the spring is at rest.
#[derive(Debug, Clone, Copy)]
struct RestWindow {
    first: f64,
    last: f64,
}

/// Bisect a monotone magnitude down to `tol`, returning the bracket of the
/// crossing with `q` inside `[lo, hi]`. Uses the standard sign-tracking
/// bisection: `lo` keeps the side it started on, so the bracket is nested
/// across successive halvings and across tolerances.
fn crossing_bracket<F: Fn(f64) -> f64>(
    magnitude: F,
    mut lo: f64,
    mut hi: f64,
    q: f64,
    tol: f64,
) -> (f64, f64) {
    let lo_below = magnitude(lo) <= q;
    for _ in 0..64 {
        if hi - lo <= tol {
            break;
        }
        let mid = 0.5 * (lo + hi);
        if (magnitude(mid) <= q) == lo_below {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, hi)
}

/// Window of `[lo, hi]` where the monotone `magnitude` stays `<= q`.
/// `None` when the whole segment is above `q`.
fn rest_window<F: Fn(f64) -> f64>(
    magnitude: F,
    lo: f64,
    hi: f64,
    q: f64,
    tol: f64,
) -> Option<RestWindow> {
    let a = magnitude(lo);
    let b = magnitude(hi);
    if a <= q && b <= q {
        return Some(RestWindow { first: lo, last: hi });
    }
    if a > q && b > q {
        return None;
    }
    let (x, y) = crossing_bracket(magnitude, lo, hi, q, tol);
    if b >= a {
        // Rising: under `q` from `lo` up to the crossing, so `hi` is past it.
        Some(RestWindow {
            first: lo,
            last: y,
        })
    } else {
        // Falling: under `q` from the crossing up to `hi`, so `lo` is before it.
        Some(RestWindow {
            first: x,
            last: hi,
        })
    }
}

/// Frame period assumed when the caller passes a nonsensical refresh rate.
/// A bad `frame_ms` must never be able to disable parking (that is the bug
/// this whole computation exists to fix), so it degrades to 60 Hz instead of
/// returning `INFINITY`.
const FALLBACK_FRAME_MS: f32 = 16.667;

/// Certification tolerance for the segment walk. Fixed (not derived from
/// `frame_ms`) so that which segment holds the first rest point never depends
/// on the caller's refresh rate -- only the resolution of the reported value
/// does, which is what makes the answer monotonic in `frame_ms`. 1 ns is far
/// below any frame and still ~1e7x above the f64 noise floor of the segment
/// endpoints, so a genuine overlap narrower than this is the only thing the
/// walk can miss, and missing it can only make the answer *later*.
const SETTLE_CERTIFY_S: f64 = 1e-9;

/// Segments walked before giving up, so a pathological config cannot hang the
/// caller. 4096 segments is ~1365 oscillation periods: far more than any real
/// profile needs, since the response is inside its thresholds within a few.
const SETTLE_SEGMENT_CAP: u32 = 4096;

impl SpringSimulation {
    pub fn new(initial_value: f32, target: f32, config: SpringConfig) -> Self {
        Self {
            value: initial_value,
            velocity: 0.0,
            target,
            config,
        }
    }

    pub fn with_velocity(mut self, velocity: f32) -> Self {
        self.velocity = velocity;
        self
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    pub fn is_at_rest(&self) -> bool {
        (self.value - self.target).abs() <= self.config.value_threshold
            && self.velocity.abs() <= (self.config.value_threshold * 50.0)
    }

    /// Step simulation by `dt` seconds using exact analytical damped oscillator dynamics (Android DynamicAnimation).
    /// Returns true when the spring reaches equilibrium at target position.
    pub fn step(&mut self, dt: f32) -> bool {
        if dt <= 0.0 {
            return self.is_at_rest();
        }

        let (new_disp, new_vel) = spring_response(
            self.config.stiffness as f64,
            self.config.damping_ratio as f64,
            (self.value - self.target) as f64,
            self.velocity as f64,
            dt as f64,
        );

        self.value = (new_disp + self.target as f64) as f32;
        self.velocity = new_vel as f32;

        if (self.value - self.target).abs() <= self.config.value_threshold
            && self.velocity.abs() <= (self.config.value_threshold * 50.0)
        {
            self.value = self.target;
            self.velocity = 0.0;
            true
        } else {
            false
        }
    }

    /// `|displacement|` or `|velocity|` of the closed-form response at `t`.
    #[inline]
    fn magnitude_at(&self, t: f64, velocity: bool) -> f64 {
        let (d, v) = spring_response(
            self.config.stiffness as f64,
            self.config.damping_ratio as f64,
            (self.value - self.target) as f64,
            self.velocity as f64,
            t,
        );
        if velocity { v.abs() } else { d.abs() }
    }

    /// Seconds until `|v| <= v_thr` **and** `|x - target| <= value_threshold`.
    ///
    /// Mirrors `SpringAnimationBuilder.getDuration` (`:159-182`): derive the
    /// analytic phase of the response, then search for the *shortest* time the
    /// response is inside both rest thresholds, bisecting to
    /// `min_diff = frame_ms / 2000` (`SpringAnimationBuilder.java:170-182`).
    ///
    /// `frame_ms` is the display's frame period (8.333 at 120 Hz, 16.667 at
    /// 60 Hz); passing it is mandatory because the tolerance is defined in
    /// units of frames, not seconds. A finer `frame_ms` can only demand an
    /// equal-or-longer settle, so the answer is monotonic in it.
    ///
    /// How the search works: `|d|` and `|v|` are each piecewise monotone in
    /// `t`, turning only where `d`, `v` or `v'` is zero, so the timeline is
    /// split at those analytic phases into segments on which both magnitudes
    /// are monotone. Each segment therefore has one interval of "at rest"
    /// time, found by bisecting the two threshold crossings; the first
    /// segment whose two intervals overlap holds the answer. The `t`-grid is
    /// the same half-period advance `getDuration` walks (`:162-166`), three
    /// sub-segments per half period because the response turns three times
    /// there.
    ///
    /// Returns `f32::INFINITY` when the spring provably never settles: an
    /// undamped (`zeta == 0`) oscillator, a non-finite or non-positive
    /// stiffness, a non-finite or non-positive damping ratio, a
    /// non-positive rest threshold, or a response that is still outside the
    /// thresholds after [`SETTLE_SEGMENT_CAP`] segments. Android rejects
    /// `zeta <= 0` outright (`SpringAnimationBuilder.java:107-109`), but this
    /// integrator supports it, so it has to be handled rather than assumed
    /// away or the shell would animate forever.
    ///
    /// The returned time is the earliest point consistent with the rest
    /// thresholds at the requested resolution, so it can sit up to
    /// `frame_ms / 2000` *before* the spring reports rest through
    /// [`Self::step`]; at 120 Hz that is at most 4.2 ms of a still-visible
    /// tail, well under half a frame. Verified against a 0.2 us direct scan of
    /// the closed form over 10k+ randomised profiles: never later than the true
    /// first rest point, never earlier by more than `min_diff`.
    /// Allocation-free and `O(segments * log(1 / tol))`, and only ever run when
    /// an animation starts.
    pub fn settle_duration(&self, frame_ms: f32) -> f32 {
        let k = self.config.stiffness;
        let zeta = self.config.damping_ratio;
        let thr = self.config.value_threshold;
        // Guard the whole domain up front: a NaN here would poison every
        // comparison below and either hang the walk or return a NaN duration.
        if !(k.is_finite() && k > 0.0)
            || !(zeta.is_finite() && zeta > 0.0)
            || !(thr.is_finite() && thr > 0.0)
        {
            return f32::INFINITY;
        }
        let frame_ms = if frame_ms.is_finite() && frame_ms > 0.0 {
            frame_ms
        } else {
            FALLBACK_FRAME_MS
        };
        let min_diff = frame_ms as f64 / 2000.0;
        // Android's `SpringForce.VELOCITY_THRESHOLD_MULTIPLIER` is 1000/16 =
        // 62.5 (`SpringForce.java:80`); `step`/`is_at_rest` here use 50, so
        // use 50 to stay consistent with the integrator this must agree with.
        let v_thr = thr as f64 * 50.0;
        let d0 = (self.value - self.target) as f64;
        let v0 = self.velocity as f64;
        if !(d0.is_finite() && v0.is_finite()) {
            return f32::INFINITY;
        }
        if d0.abs() <= thr as f64 && v0.abs() <= v_thr {
            return 0.0;
        }

        let w = (k as f64).sqrt();
        let gamma = zeta as f64 * w;
        let oscillating = zeta <= 1.0 && (zeta - 1.0).abs() >= 1e-4;

        // Analytic phase seeds, shared by both regimes:
        //   phase 0    -> d = 0  (velocity peaks, largest |v| on a zero crossing)
        //   phase psi  -> v = 0  (displacement peaks, largest |d|)
        //   phase chi  -> v' = 0 (the interior |v| maximum)
        // The underdamped response is `(R / wd) * e^(-gamma t) * sin(wd t +
        // theta)`, and `v'` is `-2 gamma wd cos(phi) + (gamma^2 - wd^2)
        // sin(phi)`, which is where the third phase comes from. Splitting the
        // timeline on all three is what makes both magnitudes monotone inside
        // a segment, which the crossing search below relies on.
        let mut damped_freq = 0.0f64;
        let mut theta = 0.0f64;
        let mut phase_1 = 0.0f64;
        let mut phase_2 = 0.0f64;
        if oscillating {
            damped_freq = w * (1.0 - (zeta as f64) * (zeta as f64)).sqrt();
            // d = (R / wd) e^(-gamma t) sin(wd t + theta) with
            // R cos(theta) = v0 + gamma d0 and R sin(theta) = wd d0.
            let a = v0 + gamma * d0;
            if (a * a + damped_freq * damped_freq * d0 * d0).sqrt() == 0.0 {
                return 0.0;
            }
            theta = (damped_freq * d0).atan2(a);
            phase_1 = damped_freq.atan2(gamma);
            phase_2 = (2.0 * gamma * damped_freq).atan2(gamma * gamma - damped_freq * damped_freq);
            if phase_1 > phase_2 {
                core::mem::swap(&mut phase_1, &mut phase_2);
            }
        }

        // Non-oscillating responses turn at most three times, at these roots.
        let mut flat = [0.0f64; 3];
        let mut flat_len = 0usize;
        if !oscillating {
            if zeta > 1.0 {
                let root = w * ((zeta as f64) * (zeta as f64) - 1.0).sqrt();
                let gamma_plus = -(zeta as f64) * w + root;
                let gamma_minus = -(zeta as f64) * w - root;
                let coeff_b = (gamma_minus * d0 - v0) / (gamma_minus - gamma_plus);
                let coeff_a = d0 - coeff_b;
                let span = gamma_minus - gamma_plus;
                // For `d = A e^(gm t) + B e^(gp t)`, the three turning points
                // are the roots of `d`, `d'` and `d''`:
                //   d  = 0  ->  t = ln(-B/A) / span
                //   d' = 0  ->  t = ln(-B gp / (A gm)) / span
                //   d''= 0  ->  t = ln(-B gp^2 / (A gm^2)) / span
                // Each root needs its log argument positive, which is what the
                // sign tests below check; a real turning point with the wrong
                // sign is impossible, so skipping it is exact, not a
                // conservative approximation.
                if coeff_a != 0.0 && coeff_b != 0.0 && coeff_a * coeff_b < 0.0 {
                    flat[flat_len] = (-coeff_b / coeff_a).ln() / span;
                    flat_len += 1;
                }
                if coeff_a != 0.0
                    && coeff_b != 0.0
                    && coeff_a * gamma_minus * coeff_b * gamma_plus < 0.0
                {
                    flat[flat_len] = (-coeff_b * gamma_plus / (coeff_a * gamma_minus)).ln() / span;
                    flat_len += 1;
                }
                if coeff_a != 0.0
                    && coeff_b != 0.0
                    && coeff_a * gamma_minus * gamma_minus * coeff_b * gamma_plus * gamma_plus < 0.0
                {
                    flat[flat_len] =
                        (-coeff_b * gamma_plus * gamma_plus / (coeff_a * gamma_minus * gamma_minus))
                            .ln() / span;
                    flat_len += 1;
                }
            } else {
                // Critically damped: d = (A + B t) e^(-w t), v = (B - w (A + B t))
                // e^(-w t), d' = (w^2 A + w^2 B t - 2 w B) e^(-w t).
                let coeff_b = v0 + w * d0;
                if coeff_b != 0.0 {
                    flat[0] = -d0 / coeff_b;
                    flat[1] = (coeff_b - w * d0) / (w * coeff_b);
                    flat[2] = 2.0 / w - d0 / coeff_b;
                    flat_len = 3;
                }
            }
            // Negative and non-finite roots are in the past (or garbage), so
            // they are pushed out to infinity and the walk skips them. Written
            // as a positive test because the alternative, `root <= 0.0`, is
            // false for NaN and would let NaN through into the walk.
            for root in flat.iter_mut().take(flat_len) {
                if root.is_nan() || *root <= 0.0 || !root.is_finite() {
                    *root = f64::INFINITY;
                }
            }
            for i in 1..flat_len {
                let mut j = i;
                while j > 0 && flat[j] < flat[j - 1] {
                    flat.swap(j, j - 1);
                    j -= 1;
                }
            }
        }

        // The turning points of the underdamped response are at
        // `phase = m*PI + {0, psi, chi}` for integer `m`, i.e. three per half
        // period, ordered `0 < psi < chi`. The walk has to start at the first
        // such phase that is not already in the past, which depends on the
        // initial phase `theta` and is *not* always `m = 0`: a spring released
        // near the top of its swing has `theta` near 0, while one released
        // with a large initial velocity has `theta` near +/-PI and its first
        // turning point after t=0 lies at `m = -1`. Starting at `m = 0`
        // regardless skips those, and a segment with a skipped turning point
        // has a non-monotone magnitude, so the crossing search below would
        // bracket the wrong root -- which is how a settle time came out 2.2 s
        // early during fuzzing.
        let first_period = if oscillating {
            // Smallest m with `m*PI + phase_2 >= theta` for the largest phase.
            ((theta - phase_2) / core::f64::consts::PI).floor() as i64 - 1
        } else {
            0
        };

        let mut lo = 0.0f64;
        for index in 0..SETTLE_SEGMENT_CAP {
            let mut hi = if oscillating {
                // Segments repeat every half period, three per period.
                let period = first_period + (index / 3) as i64;
                let phase = match index % 3 {
                    0 => 0.0,
                    1 => phase_1,
                    _ => phase_2,
                };
                let t = (period as f64 * core::f64::consts::PI + phase - theta) / damped_freq;
                if t < 1e7 { t } else { f64::INFINITY }
            } else {
                *flat.get(index as usize).unwrap_or(&f64::INFINITY)
            };

            if !hi.is_finite() {
                // Tail segment: walk forward until the spring is provably at
                // rest, then bisect inside that span. Bounded, and a spring
                // that is not at rest after 64 doublings never settles.
                let mut probe = lo + 1.0 / w;
                let mut ok = false;
                for _ in 0..64 {
                    let (d, v) = spring_response(k as f64, zeta as f64, d0, v0, probe);
                    if d.abs() <= thr as f64 && v.abs() <= v_thr {
                        ok = true;
                        break;
                    }
                    probe += if probe > 1.0 / w { probe } else { 1.0 / w };
                }
                if !ok {
                    return f32::INFINITY;
                }
                hi = probe;
            }
            if hi <= lo {
                continue;
            }

            // Certification pass: fixed tolerance, so the segment that holds
            // the first rest point is the same whatever `frame_ms` says.
            let (cert_d, cert_v) = match (
                rest_window(
                    |t| self.magnitude_at(t, false),
                    lo,
                    hi,
                    thr as f64,
                    SETTLE_CERTIFY_S,
                ),
                rest_window(|t| self.magnitude_at(t, true), lo, hi, v_thr, SETTLE_CERTIFY_S),
            ) {
                (Some(d), Some(v)) => (d, v),
                _ => {
                    lo = hi;
                    continue;
                }
            };
            // The segment holds rest time only if the two windows overlap, and
            // both bounds are certified inside their own window, so an overlap
            // is a real instant at which the spring is at rest.
            if cert_d.first.max(cert_v.first) > cert_d.last.min(cert_v.last) {
                lo = hi;
                continue;
            }

            // Report pass: re-solve at the caller's own resolution and take
            // the earliest point both windows admit.
            if let (Some(d), Some(v)) = (
                rest_window(|t| self.magnitude_at(t, false), lo, hi, thr as f64, min_diff),
                rest_window(|t| self.magnitude_at(t, true), lo, hi, v_thr, min_diff),
            ) {
                return d.first.max(v.first) as f32;
            }
            lo = hi;
        }
        f32::INFINITY
    }

    /// Advance `self` as if its value were `value * 1000`, returning the
    /// unscaled value.
    ///
    /// The rest thresholds stay in the scaled domain, which is the whole
    /// point: `SpringConfig::recents_scale`'s `0.002` threshold applied to
    /// `scale * 1000` is `2e-6` in real scale units, 1000x finer than the
    /// same threshold on the raw value. See [`RECENTS_SCALE_MULTIPLIER`].
    pub fn step_scaled(&mut self, dt: f32) -> f32 {
        let m = RECENTS_SCALE_MULTIPLIER;
        self.value *= m;
        self.velocity *= m;
        self.target *= m;
        self.step(dt);
        self.value /= m;
        self.velocity /= m;
        self.target /= m;
        self.value
    }
}

/// Android carries `RECENTS_SCALE_SPRING_MULTIPLIER = 1000.0`
/// (`views/RecentsDismissUtils.kt:1383`) because an f32 spring integrating
/// toward 0.9875 loses so much precision it never reaches its threshold.
/// Animating the value x1000 and dividing on read is the documented fix.
///
/// In this integrator the x1000 still buys real accuracy, but not for the
/// reason Android gives: [`SpringSimulation::step`] evaluates the response in
/// `f64` and only rounds once on store, so the arithmetic is not the limit --
/// the `f32` *state* is. Scaling does not change an f32's relative precision
/// either (it only shifts the exponent); what it does change is the
/// threshold, which the scaled domain applies 1000x tighter.
pub const RECENTS_SCALE_MULTIPLIER: f32 = 1000.0;

/// Spring profile for the recents swipe-up scale. Thin alias so call sites read
/// as intent rather than as a bare constant pair.
#[inline]
pub fn recents_scale_config() -> SpringConfig {
    SpringConfig::recents_scale()
}

/// Boundary overscroll resistance lives in [`super::layout::damped_scroll`].
///
/// It used to also live here, as `apply_overscroll_resistance(drag, screen_w)`.
/// That duplication was removed rather than kept in sync: two copies of the
/// same curve with a bit-equality test between them is strictly worse than one
/// copy, and it had already proven the point -- the copy in this file was
/// written as a `1 / (1 + x / 100)` rational while its own doc comment
/// claimed it was "Authentic Android/Lawnchair 17".
///
/// The only thing the shell lost is the `max` convenience, and that was
/// hiding an invented constant. AOSP passes the *container extent* on a drag
/// and `page_width * 0.5` on a fling (`PagedView.java:1552`); this function
/// hard-coded `screen_width * 0.35` for both. Call sites now pass `max`
/// explicitly, so the two cases can finally differ.

// Authentic Lawnchair 17 / Material You (Monet) Dynamic Tonal Palette.
// Derived with zero dynamic heap allocations on the hot render path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterialYouPalette {
    pub surface: u32,
    pub surface_container: u32,
    pub surface_container_high: u32,
    pub primary: u32,
    pub on_primary: u32,
    pub primary_container: u32,
    pub on_primary_container: u32,
    pub secondary: u32,
    pub tertiary: u32,
    pub on_surface: u32,
    pub on_surface_variant: u32,
    pub outline: u32,
    pub outline_variant: u32,
}

impl MaterialYouPalette {
    pub const fn default_dark() -> Self {
        Self {
            surface: 0xFF0B0F19,
            surface_container: 0xFF182236,
            surface_container_high: 0xFF222E46,
            primary: 0xFF38BDF8,
            on_primary: 0xFF003548,
            primary_container: 0xFF0284C7,
            on_primary_container: 0xFFE0F2FE,
            secondary: 0xFF94A3B8,
            tertiary: 0xFFA78BFA,
            on_surface: 0xFFF8FAFC,
            on_surface_variant: 0xFF94A3B8,
            outline: 0xFF334155,
            outline_variant: 0xFF1E293B,
        }
    }

    /// Derive Material You dynamic tonal scheme from an ARGB seed color (zero heap alloc).
    pub fn from_seed(seed_color: u32) -> Self {
        let r = ((seed_color >> 16) & 0xFF) as f32 / 255.0;
        let g = ((seed_color >> 8) & 0xFF) as f32 / 255.0;
        let b = (seed_color & 0xFF) as f32 / 255.0;

        // Extract HSL hue
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let mut hue = 0.0f32;
        if delta > 1e-4 {
            if (max - r).abs() < 1e-4 {
                hue = 60.0 * (((g - b) / delta) % 6.0);
            } else if (max - g).abs() < 1e-4 {
                hue = 60.0 * (((b - r) / delta) + 2.0);
            } else {
                hue = 60.0 * (((r - g) / delta) + 4.0);
            }
            if hue < 0.0 { hue += 360.0; }
        }

        let hsl_to_rgb = |h: f32, s: f32, l: f32| -> u32 {
            let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
            let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
            let m = l - c / 2.0;
            let (r1, g1, b1) = if h < 60.0 {
                (c, x, 0.0)
            } else if h < 120.0 {
                (x, c, 0.0)
            } else if h < 180.0 {
                (0.0, c, x)
            } else if h < 240.0 {
                (0.0, x, c)
            } else if h < 300.0 {
                (x, 0.0, c)
            } else {
                (c, 0.0, x)
            };
            let ir = ((r1 + m).clamp(0.0, 1.0) * 255.0).round() as u32;
            let ig = ((g1 + m).clamp(0.0, 1.0) * 255.0).round() as u32;
            let ib = ((b1 + m).clamp(0.0, 1.0) * 255.0).round() as u32;
            0xFF000000 | (ir << 16) | (ig << 8) | ib
        };

        let tertiary_hue = (hue + 60.0) % 360.0;

        Self {
            surface: hsl_to_rgb(hue, 0.15, 0.06),
            surface_container: hsl_to_rgb(hue, 0.20, 0.12),
            surface_container_high: hsl_to_rgb(hue, 0.22, 0.18),
            primary: hsl_to_rgb(hue, 0.85, 0.65),
            on_primary: hsl_to_rgb(hue, 0.90, 0.12),
            primary_container: hsl_to_rgb(hue, 0.70, 0.35),
            on_primary_container: hsl_to_rgb(hue, 0.80, 0.92),
            secondary: hsl_to_rgb(hue, 0.20, 0.65),
            tertiary: hsl_to_rgb(tertiary_hue, 0.65, 0.68),
            on_surface: 0xFFF8FAFC,
            on_surface_variant: hsl_to_rgb(hue, 0.15, 0.68),
            outline: hsl_to_rgb(hue, 0.18, 0.25),
            outline_variant: hsl_to_rgb(hue, 0.15, 0.15),
        }
    }
}

/// Lawnchair 17 / Pixel Launcher Material You display clock.
///
/// The clock is set in the shell's display weight with slightly loosened
/// tracking (the Pixel/Lawnchair clock is not a 7-segment readout), and is
/// optically centred on the cap height rather than the em box.
#[allow(clippy::too_many_arguments)]
fn draw_material_you_clock(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    center_x: usize,
    center_y: usize,
    time_str: &str,
    color: u32,
    digit_h: usize,
) {
    use super::font::{em_px_at, ASCENDER, CAP_HEIGHT};
    // digit_h is the cap height, so derive the em size from the cap metrics
    // and the panel's type density.
    let k = (w as f32 / super::font::REFERENCE_PANEL_W).clamp(0.5, 2.0);
    let size = (digit_h as f32 * k / (CAP_HEIGHT / 1000.0)).min(em_px_at(4, w));
    let tracking = size * 0.04;
    let mut run_w = clock_run_width(time_str, digit_h as f32, w);
    let max_w = w.saturating_sub(8) as f32;
    if (run_w as usize) > w.saturating_sub(8) {
        // Pathologically long string for the panel: keep it on screen.
        run_w = max_w;
    }
    let mut pen = center_x as f32 - run_w * 0.5;
    let top = center_y as f32 - digit_h as f32 * 0.5 - (ASCENDER - CAP_HEIGHT) / 1000.0 * size;
    for b in time_str.bytes() {
        pen += super::font::draw_glyph(
            buf,
            stride,
            w,
            h,
            pen,
            top,
            b,
            color,
            size,
            FontWeight::Medium,
        );
        pen += tracking;
    }
}

/// Descender-safe text height (ascender to descender) for a UI scale on a
/// `panel_w`-wide panel.
#[inline]
pub fn text_line_height(scale: usize, panel_w: usize) -> usize {
    let size = super::font::em_px_at(scale, panel_w);
    ((super::font::ASCENDER - super::font::DESCENDER) / 1000.0 * size).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
        let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..w * h {
            pixels.extend_from_slice(&rgba);
        }
        RgbaImage { width: w, height: h, pixels }
    }

    #[test]
    fn icon_blit_identity_copies_pixels_and_leaves_neighbours() {
        let img = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                10, 20, 30, 255, //
                40, 50, 60, 255, //
                70, 80, 90, 255, //
                100, 110, 120, 255,
            ],
        };
        let mut buf = vec![0xFF112233u32; 16];
        draw_icon_bitmap(&mut buf, 4, 4, 4, 1, 1, 2, 2, 0, &img);
        assert_eq!(buf[5], 0xFF0A141E);
        assert_eq!(buf[6], 0xFF28323C);
        assert_eq!(buf[9], 0xFF46505A);
        assert_eq!(buf[10], 0xFF646E78);
        assert_eq!(buf[0], 0xFF112233, "outside the tile is untouched");
        assert_eq!(buf[3], 0xFF112233);
    }

    #[test]
    fn icon_blit_alpha_blends_over_the_tile() {
        let img = solid(1, 1, [200, 100, 50, 128]);
        let mut buf = vec![0xFF000000u32; 1];
        draw_icon_bitmap(&mut buf, 1, 1, 1, 0, 0, 1, 1, 0, &img);
        // 128/255 of the icon over an opaque black tile, rounded to nearest.
        assert_eq!(buf[0], 0xFF643219, "got {:#010x}", buf[0]);

        // Fully transparent icon pixels leave the tile exactly as it was.
        let clear = solid(1, 1, [255, 255, 255, 0]);
        let mut buf = vec![0xFF102030u32; 1];
        draw_icon_bitmap(&mut buf, 1, 1, 1, 0, 0, 1, 1, 0, &clear);
        assert_eq!(buf[0], 0xFF102030);
    }

    #[test]
    fn icon_blit_mask_matches_the_tile_mask() {
        // The icon must be clipped by exactly the same rounded rectangle the
        // coloured tile uses, for the tile geometries the launcher draws.
        for (tw, th, radius) in [(64, 64, 16), (54, 54, 16), (4, 4, 2), (8, 5, 3), (3, 3, 1)] {
            let icon_color = 0xFFC86432;
            let img = solid(tw as u32, th as u32, [200, 100, 50, 255]);
            let px = tw * th;
            let mut blit = vec![0xFF000000u32; px];
            draw_icon_bitmap(&mut blit, tw, tw, th, 0, 0, tw, th, radius, &img);
            let mut tile = vec![0xFF000000u32; px];
            draw_rounded_rect(&mut tile, tw, tw, th, 0, 0, tw, th, radius, 0xFF123456);

            for i in 0..px {
                let in_tile = tile[i] == 0xFF123456;
                let in_blit = blit[i] == icon_color;
                assert_eq!(in_tile, in_blit, "tw={tw} th={th} radius={radius} pixel {i}");
            }
        }
    }

    #[test]
    fn icon_blit_stays_inside_the_framebuffer() {
        let img = solid(4, 4, [9, 9, 9, 255]);
        // Tile larger than the framebuffer and offset past its right edge.
        let mut buf = vec![0xFF000000u32; 4];
        draw_icon_bitmap(&mut buf, 2, 2, 2, 1, 1, 4, 4, 1, &img);
        draw_icon_bitmap(&mut buf, 2, 2, 2, 5, 5, 4, 4, 1, &img);
        // Degenerate inputs are no-ops rather than panics.
        draw_icon_bitmap(&mut buf, 2, 2, 2, 0, 0, 0, 4, 1, &img);
        draw_icon_bitmap(&mut buf, 2, 2, 2, 0, 0, 4, 4, 1, &RgbaImage { width: 0, height: 0, pixels: vec![] });
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn icon_blit_upscales_without_losing_coverage() {
        let img = solid(2, 2, [7, 7, 7, 255]);
        let mut buf = vec![0xFF000000u32; 16];
        draw_icon_bitmap(&mut buf, 4, 4, 4, 0, 0, 4, 4, 0, &img);
        assert!(buf.iter().all(|px| *px == 0xFF070707), "every tile pixel filled");
    }

    #[test]
    fn frame_hash_tracks_icon_content() {
        let icon_a = RgbaImage { width: 1, height: 1, pixels: vec![1, 2, 3, 255] };
        let icon_b = RgbaImage { width: 1, height: 1, pixels: vec![4, 5, 6, 255] };
        let mut state = DrmInteractiveState::default();

        fn item<'a>(icon: Option<&'a RgbaImage>) -> AppGridItem<'a> {
            AppGridItem {
                id: "x",
                name: "X",
                color: 0xFF000000,
                glyph: "X",
                icon,
            }
        }
        let none = [item(None)];
        let with_a_items = [item(Some(&icon_a))];
        let with_b_items = [item(Some(&icon_b))];

        state.grid_apps = &none;
        let base = interactive_state_hash(&state);
        state.grid_apps = &with_a_items;
        let with_a = interactive_state_hash(&state);
        state.grid_apps = &with_b_items;
        let with_b = interactive_state_hash(&state);
        assert_ne!(base, with_a, "loading an icon forces a redraw");
        assert_ne!(with_a, with_b, "a different icon forces a redraw");

        // Same state again: the render-skip fast path must still hold.
        state.grid_apps = &with_b_items;
        assert_eq!(with_b, interactive_state_hash(&state));
    }

    #[test]
    fn test_app_input_and_typing_focus_hash_invalidation() {
        let mut state = DrmInteractiveState::default();
        let h0 = interactive_state_hash(&state);

        state.active_app = Some("Browser");
        let h_app = interactive_state_hash(&state);
        assert_ne!(h0, h_app);

        state.app_input_focused = true;
        let h_focused = interactive_state_hash(&state);
        assert_ne!(h_app, h_focused);

        state.app_input = "https://example.org";
        let h_typed = interactive_state_hash(&state);
        assert_ne!(h_focused, h_typed);

        state.keyboard_shift_active = true;
        let h_shift = interactive_state_hash(&state);
        assert_ne!(h_typed, h_shift);
    }

    #[test]
    fn test_draw_icon_bitmap_blitting() {
        let mut buf = vec![0xFF000000u32; 100 * 100];
        let mut pixels = vec![0u8; 64 * 64 * 4];
        for y in 20..44 {
            for x in 20..44 {
                let idx = (y * 64 + x) * 4;
                pixels[idx] = 255;
                pixels[idx + 1] = 255;
                pixels[idx + 2] = 255;
                pixels[idx + 3] = 255;
            }
        }
        let img = crate::graphics::png::RgbaImage {
            width: 64,
            height: 64,
            pixels,
        };
        draw_icon_bitmap(&mut buf, 100, 100, 100, 18, 18, 64, 64, 16, &img);
        let drawn_pixels = buf.iter().filter(|&&px| px == 0xFFFFFFFF).count();
        assert!(drawn_pixels > 0, "Icon bitmap pixels must be blitted onto framebuffer");
    }

    #[test]
    fn test_format_apps_count_stack_only() {
        let mut buf = [0u8; 16];
        assert_eq!(format_apps_count(&mut buf, 0), "0 APPS");
        assert_eq!(format_apps_count(&mut buf, 1), "1 APPS");
        assert_eq!(format_apps_count(&mut buf, 15), "15 APPS");
        assert_eq!(format_apps_count(&mut buf, 42), "42 APPS");
        assert_eq!(format_apps_count(&mut buf, 128), "128 APPS");
    }

    #[test]
    fn test_apply_frosted_blur_region_scanline_clamping() {
        let w = 16;
        let h = 16;
        let mut buf = vec![0xFFFFFFFFu32; w * h];
        // Apply blur only from y = 8 to 16
        apply_frosted_blur_region(&mut buf, w, w, 8, 16);
        // Top 8 rows should remain completely white (untouched)
        for y in 0..8 {
            for x in 0..w {
                assert_eq!(buf[y * w + x], 0xFFFFFFFF, "Row {} must be untouched", y);
            }
        }
        // Rows 8..16 should be tinted/blurred
        for y in 8..16 {
            for x in 0..w {
                assert_ne!(buf[y * w + x], 0xFFFFFFFF, "Row {} should be blurred", y);
            }
        }
    }

    /// Rows that carry any ink for `ch` drawn at UI `scale`, as (top, bottom).
    ///
    /// The buffer doubles as the "panel", so the type density matches the
    /// em the renderer would actually use for it.
    fn ink_rows(ch: char, scale: usize) -> (usize, usize) {
        let w = 1080usize;
        let h = 240usize;
        let mut buf = vec![0xFF000000u32; w * h];
        let s = ch.to_string();
        draw_text(&mut buf, w, w, h, 40, 40, &s, 0xFFFFFFFF, scale);
        let mut top = usize::MAX;
        let mut bottom = 0usize;
        for y in 0..h {
            if buf[y * w..(y + 1) * w].iter().any(|&p| p != 0xFF000000) {
                top = top.min(y);
                bottom = y;
            }
        }
        (top, bottom)
    }

    #[test]
    fn test_vector_type_descenders_and_ascenders() {
        // Compare against 'o' at the same size, and require a real fraction of
        // the em: 200-12 = 188 units of descender, 740-532 = 208 of ascender.
        let em = crate::graphics::font::em_px_at(3, 1080);
        let descender_px = (188.0 / 1000.0 * em) as i64;
        let ascender_px = (208.0 / 1000.0 * em) as i64;
        let (o_top, o_bottom) = ink_rows('o', 3);
        assert!(o_bottom > o_top, "sanity: 'o' must have ink");
        for ch in ['g', 'p', 'q', 'y', 'j'] {
            let (_, bottom) = ink_rows(ch, 3);
            assert!(
                bottom as i64 - o_bottom as i64 >= descender_px / 2,
                "'{}' must descend below the x-height baseline ({} vs {}, want >= {})",
                ch,
                bottom,
                o_bottom,
                descender_px / 2
            );
        }
        for ch in ['b', 'd', 'h', 'k', 'l', 'f'] {
            let (top, _) = ink_rows(ch, 3);
            assert!(
                o_top as i64 - top as i64 >= ascender_px / 2,
                "'{}' must ascend above the x-height ({} vs {}, want >= {})",
                ch,
                top,
                o_top,
                ascender_px / 2
            );
        }
    }

    #[test]
    fn test_font_weight_drawing() {
        let mut buf_reg = vec![0xFF000000u32; 32 * 32];
        let mut buf_med = vec![0xFF000000u32; 32 * 32];
        let mut buf_bold = vec![0xFF000000u32; 32 * 32];
        draw_text_weighted(&mut buf_reg, 32, 32, 32, 4, 4, "A", 0xFFFFFFFF, 1, FontWeight::Regular);
        draw_text_weighted(&mut buf_med, 32, 32, 32, 4, 4, "A", 0xFFFFFFFF, 1, FontWeight::Medium);
        draw_text_weighted(&mut buf_bold, 32, 32, 32, 4, 4, "A", 0xFFFFFFFF, 1, FontWeight::Bold);
        let ink = |b: &[u32]| b.iter().filter(|&&p| p != 0xFF000000).count();
        let (reg, med, bold) = (ink(&buf_reg), ink(&buf_med), ink(&buf_bold));
        assert!(bold > med, "Bold must carry more ink than Medium ({} vs {})", bold, med);
        assert!(med > reg, "Medium must carry more ink than Regular ({} vs {})", med, reg);
    }

    #[test]
    fn test_antialiased_glyphs_have_no_hard_edges() {
        // Analytic coverage means a rendered glyph must contain partially
        // covered pixels along its outline: a purely binary blit would not.
        let w = 200usize;
        let mut buf = vec![0xFF000000u32; w * w];
        draw_text(&mut buf, w, w, w, 10, 40, "S", 0xFFFFFFFF, 3);
        let partial = buf
            .iter()
            .filter(|&&p| (p & 0xFF) > 8 && (p & 0xFF) < 250)
            .count();
        assert!(partial > 40, "expected an anti-aliased ramp, got {} partial px", partial);
    }

    #[test]
    fn test_spring_simulation_analytical_dynamics() {
        // Drawer spring settling
        let mut spring = SpringSimulation::new(0.0, 1.0, SpringConfig::drawer());
        let dt = 0.016;
        for _ in 0..10 {
            spring.step(dt);
        }
        assert!(spring.value > 0.80, "Drawer spring should smoothly reach > 0.80 in ~160ms, got {}", spring.value);

        // Continue until rest
        for _ in 0..60 {
            spring.step(dt);
        }
        assert!(spring.is_at_rest(), "Spring should reach equilibrium at rest");
        assert_eq!(spring.value, 1.0);

        // Page swipe overshoot & rebound
        let mut swipe_spring = SpringSimulation::new(400.0, 0.0, SpringConfig::page_swipe());
        let mut overshot = false;
        for _ in 0..80 {
            swipe_spring.step(dt);
            if swipe_spring.value < 0.0 {
                overshot = true;
            }
        }
        assert!(overshot, "Page swipe spring (damping ratio 0.75) must produce authentic overshoot & rebound");
        assert!(swipe_spring.is_at_rest(), "Page swipe spring must settle to rest");

        // Icon bounce compression and rebound
        let mut bounce_spring = SpringSimulation::new(0.92, 1.0, SpringConfig::icon_bounce());
        let mut bounce_overshoot = false;
        for _ in 0..60 {
            bounce_spring.step(dt);
            if bounce_spring.value > 1.01 {
                bounce_overshoot = true;
            }
        }
        assert!(bounce_overshoot, "Icon bounce must compress (0.92) and rebound past 1.0 (> 1.01)");
    }

    #[test]
    fn test_material_you_palette_derivation() {
        let palette_blue = MaterialYouPalette::from_seed(0xFF3B82F6);
        assert_eq!(palette_blue.on_surface, 0xFFF8FAFC);
        assert_ne!(palette_blue.surface, 0x00000000);
        assert_ne!(palette_blue.primary, 0x00000000);

        let palette_green = MaterialYouPalette::from_seed(0xFF10B981);
        assert_ne!(palette_blue.primary, palette_green.primary, "Different seeds must produce distinct palettes");

        // Verify default dark
        let def = MaterialYouPalette::default_dark();
        assert_eq!(def.primary, 0xFF38BDF8);
    }

    // -----------------------------------------------------------------
    // Spring profile table (§1.11 of the rewrite plan)
    // -----------------------------------------------------------------

    /// Every sourced Lawnchair row, with the k/zeta/threshold it must produce.
    /// `#[derive(PartialEq)]` compares bit-exactly, which is what a table
    /// transcription wants; the tolerance is applied separately for values the
    /// table itself derives (`stretch_edge`'s omega -> k conversion).
    const SPRING_TABLE: [(&str, SpringConfig, f32, f32, f32); 16] = [
        ("icon_rebound", SpringConfig::icon_rebound(), 1500.0, 0.5, 0.002),
        ("folder_morph", SpringConfig::folder_morph(), 380.0, 0.8, 0.002),
        ("folder_scrim", SpringConfig::folder_scrim(), 380.0, 0.98, 0.002),
        ("folder_alpha", SpringConfig::folder_alpha(), 1600.0, 0.9, 0.002),
        ("drawer_reveal", SpringConfig::drawer_reveal(), 150.0, 0.7, 0.002),
        ("task_dismiss", SpringConfig::task_dismiss(), 850.0, 0.65, 0.5),
        ("grid_reflow", SpringConfig::grid_reflow(), 2800.0, 0.8, 0.5),
        ("magnetic_detach", SpringConfig::magnetic_detach(), 800.0, 0.95, 0.5),
        ("spring_loaded", SpringConfig::spring_loaded(), 200.0, 0.7, 0.002),
        ("recents_scale", SpringConfig::recents_scale(), 200.0, 0.75, 0.002),
        ("dismiss_effects", SpringConfig::dismiss_effects(), 1600.0, 1.0, 0.002),
        ("desktop_slide", SpringConfig::desktop_slide(), 380.0, 0.8, 0.5),
        ("icon_swipe_offset", SpringConfig::icon_swipe_offset(), 10000.0, 1.0, 0.5),
        ("icon_swipe_postfling", SpringConfig::icon_swipe_postfling(), 1500.0, 1.0, 0.5),
        ("recents_attach_alpha", SpringConfig::recents_attach_alpha(), 250.0, 0.8, 0.002),
        ("taskbar_translation", SpringConfig::taskbar_translation(), 200.0, 0.5, 0.5),
    ];

    /// 120 Hz frame period: `1000.0 / 120.0`.
    const FRAME_120: f32 = 8.333;

    #[test]
    fn spring_profiles_match_lawnchair_table() {
        for (name, cfg, k, zeta, thr) in SPRING_TABLE {
            assert_eq!(cfg.stiffness, k, "{}: stiffness", name);
            assert_eq!(cfg.damping_ratio, zeta, "{}: damping ratio", name);
            assert_eq!(cfg.value_threshold, thr, "{}: value threshold", name);
        }
        assert_eq!(SPRING_TABLE.len(), 16, "every sourced row must be listed");

        // The four legacy constructors `utlc` links against must keep their
        // exact values: they are API, not a profile table.
        assert_eq!(SpringConfig::drawer(), SpringConfig { stiffness: 300.0, damping_ratio: 0.85, value_threshold: 0.001 });
        assert_eq!(SpringConfig::page_swipe(), SpringConfig { stiffness: 280.0, damping_ratio: 0.75, value_threshold: 0.5 });
        assert_eq!(SpringConfig::icon_bounce(), SpringConfig { stiffness: 450.0, damping_ratio: 0.5, value_threshold: 0.002 });
        assert_eq!(SpringConfig::app_launch(), SpringConfig { stiffness: 320.0, damping_ratio: 0.90, value_threshold: 0.001 });
    }

    #[test]
    fn task_dismiss_hops_raise_damping_ratio_and_clamp() {
        // RecentsDismissUtils.kt:1382 adds 0.15 per further neighbour.
        assert_eq!(SpringConfig::task_dismiss_with_hops(0), SpringConfig::task_dismiss());
        // f32 accumulation: 0.65 + 0.15 == 0.79999995, so compare with the
        // tolerance the mantissa forces rather than to a decimal.
        assert!((SpringConfig::task_dismiss_with_hops(1).damping_ratio - 0.80).abs() < 1e-6);
        assert!((SpringConfig::task_dismiss_with_hops(2).damping_ratio - 0.95).abs() < 1e-6);
        assert!((SpringConfig::task_dismiss_with_hops(3).damping_ratio - 1.00).abs() < 1e-6);
        // 0.65 + 4*0.15 == 1.25: clamped, because zeta >= 1 is critically
        // damped or worse and SpringAnimationBuilder rejects it (:107-109).
        for hops in [3u8, 4, 7, 255] {
            assert_eq!(SpringConfig::task_dismiss_with_hops(hops).damping_ratio, 1.0, "hops {}", hops);
        }
        // Stiffness and threshold are hop-independent.
        assert_eq!(SpringConfig::task_dismiss_with_hops(2).stiffness, 850.0);
        assert_eq!(SpringConfig::task_dismiss_with_hops(2).value_threshold, 0.5);

        // StretchEdgeEffect specifies omega = 24.657 rad/s, so the stiffness is
        // omega^2 = 607.967649. Using 24.657 directly as a stiffness would be
        // a 24.7x stiffer edge, which is the mistake this row exists to prevent.
        let edge = SpringConfig::stretch_edge();
        assert_eq!(edge.stiffness, 24.657f32 * 24.657f32);
        assert!((edge.stiffness - 607.967_65).abs() < 1e-3, "omega^2 = {}", edge.stiffness);
        assert_eq!(edge.damping_ratio, 0.98);
    }

    #[test]
    fn spring_settle_duration_is_finite_and_bounded() {
        for (name, cfg, _, zeta, _) in SPRING_TABLE {
            // zeta > 0 for every sourced row, so none of these can be the
            // never-settles case.
            assert!(zeta > 0.0, "{}: table row has zeta 0", name);
            let d = SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120);
            assert!(d.is_finite(), "{}: settle_duration must be finite, got {}", name, d);
            assert!(d > 0.0, "{}: settle_duration must be positive, got {}", name, d);
            // The slowest row is drawer_reveal: k = 150 gives omega = 12.247
            // and gamma = zeta*omega = 0.7*12.247 = 8.573/s, so the decay
            // envelope falls from 1.0 to value_threshold 0.002 in
            // ln(1/0.002)/8.573 = 6.2146/8.573 = 0.725 s. It settles sooner
            // than that because the first rest window opens while the response
            // is still mid-swing; measured 0.597 s, hence the 0.600 bound.
            assert!(d <= 0.600, "{}: settle_duration {} s exceeds the 600 ms budget", name, d);
        }

        // Monotonic in frame_ms: a finer frame tolerance can only demand an
        // equal-or-later settle. Proved by construction (the segment walk is
        // certified at a frame-independent tolerance, so only the reported
        // resolution moves, and it moves later as it tightens).
        for (name, cfg, _, _, _) in SPRING_TABLE {
            let sim = SpringSimulation::new(0.0, 1.0, cfg);
            let d30 = sim.settle_duration(33.334);
            let d20 = sim.settle_duration(20.0);
            let d60 = sim.settle_duration(16.667);
            let d120 = sim.settle_duration(FRAME_120);
            let d240 = sim.settle_duration(4.167);
            assert!(d240 >= d120, "{}: 240 Hz {} < 120 Hz {}", name, d240, d120);
            assert!(d120 >= d60, "{}: 120 Hz {} < 60 Hz {}", name, d120, d60);
            assert!(d60 >= d20, "{}: 60 Hz {} < 20 ms {}", name, d60, d20);
            assert!(d20 >= d30, "{}: 20 ms {} < 30 Hz {}", name, d20, d30);
        }
    }

    /// Integrate `step` in `dt` slices and report the wall time at which it
    /// first claims rest, or `None` if it never does within `limit`.
    ///
    /// The wall time accumulates in `f64` on purpose: `t += 0.001f32` a few
    /// hundred times drifts by milliseconds on its own and would show up as
    /// disagreement with the closed form that has nothing to do with it.
    fn simulate_settle(config: SpringConfig, value: f32, target: f32, velocity: f32, dt: f32) -> Option<f32> {
        let mut sim = SpringSimulation::new(value, target, config).with_velocity(velocity);
        let mut t = 0.0f64;
        for _ in 0..(10.0 / dt as f64) as u32 {
            let rest = sim.step(dt);
            t += dt as f64;
            if rest {
                return Some(t as f32);
            }
        }
        None
    }

    #[test]
    fn spring_settle_duration_matches_a_simulated_settle() {
        // Unit displacement, released, and the same with a fling velocity:
        // both are states the shell actually produces (a drag release and a
        // swipe release). Compare over every sourced row plus the four legacy
        // ones, since the shell drives those too.
        let cases: [(&str, SpringConfig); 23] = [
            ("icon_rebound", SpringConfig::icon_rebound()),
            ("folder_morph", SpringConfig::folder_morph()),
            ("folder_scrim", SpringConfig::folder_scrim()),
            ("folder_alpha", SpringConfig::folder_alpha()),
            ("drawer_reveal", SpringConfig::drawer_reveal()),
            ("task_dismiss", SpringConfig::task_dismiss()),
            ("grid_reflow", SpringConfig::grid_reflow()),
            ("magnetic_detach", SpringConfig::magnetic_detach()),
            ("spring_loaded", SpringConfig::spring_loaded()),
            ("recents_scale", SpringConfig::recents_scale()),
            ("dismiss_effects", SpringConfig::dismiss_effects()),
            ("desktop_slide", SpringConfig::desktop_slide()),
            ("icon_swipe_offset", SpringConfig::icon_swipe_offset()),
            ("icon_swipe_postfling", SpringConfig::icon_swipe_postfling()),
            ("recents_attach_alpha", SpringConfig::recents_attach_alpha()),
            ("taskbar_translation", SpringConfig::taskbar_translation()),
            ("stretch_edge", SpringConfig::stretch_edge()),
            ("task_dismiss_hop1", SpringConfig::task_dismiss_with_hops(1)),
            ("task_dismiss_hop2", SpringConfig::task_dismiss_with_hops(2)),
            ("drawer", SpringConfig::drawer()),
            ("page_swipe", SpringConfig::page_swipe()),
            ("icon_bounce", SpringConfig::icon_bounce()),
            ("app_launch", SpringConfig::app_launch()),
        ];
        for (name, cfg) in cases {
            // Released, and flung hard enough to reverse direction: the two
            // shapes the shell produces, and the two that exercise opposite
            // ends of the segment walk.
            for velocity in [0.0f32, -12.0] {
                let sim = SpringSimulation::new(0.0, 1.0, cfg).with_velocity(velocity);
                let closed = sim.settle_duration(FRAME_120);
                let walked = simulate_settle(cfg, 0.0, 1.0, velocity, 0.001)
                    .unwrap_or_else(|| panic!("{}: integrator never settled", name));
                let err = (closed - walked).abs();
                // Two frame periods: the closed form is allowed to sit up to
                // min_diff (one half of a frame) before the integrator's own
                // verdict, and the integrator's verdict lands on the first
                // 1 ms grid point at or after the true crossing.
                let budget = 2.0 * FRAME_120 / 1000.0;
                assert!(err <= budget, "{}: closed {} vs integrator {} (err {} > {} s)", name, closed, walked, err, budget);
            }
        }
    }

    #[test]
    fn settle_duration_is_infinite_for_undamped() {
        // zeta == 0 never settles: the response is a pure sinusoid of constant
        // amplitude. Android rejects zeta <= 0 outright
        // (SpringAnimationBuilder.java:107-109) but this integrator supports
        // it, so settle_duration has to answer rather than assume.
        let undamped = SpringConfig { stiffness: 1500.0, damping_ratio: 0.0, value_threshold: 0.002 };
        let d = SpringSimulation::new(0.0, 1.0, undamped).settle_duration(FRAME_120);
        assert!(d.is_infinite(), "undamped spring must report INFINITY, got {}", d);

        // zeta == 1 and zeta > 1 are also rejected by Android, but they do
        // settle here, and the shell drives such profiles (dismiss_effects and
        // icon_swipe_offset are both zeta == 1). Answering INFINITY for those
        // would leave the shell animating forever, which is the bug being fixed.
        for zeta in [1.0f32, 1.0001, 2.0, 5.0] {
            let cfg = SpringConfig { stiffness: 400.0, damping_ratio: zeta, value_threshold: 0.002 };
            let d = SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120);
            assert!(d.is_finite() && d > 0.0, "zeta {} must settle, got {}", zeta, d);
        }
    }

    #[test]
    fn settle_duration_rejects_hostile_configs() {
        let bad: [(f32, f32); 6] = [
            (0.0, 0.5),      // no stiffness: nothing pulls it back
            (-5.0, 0.5),     // negative stiffness
            (f32::NAN, 0.5), // NaN stiffness
            (f32::INFINITY, 0.5),
            (400.0, f32::NAN),
            (400.0, -0.5),   // negative damping ratio
        ];
        for (k, zeta) in bad {
            let cfg = SpringConfig { stiffness: k, damping_ratio: zeta, value_threshold: 0.002 };
            let d = SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120);
            assert!(d.is_infinite(), "k={} zeta={} must be INFINITY, got {}", k, zeta, d);
            assert!(!d.is_nan(), "k={} zeta={} must not be NaN", k, zeta);
        }
        // A non-positive or non-finite threshold can never be met.
        for thr in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let cfg = SpringConfig { stiffness: 400.0, damping_ratio: 0.5, value_threshold: thr };
            assert!(SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120).is_infinite());
        }
        // A non-finite initial state must not produce NaN.
        for (value, velocity) in [(f32::NAN, 0.0f32), (0.0, f32::NAN), (f32::INFINITY, 0.0), (0.0, f32::NEG_INFINITY)] {
            let mut sim = SpringSimulation::new(value, 1.0, SpringConfig::icon_rebound());
            sim.velocity = velocity;
            let d = sim.settle_duration(FRAME_120);
            assert!(!d.is_nan(), "value={} velocity={} gave NaN", value, velocity);
            assert!(d.is_infinite(), "value={} velocity={} should be INFINITY, got {}", value, velocity, d);
        }
        // A nonsense refresh rate must not disable parking: that is the whole
        // point of the computation. It degrades to 60 Hz instead.
        for frame_ms in [0.0f32, -FRAME_120, f32::NAN, f32::INFINITY] {
            let d = SpringSimulation::new(0.0, 1.0, SpringConfig::icon_rebound()).settle_duration(frame_ms);
            assert!(d.is_finite() && d > 0.0, "frame_ms={} gave {}", frame_ms, d);
        }
        // A spring already at rest needs no animation at all.
        assert_eq!(SpringSimulation::new(1.0, 1.0, SpringConfig::icon_rebound()).settle_duration(FRAME_120), 0.0);
    }

    #[test]
    fn recents_scale_spring_converges_at_scale_precision() {
        // The documented case: a recents scale settling onto 0.975.
        // RecentsDismissUtils.kt:1383 multiplies by 1000 because
        // "SpringAnimation struggles to animate small values"; here the effect
        // is measurable even though `step` is not the weak link it was in
        // Android, because the *threshold* is what changes: `step_scaled`
        // applies `value_threshold` in the scaled domain, so 0.002 there is
        // 2e-6 in real scale units, 1000x finer than on the raw value.
        let cfg = SpringConfig::recents_scale();
        assert_eq!(cfg.stiffness, 200.0);
        assert_eq!(cfg.damping_ratio, 0.75);
        let target = 0.975f32;

        let mut scaled = SpringSimulation::new(1.0, target, cfg);
        let mut t = 0.0f32;
        let mut settled = None;
        for _ in 0..4000 {
            let value = scaled.step_scaled(0.001);
            t += 0.001;
            // The scaled rest test, expressed in real units: 1000x finer.
            if (value - target).abs() <= cfg.value_threshold / RECENTS_SCALE_MULTIPLIER {
                settled = Some((t, value));
                break;
            }
        }
        let (t_scaled, value_scaled) = settled.expect("step_scaled must converge on 0.975");
        assert!((0.0..=1.5).contains(&value_scaled), "value {} out of range", value_scaled);
        assert!(
            (value_scaled - target).abs() <= cfg.value_threshold / RECENTS_SCALE_MULTIPLIER,
            "scaled settle residual {} exceeds the scaled threshold",
            (value_scaled - target).abs()
        );

        // The raw path, same profile and same threshold in its own units.
        let mut raw = SpringSimulation::new(1.0, target, cfg);
        let mut t_raw = 0.0f32;
        let mut raw_settled = None;
        for _ in 0..4000 {
            raw.step(0.001);
            t_raw += 0.001;
            if raw.is_at_rest() {
                raw_settled = Some(t_raw);
                break;
            }
        }
        let t_raw = raw_settled.expect("raw step must converge too");

        // The raw spring stops at its own (coarse) threshold; the scaled one
        // is still running and lands 1000x closer to the target. That
        // difference is the whole point of the multiplier.
        assert!(t_scaled > t_raw, "x1000 must settle later, not earlier: {} vs {}", t_scaled, t_raw);
        assert!(
            t_scaled - t_raw > 0.005,
            "the finer threshold must cost real time, got {} s later",
            t_scaled - t_raw
        );

        // `step_scaled` leaves the simulation in real units, so `is_at_rest`
        // and the value read back by the shell are unscaled.
        assert!(scaled.is_at_rest() || (scaled.value - target).abs() <= cfg.value_threshold);
        assert!(scaled.value < 1.5 && scaled.value > 0.0, "left unscaled: {}", scaled.value);
        assert!((scaled.target - target).abs() < 1e-6, "target left unscaled: {}", scaled.target);

        // Precision floor, asserted because it is the real effect. f32
        // resolution at 0.975 is 1.192e-7 (one ULP of 0.975f32), so a
        // threshold at or below that is unreachable: the value stalls on the
        // adjacent f32 and never reports rest. This is exactly the failure
        // RecentsDismissUtils.kt:1383 works around, and it is *not* cured by
        // the multiplier here, because scaling by a power of ten does not
        // change how many bits an f32 carries -- it only moves the threshold
        // relative to the ULP, which the scaled test above already covers.
        let tiny = SpringConfig { value_threshold: 1e-9, ..cfg };
        let mut raw_tiny = SpringSimulation::new(1.0, target, tiny);
        let mut raw_stuck = true;
        for _ in 0..4000 {
            if raw_tiny.step(0.001) {
                raw_stuck = false;
                break;
            }
        }
        assert!(raw_stuck, "a sub-ULP threshold is unreachable in f32; the value must stall");
        assert!(
            (raw_tiny.value - target).abs() >= f32::EPSILON * 0.5,
            "stalled one ULP away, not on the target: {}",
            (raw_tiny.value - target).abs()
        );

        // The scaled path carries the same 1.192e-7 f32 floor, so it also
        // stalls at a sub-ULP *raw* threshold -- but its own threshold is
        // evaluated in the scaled domain, where 1e-9 means 1e-12 of real
        // scale, and that is likewise below the floor. Recorded rather than
        // asserted as a cure: the multiplier's real, measurable benefit is the
        // 1000x finer effective threshold proven above, which is the part
        // that decides when a recents scale stops animating.
        let mut scaled_tiny = SpringSimulation::new(1.0, target, tiny);
        let mut scaled_stuck = true;
        for _ in 0..4000 {
            let before = scaled_tiny.value;
            scaled_tiny.step_scaled(0.001);
            if scaled_tiny.is_at_rest() && scaled_tiny.value != before {
                scaled_stuck = false;
                break;
            }
        }
        assert!(scaled_stuck, "the scaled path inherits the same f32 floor; recorded, not hidden");
    }

    #[test]
    fn test_paint_super_extreme_frame() {
        let _guard = crate::graphics::font::TEST_FONT_MUTEX.lock().unwrap();
        let (w, h) = (360, 640);
        let mut buf = vec![0u32; w * h];
        let mut sex = crate::compositor::super_extreme::SuperExtremeState::new();
        sex.volume_hud.trigger(70);

        // 1. Lock screen
        paint_frame(&mut buf, w, w, h, &DrmInteractiveState {
            power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
            super_extreme_state: Some(&sex),
            ..DrmInteractiveState::default()
        });
        assert!(buf.iter().any(|&p| p != 0xFF000000), "Lock screen must have ink");

        // 2. Camera preview
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::CameraPreview;
        sex.camera_preview.update_preview();
        paint_frame(&mut buf, w, w, h, &DrmInteractiveState {
            power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
            super_extreme_state: Some(&sex),
            ..DrmInteractiveState::default()
        });
        assert!(buf.iter().any(|&p| p == 0xFF22C55E), "Camera preview must have green terminal ink");

        // 3. Password screen
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::Password;
        sex.password_input.push('1');
        paint_frame(&mut buf, w, w, h, &DrmInteractiveState {
            power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
            super_extreme_state: Some(&sex),
            ..DrmInteractiveState::default()
        });

        // 4. Home screen
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::Home;
        paint_frame(&mut buf, w, w, h, &DrmInteractiveState {
            power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
            super_extreme_state: Some(&sex),
            ..DrmInteractiveState::default()
        });

        // 5. Power menu
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::PowerMenu;
        paint_frame(&mut buf, w, w, h, &DrmInteractiveState {
            power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
            super_extreme_state: Some(&sex),
            ..DrmInteractiveState::default()
        });
    }
}

