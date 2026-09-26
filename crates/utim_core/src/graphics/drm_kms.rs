//! Direct Rendering Manager (DRM) Kernel Mode Setting (KMS) Hardware Scanout.
//! Provides zero-dependency, bare-metal hardware display presentation for Universal Treble Linux
//! via Linux DRM dumb buffers and CRTC modesetting. Adheres strictly to GEMINI.md systems rules.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::png::RgbaImage;

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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerSearchHit {
    None,
    Focus,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeActionHit {
    RemoveFromHome,
    MoveToOtherPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalTabHit {
    None,
    SelectTab(usize),
    CloseTab(usize),
    AddTab,
}

/// Unified Pixel-Perfect Layout Geometry and Hit-Testing Engine.
/// Guarantees that hit-test areas match DRM KMS render boundaries with 100% mathematical consistency.
pub struct LauncherLayout;

impl LauncherLayout {
    pub const STATUS_BAR_H: f32 = 44.0;

    pub const CLOCK_Y: f32 = 120.0;
    pub const CLOCK_H: f32 = 72.0;

    pub const SEARCH_Y: f32 = 235.0;
    pub const SEARCH_H: f32 = 56.0;
    pub const SEARCH_PAD_X: f32 = 32.0;

    pub const ACTION_CHIPS_Y: f32 = 303.0;
    pub const ACTION_CHIPS_H: f32 = 40.0;

    pub const GRID_TOP_NORMAL: f32 = 325.0;
    pub const GRID_TOP_SELECTED: f32 = 395.0;
    pub const GRID_ROW_H: f32 = 115.0;
    pub const GRID_COLS: usize = 4;
    pub const ICON_SIZE: f32 = 64.0;

    pub const DOCK_H: f32 = 100.0;
    pub const DOCK_BOTTOM_MARGIN: f32 = 40.0;
    pub const DOCK_PAD_X: f32 = 20.0;
    pub const DOCK_SLOTS: usize = 5;

    pub const DRAWER_HANDLE_Y: f32 = 54.0;
    pub const DRAWER_SEARCH_X: f32 = 24.0;
    pub const DRAWER_SEARCH_Y: f32 = 70.0;
    pub const DRAWER_SEARCH_H: f32 = 52.0;
    pub const DRAWER_HEADER_Y: f32 = 134.0;
    pub const DRAWER_GRID_TOP: f32 = 198.0;

    pub const KB_H: f32 = 420.0;
    pub const KB_BOTTOM_MARGIN: f32 = 20.0;
    pub const KB_KEY_H: f32 = 65.0;

    pub const APP_BAR_Y: f32 = 48.0;
    pub const APP_BAR_H: f32 = 56.0;

    #[inline]
    pub fn status_bar_hit(y: f32) -> bool {
        (0.0..=Self::STATUS_BAR_H).contains(&y)
    }

    #[inline]
    pub fn home_search_hit(w: f32, x: f32, y: f32) -> bool {
        (Self::SEARCH_PAD_X..=(w - Self::SEARCH_PAD_X)).contains(&x)
            && (Self::SEARCH_Y..=(Self::SEARCH_Y + Self::SEARCH_H)).contains(&y)
    }

    #[inline]
    pub fn home_action_chips_hit(w: f32, x: f32, y: f32) -> Option<HomeActionHit> {
        if (Self::ACTION_CHIPS_Y..=(Self::ACTION_CHIPS_Y + Self::ACTION_CHIPS_H)).contains(&y) {
            let chip_w = (w - 76.0) / 2.0;
            if (28.0..=(28.0 + chip_w)).contains(&x) {
                Some(HomeActionHit::RemoveFromHome)
            } else if ((28.0 + chip_w + 20.0)..=(w - 28.0)).contains(&x) {
                Some(HomeActionHit::MoveToOtherPage)
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline]
    pub fn home_clock_hit(w: f32, x: f32, y: f32) -> bool {
        (Self::SEARCH_PAD_X..=(w - Self::SEARCH_PAD_X)).contains(&x)
            && ((Self::CLOCK_Y - 15.0)..=(Self::SEARCH_Y - 5.0)).contains(&y)
    }

    #[inline]
    pub fn max_home_rows(h: f32, has_selection: bool) -> usize {
        let grid_top = if has_selection {
            Self::GRID_TOP_SELECTED
        } else {
            Self::GRID_TOP_NORMAL
        };
        let dock_y = h - Self::DOCK_H - Self::DOCK_BOTTOM_MARGIN;
        let available = dock_y - grid_top - 93.0;
        if available > 0.0 {
            ((available / Self::GRID_ROW_H) as usize) + 1
        } else {
            0
        }
    }

    #[inline]
    pub fn max_drawer_rows(h: f32) -> usize {
        let available = h - Self::DRAWER_GRID_TOP - 93.0;
        if available > 0.0 {
            ((available / Self::GRID_ROW_H) as usize) + 1
        } else {
            0
        }
    }

    #[inline]
    pub fn home_page_dots_hit(w: f32, h: f32, x: f32, y: f32, total_pages: usize) -> Option<usize> {
        let dock_y = h - Self::DOCK_H - Self::DOCK_BOTTOM_MARGIN;
        let dots_y = dock_y - 20.0;
        if x >= 0.0 && x <= w && y >= dots_y - 15.0 && y <= dots_y + 15.0 && total_pages > 0 {
            let slot_w = w / total_pages as f32;
            let target = (x / slot_w).clamp(0.0, (total_pages - 1) as f32) as usize;
            Some(target)
        } else {
            None
        }
    }

    #[inline]
    pub fn home_dock_hit(w: f32, h: f32, x: f32, y: f32, num_slots: usize) -> Option<usize> {
        let dock_y = h - Self::DOCK_H - Self::DOCK_BOTTOM_MARGIN;
        let dock_w = w - Self::DOCK_PAD_X * 2.0;
        if x >= Self::DOCK_PAD_X
            && x <= (w - Self::DOCK_PAD_X)
            && y >= dock_y
            && y <= (dock_y + Self::DOCK_H)
        {
            let slot_w = dock_w / (num_slots.max(1) as f32);
            let slot = ((x - Self::DOCK_PAD_X) / slot_w).clamp(0.0, (num_slots - 1) as f32) as usize;
            Some(slot)
        } else {
            None
        }
    }

    #[inline]
    pub fn home_grid_hit(w: f32, h: f32, x: f32, y: f32, has_selection: bool) -> Option<usize> {
        Self::home_grid_hit_with_scroll(w, h, x, y, has_selection, 0.0)
    }

    pub fn home_grid_hit_with_scroll(
        w: f32,
        h: f32,
        x: f32,
        y: f32,
        has_selection: bool,
        scroll_offset: f32,
    ) -> Option<usize> {
        let max_rows = Self::max_home_rows(h, has_selection);
        if max_rows == 0 {
            return None;
        }
        let grid_top = if has_selection {
            Self::GRID_TOP_SELECTED
        } else {
            Self::GRID_TOP_NORMAL
        };
        let dock_y = h - Self::DOCK_H - Self::DOCK_BOTTOM_MARGIN;
        let grid_start_y = if has_selection {
            Self::ACTION_CHIPS_Y + Self::ACTION_CHIPS_H + 2.0
        } else {
            Self::SEARCH_Y + Self::SEARCH_H + 2.0
        };
        let grid_end_y = dock_y - 20.0;
        if y >= grid_start_y && y < grid_end_y {
            let rel_x = x - scroll_offset;
            if rel_x < 0.0 || rel_x >= w {
                return None;
            }
            let rel_y = y - (grid_top - Self::GRID_ROW_H / 2.0);
            let row = if rel_y >= 0.0 {
                (rel_y / Self::GRID_ROW_H) as usize
            } else {
                0
            };
            if row >= max_rows {
                return None;
            }
            let col_w = w / Self::GRID_COLS as f32;
            let col = (rel_x / col_w).clamp(0.0, (Self::GRID_COLS - 1) as f32) as usize;
            Some(row * Self::GRID_COLS + col)
        } else {
            None
        }
    }

    #[inline]
    pub fn drawer_handle_hit(drawer_y_offset: f32, y: f32) -> bool {
        y >= drawer_y_offset && y <= (drawer_y_offset + Self::DRAWER_HANDLE_Y + 12.0)
    }

    #[inline]
    pub fn drawer_search_hit(w: f32, x: f32, y: f32) -> DrawerSearchHit {
        Self::drawer_search_hit_with_offset(w, 0.0, x, y)
    }

    #[inline]
    pub fn drawer_search_hit_with_offset(w: f32, drawer_y_offset: f32, x: f32, y: f32) -> DrawerSearchHit {
        let search_w = w - Self::DRAWER_SEARCH_X * 2.0;
        let search_y = drawer_y_offset + Self::DRAWER_SEARCH_Y;
        if x >= Self::DRAWER_SEARCH_X
            && x <= (Self::DRAWER_SEARCH_X + search_w)
            && y >= search_y
            && y <= (search_y + Self::DRAWER_SEARCH_H)
        {
            if x >= (Self::DRAWER_SEARCH_X + search_w - 45.0) {
                DrawerSearchHit::Clear
            } else {
                DrawerSearchHit::Focus
            }
        } else {
            DrawerSearchHit::None
        }
    }

    #[inline]
    pub fn drawer_grid_hit(w: f32, h: f32, x: f32, y: f32) -> Option<usize> {
        Self::drawer_grid_hit_with_offset(w, h, 0.0, x, y)
    }

    pub fn drawer_grid_hit_with_offset(
        w: f32,
        h: f32,
        drawer_y_offset: f32,
        x: f32,
        y: f32,
    ) -> Option<usize> {
        if x < 0.0 || x >= w {
            return None;
        }
        let max_rows = Self::max_drawer_rows(h);
        if max_rows == 0 {
            return None;
        }
        let grid_start_y = drawer_y_offset + Self::DRAWER_HEADER_Y + 16.0;
        let grid_end_y = h - 35.0;
        if y >= grid_start_y && y < grid_end_y {
            let rel_y = y - (drawer_y_offset + Self::DRAWER_GRID_TOP - Self::GRID_ROW_H / 2.0);
            let row = if rel_y >= 0.0 {
                (rel_y / Self::GRID_ROW_H) as usize
            } else {
                0
            };
            if row >= max_rows {
                return None;
            }
            let col_w = w / Self::GRID_COLS as f32;
            let col = (x / col_w).clamp(0.0, (Self::GRID_COLS - 1) as f32) as usize;
            Some(row * Self::GRID_COLS + col)
        } else {
            None
        }
    }

    #[inline]
    pub fn quick_tile_hit(w: f32, x: f32, y: f32) -> Option<usize> {
        let tile_w = (w - 72.0 - 20.0) / 2.0;
        let tile_h = 70.0;
        let tile_spacing_y = 14.0;
        let start_y = 155.0;
        let stride_y = tile_h + tile_spacing_y;
        if (start_y..start_y + 4.0 * stride_y).contains(&y) {
            let rel_y = y - start_y;
            let row = (rel_y / stride_y) as usize;
            let row_off = rel_y - (row as f32 * stride_y);
            if row < 4 && row_off <= tile_h {
                if (36.0..36.0 + tile_w).contains(&x) {
                    Some(row * 2)
                } else if (36.0 + tile_w + 20.0..36.0 + 2.0 * tile_w + 20.0).contains(&x) {
                    Some(row * 2 + 1)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline]
    pub fn app_bar_back_hit(x: f32, y: f32) -> bool {
        (26.0..=125.0).contains(&x) && ((Self::APP_BAR_Y + 8.0)..=(Self::APP_BAR_Y + 48.0)).contains(&y)
    }

    #[inline]
    pub fn app_bar_close_hit(w: f32, x: f32, y: f32) -> bool {
        ((w - 85.0)..=(w - 25.0)).contains(&x) && ((Self::APP_BAR_Y + 8.0)..=(Self::APP_BAR_Y + 48.0)).contains(&y)
    }

    #[inline]
    pub fn bottom_nav_pill_hit(h: f32, y: f32) -> bool {
        y >= (h - 35.0)
    }

    pub fn terminal_tab_hit(
        x: f32,
        y: f32,
        num_tabs: usize,
        active_tab_idx: usize,
    ) -> TerminalTabHit {
        if !(120.0..=175.0).contains(&y) {
            return TerminalTabHit::None;
        }
        let start_x = 36.0;
        let tab_w = 200.0;
        let spacing = 10.0;

        for i in 0..num_tabs {
            let tab_x = start_x + i as f32 * (tab_w + spacing);
            if x >= tab_x && x < tab_x + tab_w {
                if i == active_tab_idx && num_tabs > 1 && x >= (tab_x + tab_w - 35.0) {
                    return TerminalTabHit::CloseTab(i);
                } else {
                    return TerminalTabHit::SelectTab(i);
                }
            }
        }
        if num_tabs < 4 {
            let plus_x = start_x + num_tabs as f32 * (tab_w + spacing);
            if x >= plus_x && x <= (plus_x + 60.0) {
                return TerminalTabHit::AddTab;
            }
        }
        TerminalTabHit::None
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
        draw_text_weighted(buf, stride, w, h, 24, 14, state.time_str, 0xFFFFFFFF, 2, FontWeight::Medium);

        // Status Icons (Right side of status bar)
        let icon_right = w - 24;
        draw_battery(buf, stride, w, h, icon_right - 40, 14, 98);
        if state.quick_tiles_active[0] {
            draw_wifi(buf, stride, w, h, icon_right - 80, 14);
        }
        let rat_label = if state.quick_tiles_active[1] { "5G" } else { "OFF" };
        let rat_color = if state.quick_tiles_active[1] { 0xFFFFFFFF } else { 0xFF888888 };
        draw_text_weighted(buf, stride, w, h, icon_right - 135, 14, rat_label, rat_color, 2, FontWeight::Bold);

        if state.is_locked {
            // Lock Screen UI with Lawnchair 17 / Pixel Material You Clock
            let center_x = w / 2;
            let center_y = h / 3;
            draw_material_you_clock(buf, stride, w, h, center_x, center_y, state.time_str, 0xFFFFFFFF, 96);
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
            draw_material_you_clock(buf, stride, w, h, 36 + 65, 80, state.time_str, 0xFFFFFFFF, 46);
            draw_text_weighted(buf, stride, w, h, 36, 115, "Tue, Sep 22 | Universal Treble GSI", 0xFF94A3B8, 2, FontWeight::Medium);

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
                draw_text_weighted(buf, stride, w, h, tx + 18, ty + 18, name, text_color, 2, FontWeight::Medium);
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
            draw_text_weighted(buf, stride, w, h, 36, notif_y, "NOTIFICATIONS", 0xFF64748B, 2, FontWeight::Bold);

            // Notification Card 1
            draw_rounded_rect(buf, stride, w, h, 36, notif_y + 26, w - 72, 85, 18, 0xFF1E293B);
            draw_text_weighted(buf, stride, w, h, 56, notif_y + 40, "UTIM PID 1 & UTLC Wayland", 0xFFF8FAFC, 2, FontWeight::Medium);
            draw_text(buf, stride, w, h, 56, notif_y + 68, "Interactive mobile compositor active with < 8ms input response", 0xFF94A3B8, 1);

            // Notification Card 2
            draw_rounded_rect(buf, stride, w, h, 36, notif_y + 125, w - 72, 85, 18, 0xFF1E293B);
            draw_text_weighted(buf, stride, w, h, 56, notif_y + 139, "Direct DRM KMS Scanout", 0xFFF8FAFC, 2, FontWeight::Medium);
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
            draw_text_centered_weighted(buf, stride, w, h, w / 2, bar_y + 18, app_name, 0xFFFFFFFF, 2, FontWeight::Bold);

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
                // Interactive Mobile Settings Page with Search Input Bar
                let s_bar_y = content_y + 12;
                let s_bar_h = 44;
                let s_bar_x = 32;
                let s_bar_w = w - 64;
                let s_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, s_bar_x - 1, s_bar_y - 1, s_bar_w + 2, s_bar_h + 2, 14, s_border);
                draw_rounded_rect(buf, stride, w, h, s_bar_x, s_bar_y, s_bar_w, s_bar_h, 13, 0xFF1E293B);
                let s_disp = if !state.app_input.is_empty() { state.app_input } else { "Search settings..." };
                let s_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, s_bar_x + 16, s_bar_y + 13, s_disp, s_col, 2);
                if state.app_input_focused {
                    let cur_x = s_bar_x + 16 + (state.app_input.len() * 12).min(s_bar_w - 30);
                    draw_rect(buf, stride, w, h, cur_x, s_bar_y + 11, 2, 22, 0xFF38BDF8);
                }

                let mut card_y = s_bar_y + s_bar_h + 14;
                let cards = [
                    ("Network & Internet", "Wi-Fi, Mobile, Hotspot, VPN"),
                    ("Connected Devices", "Bluetooth, Android HAL bridge"),
                    ("Display & Graphics", "1080x2400 @ 120Hz Direct DRM KMS"),
                    ("Sound & Multimedia", "PipeWire spa-droid Audio"),
                    ("Storage", "4.00 GB ext4 System GSI Image"),
                    ("Battery", "98% - Mobile Power Governor active"),
                    ("About Phone", "Universal Treble Linux (Android 14 GKI)"),
                ];
                let query = state.app_input.to_lowercase();
                for (title, desc) in cards {
                    if !query.is_empty() && !title.to_lowercase().contains(&query) && !desc.to_lowercase().contains(&query) {
                        continue;
                    }
                    if card_y + 70 < content_y + content_h {
                        draw_rounded_rect(buf, stride, w, h, 32, card_y, w - 64, 60, 12, 0xFF1E293B);
                        draw_text(buf, stride, w, h, 48, card_y + 12, title, 0xFFF8FAFC, 2);
                        draw_text(buf, stride, w, h, 48, card_y + 36, desc, 0xFF94A3B8, 1);
                        card_y += 72;
                    }
                }
            } else if app_name == "Browser" || app_name == "Web" || app_name.contains("Browser") || app_name == "Firefox" {
                // Interactive Modern Mobile Android Browser Window with Active Omnibox
                let bar_top = content_y + 10;
                let bar_h = 46;
                let pad = 16;
                let url_w = w - pad * 2 - 32;

                let url_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, pad + 15, bar_top - 1, url_w + 2, bar_h + 2, 16, url_border);
                draw_rounded_rect(buf, stride, w, h, pad + 16, bar_top, url_w, bar_h, 15, 0xFF1E293B);

                // SSL Padlock indicator (Emerald)
                draw_rounded_rect(buf, stride, w, h, pad + 28, bar_top + 14, 16, 16, 4, 0xFF10B981);
                draw_text(buf, stride, w, h, pad + 32, bar_top + 15, "*", 0xFFFFFFFF, 1);

                // URL display & cursor
                let b_disp = if !state.app_input.is_empty() {
                    state.app_input
                } else if state.app_input_focused {
                    "Search or type web address"
                } else {
                    "https://www.google.com"
                };
                let b_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, pad + 54, bar_top + 14, b_disp, b_col, 2);
                if state.app_input_focused {
                    let cur_x = pad + 54 + (state.app_input.len() * 12).min(url_w.saturating_sub(80));
                    draw_rect(buf, stride, w, h, cur_x, bar_top + 12, 2, 22, 0xFF38BDF8);
                }

                // Tab count badge [ 1 ] and Reload icon
                draw_rounded_rect(buf, stride, w, h, pad + 16 + url_w - 60, bar_top + 11, 24, 24, 6, 0xFF334155);
                draw_text(buf, stride, w, h, pad + 16 + url_w - 53, bar_top + 15, "1", 0xFFFFFFFF, 1);
                draw_text(buf, stride, w, h, pad + 16 + url_w - 28, bar_top + 14, "O", 0xFF94A3B8, 2);

                // Web View Viewport Container
                let toolbar_h = 52;
                let page_y = bar_top + bar_h + 12;
                let page_h = content_h.saturating_sub(bar_h + toolbar_h + 30);
                draw_rounded_rect(buf, stride, w, h, pad + 16, page_y, url_w, page_h, 16, 0xFF0F172A);

                if !state.app_input.is_empty() {
                    // Active search result view
                    draw_rounded_rect(buf, stride, w, h, pad + 32, page_y + 24, url_w - 32, 42, 10, 0xFF1E293B);
                    draw_text(buf, stride, w, h, pad + 44, page_y + 36, "Web results for: ", 0xFF94A3B8, 2);
                    draw_text(buf, stride, w, h, pad + 210, page_y + 36, state.app_input, 0xFF38BDF8, 2);

                    // Card 1
                    draw_rounded_rect(buf, stride, w, h, pad + 32, page_y + 80, url_w - 32, 90, 12, 0xFF1E293B);
                    draw_text(buf, stride, w, h, pad + 48, page_y + 94, state.app_input, 0xFF60A5FA, 2);
                    draw_text(buf, stride, w, h, pad + 48, page_y + 118, "https://www.google.com/search", 0xFF34D399, 1);
                    draw_text(buf, stride, w, h, pad + 48, page_y + 138, "Top match and official verified web destination.", 0xFF94A3B8, 1);

                    // Card 2
                    draw_rounded_rect(buf, stride, w, h, pad + 32, page_y + 184, url_w - 32, 90, 12, 0xFF1E293B);
                    draw_text(buf, stride, w, h, pad + 48, page_y + 198, "Wikipedia - Free Encyclopedia", 0xFF60A5FA, 2);
                    draw_text(buf, stride, w, h, pad + 48, page_y + 222, "https://en.wikipedia.org/wiki", 0xFF34D399, 1);
                    draw_text(buf, stride, w, h, pad + 48, page_y + 242, "Overview, history, documentation, and references.", 0xFF94A3B8, 1);
                } else {
                    // Google / Mobile Web Start Page
                    let center_x = w / 2;
                    let g_y = page_y + 40;
                    // Google stylized multi-color logo
                    draw_text(buf, stride, w, h, center_x - 84, g_y, "G", 0xFF4285F4, 4);
                    draw_text(buf, stride, w, h, center_x - 56, g_y, "o", 0xFFEA4335, 4);
                    draw_text(buf, stride, w, h, center_x - 28, g_y, "o", 0xFFFBBC05, 4);
                    draw_text(buf, stride, w, h, center_x,      g_y, "g", 0xFF4285F4, 4);
                    draw_text(buf, stride, w, h, center_x + 28, g_y, "l", 0xFF34A853, 4);
                    draw_text(buf, stride, w, h, center_x + 48, g_y, "e", 0xFFEA4335, 4);

                    // Start page in-page search bar
                    let s_box_y = g_y + 60;
                    let s_box_w = (url_w - 40).min(500);
                    let s_box_x = center_x - s_box_w / 2;
                    draw_rounded_rect(buf, stride, w, h, s_box_x, s_box_y, s_box_w, 46, 23, 0xFF1E293B);
                    draw_text(buf, stride, w, h, s_box_x + 20, s_box_y + 14, "G", 0xFF4285F4, 2);
                    draw_text(buf, stride, w, h, s_box_x + 44, s_box_y + 14, "Search or type web address", 0xFF64748B, 2);

                    // Shortcuts Grid (2 rows x 3 cols)
                    let sc_start_y = s_box_y + 70;
                    let shortcuts = [
                        ("Google", 0xFF4285F4, "G"),
                        ("YouTube", 0xFFFF0000, "Y"),
                        ("Wikipedia", 0xFF475569, "W"),
                        ("Reddit", 0xFFFF4500, "R"),
                        ("GitHub", 0xFF24292F, "g"),
                        ("Weather", 0xFF0284C7, "W"),
                    ];
                    let sc_cols = 3;
                    let sc_col_w = (url_w - 40) / sc_cols;
                    for (idx, (sc_title, sc_color, sc_glyph)) in shortcuts.iter().enumerate() {
                        let sc_c = idx % sc_cols;
                        let sc_r = idx / sc_cols;
                        let sc_x = pad + 20 + sc_c * sc_col_w + sc_col_w / 2;
                        let sc_y = sc_start_y + sc_r * 72;
                        if sc_y + 50 < page_y + page_h {
                            draw_rounded_rect(buf, stride, w, h, sc_x - 22, sc_y, 44, 44, 22, *sc_color);
                            draw_text_centered(buf, stride, w, h, sc_x, sc_y + 12, sc_glyph, 0xFFFFFFFF, 2);
                            draw_text_centered(buf, stride, w, h, sc_x, sc_y + 50, sc_title, 0xFF94A3B8, 1);
                        }
                    }
                }

                // Bottom Mobile Browser Toolbar (Back, Forward, Home, Tabs, Menu)
                let tb_y = page_y + page_h + 8;
                draw_rounded_rect(buf, stride, w, h, pad + 16, tb_y, url_w, toolbar_h, 14, 0xFF1E293B);
                let tb_slots = ["<", ">", "H", "1", ":"];
                let tb_slot_w = url_w / tb_slots.len();
                for (t_idx, symbol) in tb_slots.iter().enumerate() {
                    let tx = pad + 16 + t_idx * tb_slot_w + tb_slot_w / 2;
                    let t_col = if t_idx == 2 { 0xFF38BDF8 } else { 0xFF94A3B8 };
                    draw_text_centered(buf, stride, w, h, tx, tb_y + 16, symbol, t_col, 2);
                }
            } else if app_name == "Messages" {
                // Interactive Messages App with live chat and message input bar
                let header_y = content_y + 12;
                draw_rounded_rect(buf, stride, w, h, 32, header_y, w - 64, 40, 10, 0xFF1E293B);
                draw_text_centered(buf, stride, w, h, w / 2, header_y + 11, "Treble Carrier (SIM 1 - 4G LTE Active)", 0xFF38BDF8, 2);

                // Chat bubble list
                let mut bubble_y = header_y + 54;
                let max_bubble_w = w - 120;

                // Incoming system bubble
                draw_rounded_rect(buf, stride, w, h, 32, bubble_y, max_bubble_w, 48, 12, 0xFF1E293B);
                draw_text(buf, stride, w, h, 48, bubble_y + 14, "Treble: Welcome! Tap below to type a message.", 0xFFE2E8F0, 2);
                bubble_y += 60;

                // User sent messages
                for msg in state.messages_list.iter() {
                    if bubble_y + 50 < content_y + content_h - 60 {
                        let msg_w = (msg.len() * 12 + 32).min(max_bubble_w);
                        let bubble_x = (w - 32).saturating_sub(msg_w);
                        draw_rounded_rect(buf, stride, w, h, bubble_x, bubble_y, msg_w, 44, 12, 0xFF0284C7);
                        draw_text(buf, stride, w, h, bubble_x + 16, bubble_y + 12, msg, 0xFFFFFFFF, 2);
                        bubble_y += 54;
                    }
                }

                // Bottom Message Input Box + Send Button
                let msg_box_h = 46;
                let msg_box_y = content_y + content_h.saturating_sub(msg_box_h + 12);
                let msg_box_x = 32;
                let send_btn_w = 64;
                let msg_box_w = w - 64 - send_btn_w - 12;

                let msg_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, msg_box_x - 1, msg_box_y - 1, msg_box_w + 2, msg_box_h + 2, 14, msg_border);
                draw_rounded_rect(buf, stride, w, h, msg_box_x, msg_box_y, msg_box_w, msg_box_h, 13, 0xFF1E293B);
                let m_disp = if !state.app_input.is_empty() { state.app_input } else { "Type a message..." };
                let m_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, msg_box_x + 16, msg_box_y + 14, m_disp, m_col, 2);
                if state.app_input_focused {
                    let cur_x = msg_box_x + 16 + (state.app_input.len() * 12).min(msg_box_w.saturating_sub(30));
                    draw_rect(buf, stride, w, h, cur_x, msg_box_y + 12, 2, 22, 0xFF38BDF8);
                }

                let send_x = msg_box_x + msg_box_w + 12;
                draw_rounded_rect(buf, stride, w, h, send_x, msg_box_y, send_btn_w, msg_box_h, 13, 0xFF0284C7);
                draw_text_centered(buf, stride, w, h, send_x + send_btn_w / 2, msg_box_y + 14, "Send", 0xFFFFFFFF, 2);
            } else if app_name == "Phone" {
                // Interactive Phone Dialer
                let num_y = content_y + 12;
                let num_h = 48;
                let num_x = 32;
                let num_w = w - 64;
                let num_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, num_x - 1, num_y - 1, num_w + 2, num_h + 2, 14, num_border);
                draw_rounded_rect(buf, stride, w, h, num_x, num_y, num_w, num_h, 13, 0xFF1E293B);
                let p_disp = if !state.app_input.is_empty() { state.app_input } else { "Enter phone number..." };
                let p_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, num_x + 16, num_y + 14, p_disp, p_col, 2);
                if state.app_input_focused {
                    let cur_x = num_x + 16 + (state.app_input.len() * 12).min(num_w.saturating_sub(30));
                    draw_rect(buf, stride, w, h, cur_x, num_y + 13, 2, 22, 0xFF38BDF8);
                }

                // Call status / dial info
                draw_text_centered(buf, stride, w, h, w / 2, num_y + 80, "Universal Cellular RIL Bridge", 0xFF10B981, 2);
                draw_rounded_rect(buf, stride, w, h, (w - 180) / 2, num_y + 120, 180, 52, 14, 0xFF10B981);
                draw_text_centered(buf, stride, w, h, w / 2, num_y + 135, "Call", 0xFFFFFFFF, 3);
            } else if app_name == "Contacts" {
                // Interactive Contacts App
                let c_bar_y = content_y + 12;
                let c_bar_h = 44;
                let c_bar_x = 32;
                let c_bar_w = w - 64;
                let c_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, c_bar_x - 1, c_bar_y - 1, c_bar_w + 2, c_bar_h + 2, 14, c_border);
                draw_rounded_rect(buf, stride, w, h, c_bar_x, c_bar_y, c_bar_w, c_bar_h, 13, 0xFF1E293B);
                let c_disp = if !state.app_input.is_empty() { state.app_input } else { "Search contacts..." };
                let c_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, c_bar_x + 16, c_bar_y + 13, c_disp, c_col, 2);
                if state.app_input_focused {
                    let cur_x = c_bar_x + 16 + (state.app_input.len() * 12).min(c_bar_w.saturating_sub(30));
                    draw_rect(buf, stride, w, h, cur_x, c_bar_y + 11, 2, 22, 0xFF38BDF8);
                }

                let contacts = [
                    ("Emergency Services", "112 / 911"),
                    ("Voice Mailbox", "*86"),
                    ("Treble Support", "+1 800 555 0199"),
                ];
                let mut cy = c_bar_y + c_bar_h + 16;
                for (name, num) in contacts {
                    if cy + 60 < content_y + content_h {
                        draw_rounded_rect(buf, stride, w, h, 32, cy, w - 64, 52, 12, 0xFF1E293B);
                        draw_text(buf, stride, w, h, 48, cy + 10, name, 0xFFF8FAFC, 2);
                        draw_text(buf, stride, w, h, 48, cy + 30, num, 0xFF94A3B8, 1);
                        cy += 62;
                    }
                }
            } else if app_name == "Files" {
                // Interactive Files App
                let f_bar_y = content_y + 12;
                let f_bar_h = 44;
                let f_bar_x = 32;
                let f_bar_w = w - 64;
                let f_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, f_bar_x - 1, f_bar_y - 1, f_bar_w + 2, f_bar_h + 2, 14, f_border);
                draw_rounded_rect(buf, stride, w, h, f_bar_x, f_bar_y, f_bar_w, f_bar_h, 13, 0xFF1E293B);
                let f_disp = if !state.app_input.is_empty() { state.app_input } else { "Filter files (/root)..." };
                let f_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, f_bar_x + 16, f_bar_y + 13, f_disp, f_col, 2);
                if state.app_input_focused {
                    let cur_x = f_bar_x + 16 + (state.app_input.len() * 12).min(f_bar_w.saturating_sub(30));
                    draw_rect(buf, stride, w, h, cur_x, f_bar_y + 11, 2, 22, 0xFF38BDF8);
                }

                let dirs = [
                    ("Documents", "Directory"),
                    ("Downloads", "Directory"),
                    ("Pictures", "Directory"),
                    ("Music", "Directory"),
                ];
                let mut fy = f_bar_y + f_bar_h + 16;
                for (name, kind) in dirs {
                    if fy + 60 < content_y + content_h {
                        draw_rounded_rect(buf, stride, w, h, 32, fy, w - 64, 52, 12, 0xFF1E293B);
                        draw_text(buf, stride, w, h, 48, fy + 10, name, 0xFFF8FAFC, 2);
                        draw_text(buf, stride, w, h, 48, fy + 30, kind, 0xFF94A3B8, 1);
                        fy += 62;
                    }
                }
            } else {
                // Generic Modern Mobile App Screen with Universal Text Input Bar
                let g_bar_y = content_y + 12;
                let g_bar_h = 44;
                let g_bar_x = 32;
                let g_bar_w = w - 64;
                let g_border = if state.app_input_focused { 0xFF38BDF8 } else { 0xFF334155 };
                draw_rounded_rect(buf, stride, w, h, g_bar_x - 1, g_bar_y - 1, g_bar_w + 2, g_bar_h + 2, 14, g_border);
                draw_rounded_rect(buf, stride, w, h, g_bar_x, g_bar_y, g_bar_w, g_bar_h, 13, 0xFF1E293B);
                let g_disp = if !state.app_input.is_empty() { state.app_input } else { "Search or enter text..." };
                let g_col = if !state.app_input.is_empty() { 0xFFFFFFFF } else { 0xFF94A3B8 };
                draw_text(buf, stride, w, h, g_bar_x + 16, g_bar_y + 13, g_disp, g_col, 2);
                if state.app_input_focused {
                    let cur_x = g_bar_x + 16 + (state.app_input.len() * 12).min(g_bar_w.saturating_sub(30));
                    draw_rect(buf, stride, w, h, cur_x, g_bar_y + 11, 2, 22, 0xFF38BDF8);
                }

                draw_text_centered(buf, stride, w, h, w / 2, content_y + 90, app_name, 0xFF38BDF8, 4);
                draw_text_centered(buf, stride, w, h, w / 2, content_y + 140, "Universal Treble Linux Mobile Application", 0xFF94A3B8, 2);
                draw_rounded_rect(buf, stride, w, h, (w - 200) / 2, content_y + 190, 200, 50, 14, 0xFF3B82F6);
                draw_text_centered(buf, stride, w, h, w / 2, content_y + 205, "Action Ready", 0xFFFFFFFF, 2);
            }

            // Bottom Navigation Pill
            let nav_y = h - 20;
            let nav_w = 140;
            let nav_x = (w - nav_w) / 2;
            draw_rounded_rect(buf, stride, w, h, nav_x, nav_y, nav_w, 5, 2, 0xFFFFFFFF);
        } else {
            // 5. Foundational Layer: Home Screen
            let widget_y = LauncherLayout::CLOCK_Y as usize;
            draw_material_you_clock(buf, stride, w, h, w / 2, widget_y + 35, state.time_str, 0xFFFFFFFF, 72);
            draw_text_centered_weighted(
                buf,
                stride,
                w,
                h,
                w / 2,
                widget_y + 85,
                "Tue, Sep 22  |  28 C Sunny",
                0xFF88A0C0,
                2,
                FontWeight::Medium,
            );

            // 6. Google / Treble Search Pill Widget
            let search_y = LauncherLayout::SEARCH_Y as usize;
            let search_h = LauncherLayout::SEARCH_H as usize;
            let search_pad_x = LauncherLayout::SEARCH_PAD_X as usize;
            let search_w = w - search_pad_x * 2;
            let search_x = search_pad_x;
            let pill_bg = if state.search_active { 0xFF334155 } else { 0xFF2A3345 };
            let pill_border = if state.search_active { 0xFF38BDF8 } else { 0xFF475569 };
            draw_rounded_rect(buf, stride, w, h, search_x - 2, search_y - 2, search_w + 4, search_h + 4, (search_h + 4) / 2, pill_border);
            draw_rounded_rect(buf, stride, w, h, search_x, search_y, search_w, search_h, search_h / 2, pill_bg);
            draw_text(buf, stride, w, h, search_x + 20, search_y + 16, "G", 0xFF4285F4, 3);

            if state.search_active {
                let disp_query = if state.search_query.is_empty() {
                    "Type to search..."
                } else {
                    state.search_query
                };
                let q_color = if state.search_query.is_empty() { 0xFF94A3B8 } else { 0xFFFFFFFF };
                draw_text(buf, stride, w, h, search_x + 55, search_y + 18, disp_query, q_color, 2);
                let cur_x = search_x + 55 + (if state.search_query.is_empty() { 0 } else { text_width(state.search_query, 2) });
                draw_rect(buf, stride, w, h, cur_x, search_y + 16, 2, 24, 0xFF38BDF8);
            } else {
                draw_text(buf, stride, w, h, search_x + 55, search_y + 18, "Search apps, web...", 0xFF8A99AD, 2);
                draw_text(buf, stride, w, h, search_x + search_w - 36, search_y + 16, "*", 0xFFEA4335, 3);
            }

            // Edit Mode Action Bar (if an icon is selected on Home Screen)
            let has_selection = state.selected_icon_id.is_some();
            if has_selection {
                let act_y = LauncherLayout::ACTION_CHIPS_Y as usize;
                let act_h = LauncherLayout::ACTION_CHIPS_H as usize;
                let chip_w = (w - 76) / 2;
                // Remove from Home chip
                draw_rounded_rect(buf, stride, w, h, 28, act_y, chip_w, act_h, 12, 0xDD7F1D1D);
                draw_rounded_rect(buf, stride, w, h, 27, act_y - 1, chip_w + 2, act_h + 2, 13, 0xFFEF4444);
                draw_text_centered_weighted(buf, stride, w, h, 28 + chip_w / 2, act_y + 12, "X Remove from Home", 0xFFFFFFFF, 2, FontWeight::Medium);

                // Move to Page chip
                let chip2_x = 28 + chip_w + 20;
                let move_label = if state.home_page == 0 { "-> Move to Page 2" } else { "<- Move to Page 1" };
                draw_rounded_rect(buf, stride, w, h, chip2_x, act_y, chip_w, act_h, 12, 0xDD1E3A8A);
                draw_rounded_rect(buf, stride, w, h, chip2_x - 1, act_y - 1, chip_w + 2, act_h + 2, 13, 0xFF3B82F6);
                draw_text_centered_weighted(buf, stride, w, h, chip2_x + chip_w / 2, act_y + 12, move_label, 0xFFFFFFFF, 2, FontWeight::Medium);
            }

            // 7. App Grid Icons (4 columns x dynamic rows)
            let grid_top = if has_selection {
                LauncherLayout::GRID_TOP_SELECTED as usize
            } else {
                LauncherLayout::GRID_TOP_NORMAL as usize
            };
            let cols = LauncherLayout::GRID_COLS;
            let col_width = w / cols;
            let icon_size = LauncherLayout::ICON_SIZE as usize;
            let grid_row_h = LauncherLayout::GRID_ROW_H as usize;

            let apps: &[AppGridItem] = state.grid_apps;

            let dock_h = LauncherLayout::DOCK_H as usize;
            let dock_bottom_margin = LauncherLayout::DOCK_BOTTOM_MARGIN as usize;
            let dock_y = h - dock_h - dock_bottom_margin;
            let max_rows = LauncherLayout::max_home_rows(h as f32, has_selection);
            let max_apps = max_rows * cols;
            let scroll_dx = state.home_scroll_offset as i32;

            for (idx, app) in apps.iter().take(max_apps).enumerate() {
                let row = idx / cols;
                let col = idx % cols;
                let cx_base = (col * col_width + col_width / 2) as i32;
                let cx = cx_base + scroll_dx;
                let cy = (grid_top + row * grid_row_h) as i32;

                let ix = cx - (icon_size as i32) / 2;
                let iy = cy - (icon_size as i32) / 2;

                if ix + (icon_size as i32) <= 0 || ix >= w as i32 {
                    continue;
                }

                let is_selected = state.selected_icon_id == Some(app.id);
                if is_selected {
                    draw_rounded_rect_i32(buf, stride, w, h, ix - 5, iy - 5, icon_size + 10, icon_size + 10, 20, 0xFF38BDF8);
                    draw_rounded_rect_i32(buf, stride, w, h, ix - 2, iy - 2, icon_size + 4, icon_size + 4, 18, 0xFF0B1120);
                }

                draw_rounded_rect_i32(buf, stride, w, h, ix, iy, icon_size, icon_size, 16, app.color);
                match app.icon {
                    Some(icon) => {
                        draw_icon_bitmap_i32(buf, stride, w, h, ix, iy, icon_size, icon_size, 16, icon);
                    }
                    None => {
                        draw_text_centered_i32(buf, stride, w, h, cx, cy - 8, app.glyph, 0xFFFFFFFF, 3);
                    }
                }
                draw_text_centered_weighted_i32(buf, stride, w, h, cx, cy + 42, app.name, 0xFFE2E8F0, 2, FontWeight::Medium);
            }

            // Multi-Page Indicator Dots with Smooth Sliding Active Pill
            let dots_y = dock_y - 20;
            let total_pages = state.total_home_pages.max(1);
            let dot_spacing = 22;
            let total_dots_w = (total_pages - 1) * dot_spacing + 24;
            let start_dot_x = (w - total_dots_w) / 2;
            for p in 0..total_pages {
                let dx = start_dot_x + p * dot_spacing;
                draw_rounded_rect(buf, stride, w, h, dx + 8, dots_y, 6, 6, 3, 0x66FFFFFF);
            }
            let fractional_page = (state.home_page as f32 - (state.home_scroll_offset / w as f32))
                .clamp(0.0, (total_pages - 1) as f32);
            let active_dot_x = (start_dot_x as f32 + fractional_page * (dot_spacing as f32)) as usize;
            draw_rounded_rect(buf, stride, w, h, active_dot_x, dots_y, 22, 6, 3, 0xFFFFFFFF);

            // 8. Persistent Hotseat Dock at Bottom
            let dock_x = LauncherLayout::DOCK_PAD_X as usize;
            let dock_w = w - dock_x * 2;

            draw_rounded_rect(buf, stride, w, h, dock_x, dock_y, dock_w, dock_h, 32, 0xFF182236);

            let fallback_dock = [
                AppGridItem { id: "phone", name: "Phone", color: 0xFF10B981, glyph: "P", icon: None },
                AppGridItem { id: "messages", name: "Messages", color: 0xFF3B82F6, glyph: "M", icon: None },
                AppGridItem { id: "apps", name: "Apps", color: 0xFF475569, glyph: ":", icon: None },
                AppGridItem { id: "browser", name: "Browser", color: 0xFF06B6D4, glyph: "B", icon: None },
                AppGridItem { id: "camera", name: "Camera", color: 0xFFF43F5E, glyph: "C", icon: None },
            ];
            let dock_apps: &[AppGridItem] = if !state.dock_apps.is_empty() {
                state.dock_apps
            } else {
                &fallback_dock[..]
            };

            let dock_col_w = dock_w / dock_apps.len().max(1);
            for (i, app) in dock_apps.iter().enumerate() {
                let cx = dock_x + i * dock_col_w + dock_col_w / 2;
                let cy = dock_y + dock_h / 2;
                let d_size = 54;
                let dx = cx.saturating_sub(d_size / 2);
                let dy = cy.saturating_sub(d_size / 2);
                draw_rounded_rect(buf, stride, w, h, dx, dy, d_size, d_size, 16, app.color);
                match app.icon {
                    Some(icon) => {
                        draw_icon_bitmap(buf, stride, w, h, dx, dy, d_size, d_size, 16, icon);
                    }
                    None => {
                        draw_text_centered(buf, stride, w, h, cx, cy, app.glyph, 0xFFFFFFFF, 2);
                    }
                }
            }

            // 9. Gesture Navigation Bar (Pill at bottom)
            let nav_y = h - 20;
            let nav_w = 140;
            let nav_x = (w - nav_w) / 2;
            draw_rounded_rect(buf, stride, w, h, nav_x, nav_y, nav_w, 5, 2, 0xFFFFFFFF);

            // Layered App Drawer Overlay (Lawnchair 17 sliding drawer with frosted blur)
            if state.app_drawer_open || state.drawer_progress > 0.001 {
                let drawer_prog = if state.drawer_progress > 0.001 {
                    state.drawer_progress.clamp(0.0, 1.0)
                } else if state.app_drawer_open {
                    1.0
                } else {
                    0.0
                };
                let drawer_y_offset = ((1.0 - drawer_prog) * h as f32) as usize;

                if drawer_y_offset < h {
                    let dh = h - drawer_y_offset;
                    apply_frosted_blur_region(buf, stride, w, drawer_y_offset, h);
                    draw_rect(buf, stride, w, h, 0, drawer_y_offset, w, dh, 0xCC080D1A);

                    // Top Status Bar text (visible over blur)
                    draw_text_weighted(buf, stride, w, h, 24, drawer_y_offset + 14, state.time_str, 0xFFFFFFFF, 2, FontWeight::Medium);
                    let icon_right = w - 24;
                    draw_battery(buf, stride, w, h, icon_right - 40, drawer_y_offset + 14, 98);
                    if state.quick_tiles_active[0] {
                        draw_wifi(buf, stride, w, h, icon_right - 80, drawer_y_offset + 14);
                    }

                    // Pull handle at top
                    draw_rounded_rect(buf, stride, w, h, (w - 72) / 2, drawer_y_offset + LauncherLayout::DRAWER_HANDLE_Y as usize, 72, 5, 2, 0xFF64748B);

                    // PixelUI App Drawer Search Bar
                    let search_y = drawer_y_offset + LauncherLayout::DRAWER_SEARCH_Y as usize;
                    let search_h = LauncherLayout::DRAWER_SEARCH_H as usize;
                    let search_x = LauncherLayout::DRAWER_SEARCH_X as usize;
                    let search_w = w - search_x * 2;
                    draw_rounded_rect(buf, stride, w, h, search_x - 1, search_y - 1, search_w + 2, search_h + 2, 26, 0x4438BDF8);
                    draw_rounded_rect(buf, stride, w, h, search_x, search_y, search_w, search_h, 25, 0xEE1E293B);
                    draw_text(buf, stride, w, h, search_x + 18, search_y + 15, "G", 0xFF4285F4, 2);
                    let search_text = if state.drawer_search.is_empty() { "Search apps..." } else { state.drawer_search };
                    let text_col = if state.drawer_search.is_empty() { 0xFF94A3B8 } else { 0xFFFFFFFF };
                    draw_text(buf, stride, w, h, search_x + 48, search_y + 16, search_text, text_col, 2);
                    if !state.drawer_search.is_empty() {
                        draw_text(buf, stride, w, h, search_x + search_w - 32, search_y + 16, "X", 0xFF94A3B8, 2);
                    }

                    // Category Header
                    let header_y = drawer_y_offset + LauncherLayout::DRAWER_HEADER_Y as usize;
                    draw_text_weighted(buf, stride, w, h, 28, header_y, "ALL APPLICATIONS", 0xFF94A3B8, 2, FontWeight::Bold);
                    let mut count_buf = [0u8; 16];
                    let count_str = format_apps_count(&mut count_buf, state.drawer_apps.len());
                    draw_text(buf, stride, w, h, w - 28 - text_width(count_str, 2), header_y, count_str, 0xFF64748B, 2);

                    // Full Apps Grid
                    let drawer_grid_top = drawer_y_offset + LauncherLayout::DRAWER_GRID_TOP as usize;
                    let cols = LauncherLayout::GRID_COLS;
                    let col_width = w / cols;
                    let icon_size = LauncherLayout::ICON_SIZE as usize;
                    let grid_row_h = LauncherLayout::GRID_ROW_H as usize;
                    let max_drawer_rows = LauncherLayout::max_drawer_rows(h as f32);
                    let max_drawer_apps = max_drawer_rows * cols;

                    for (idx, app) in state.drawer_apps.iter().take(max_drawer_apps).enumerate() {
                        let row = idx / cols;
                        let col = idx % cols;
                        let cx = col * col_width + col_width / 2;
                        let cy = drawer_grid_top + row * grid_row_h;

                        let ix = cx.saturating_sub(icon_size / 2);
                        let iy = cy.saturating_sub(icon_size / 2);

                        let is_selected = state.selected_icon_id == Some(app.id);
                        if is_selected {
                            draw_rounded_rect(buf, stride, w, h, ix - 4, iy - 4, icon_size + 8, icon_size + 8, 20, 0xFF38BDF8);
                        }

                        draw_rounded_rect(buf, stride, w, h, ix, iy, icon_size, icon_size, 16, app.color);
                        match app.icon {
                            Some(icon) => {
                                draw_icon_bitmap(buf, stride, w, h, ix, iy, icon_size, icon_size, 16, icon);
                            }
                            None => {
                                draw_text_centered(buf, stride, w, h, cx, cy - 8, app.glyph, 0xFFFFFFFF, 3);
                            }
                        }
                        draw_text_centered_weighted(buf, stride, w, h, cx, cy + 42, app.name, 0xFFE2E8F0, 2, FontWeight::Medium);
                    }

                    // Bottom Navigation Pill in Drawer
                    let nav_y = h - 20;
                    let nav_w = 140;
                    let nav_x = (w - nav_w) / 2;
                    draw_rounded_rect(buf, stride, w, h, nav_x, nav_y, nav_w, 5, 2, 0xFFFFFFFF);
                }
            }
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
            let row1 = if state.keyboard_shift_active {
                ["Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P"]
            } else {
                ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"]
            };
            let r1_key_w = (kb_w - 30) / 10;
            let key_h = 65;
            let r1_y = kb_y + 25;
            for (i, k) in row1.iter().enumerate() {
                let kx = kb_x + 15 + i * r1_key_w;
                draw_rounded_rect(buf, stride, w, h, kx + 2, r1_y, r1_key_w - 4, key_h, 10, 0xFF334155);
                draw_text_centered(buf, stride, w, h, kx + r1_key_w / 2, r1_y + 18, k, 0xFFFFFFFF, 3);
            }

            // Row 2: A S D F G H J K L (9 keys)
            let row2 = if state.keyboard_shift_active {
                ["A", "S", "D", "F", "G", "H", "J", "K", "L"]
            } else {
                ["a", "s", "d", "f", "g", "h", "j", "k", "l"]
            };
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
            let shift_bg = if state.keyboard_shift_active { 0xFF0284C7 } else { 0xFF1E293B };
            draw_rounded_rect(buf, stride, w, h, kb_x + 15, r3_y, special_w - 4, key_h, 10, shift_bg);
            draw_text_centered(buf, stride, w, h, kb_x + 15 + special_w / 2, r3_y + 22, "^", 0xFFFFFFFF, 3);

            let row3 = if state.keyboard_shift_active {
                ["Z", "X", "C", "V", "B", "N", "M"]
            } else {
                ["z", "x", "c", "v", "b", "n", "m"]
            };
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

        // 12. Tactile Touch Ripple Animation (Lawnchair 17 / Material You touch feedback)
        if let Some((rx, ry, radius, alpha)) = state.touch_ripple {
            if alpha > 0.01 && radius > 1.0 {
                let r_int = radius as usize;
                let alpha_u8 = (alpha * 255.0).clamp(0.0, 255.0) as u8;
                draw_glow_circle(buf, stride, w, h, rx as usize, ry as usize, r_int, 0x38, 0xBD, 0xF8, alpha_u8 / 2);
                let ripple_color = ((alpha_u8 as u32) << 24) | 0x00FFFFFF;
                draw_rounded_rect_i32(
                    buf, stride, w, h,
                    (rx - radius) as i32,
                    (ry - radius) as i32,
                    r_int * 2,
                    r_int * 2,
                    r_int,
                    ripple_color,
                );
            }
        }

        // 13. App Launch Expansion Animation (Lawnchair 17 / Pixel Launcher app opening transition)
        if let Some((ox, oy)) = state.app_launch_origin {
            if state.app_launch_progress > 0.001 && state.app_launch_progress < 0.999 {
                let t = state.app_launch_progress.clamp(0.0, 1.0);
                let ease = 1.0 - (1.0 - t).powi(3);
                let cur_w = 64.0 + (w as f32 - 64.0) * ease;
                let cur_h = 64.0 + (h as f32 - 64.0) * ease;
                let cur_x = (ox - cur_w / 2.0).clamp(0.0, (w as f32 - cur_w).max(0.0));
                let cur_y = (oy - cur_h / 2.0).clamp(0.0, (h as f32 - cur_h).max(0.0));
                let radius = 32.0 * (1.0 - ease) + 16.0 * ease;
                let alpha = (ease * 230.0) as u32;
                let app_rgb = state.app_launch_color & 0x00FFFFFF;
                let color = (alpha << 24) | app_rgb;
                draw_rounded_rect_i32(
                    buf, stride, w, h,
                    cur_x as i32,
                    cur_y as i32,
                    cur_w as usize,
                    cur_h as usize,
                    radius as usize,
                    color,
                );
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
    macro_rules! mix {
        ($v:expr) => {{
            h ^= $v as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }};
    }
    for b in state.time_str.bytes() {
        mix!(b);
    }
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
    mix!(state.terminal_running as u8);
    mix!(state.terminal_active_tab as u8);
    mix!(state.terminal_tabs.len() as u8);
    for tab in state.terminal_tabs {
        mix!(tab.id as u8);
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
    mix!(state.home_page as u8);
    mix!(state.total_home_pages as u8);
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
    mix!(state.messages_list.len() as u8);
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
    h
}

/// Zero-allocation, in-place frosted glass blur filter applied to a vertical scanline region.
/// Subsamples and blends 4x4 blocks with a deep translucent acrylic tint (Material 3 Expressive).
pub fn apply_frosted_blur_region(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    y_start: usize,
    y_end: usize,
) {
    let block = 4;
    let y_start = y_start.min(y_end);
    for y in (y_start..y_end).step_by(block) {
        let row_idx = y * stride;
        for x in (0..w).step_by(block) {
            let p = buf[row_idx + x];
            let r = (p >> 16) & 0xFF;
            let g = (p >> 8) & 0xFF;
            let b = p & 0xFF;

            // Blend 40% original, 60% frosted slate dark tint (#0F172A)
            let br = ((r * 2) + (0x0F * 3)) / 5;
            let bg = ((g * 2) + (0x17 * 3)) / 5;
            let bb = ((b * 2) + (0x2A * 3)) / 5;
            let blended = (0xFF << 24) | (br << 16) | (bg << 8) | bb;

            for dy in 0..block {
                if y + dy < y_end {
                    let fill_row = (y + dy) * stride;
                    for dx in 0..block {
                        if x + dx < w {
                            buf[fill_row + x + dx] = blended;
                        }
                    }
                }
            }
        }
    }
}

/// Zero-allocation, in-place frosted glass blur filter for full screen.
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
        for b in app.name.bytes() {
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

    // 16.16 fixed-point source coordinate for a half-pixel aligned destination
    // sample, so an exact 1:1 blit resolves to integer indices with zero weight.
    // The result is clamped to the valid sample range *before* the taps are
    // taken, so an edge tap keeps a zero weight instead of double counting.
    let map = |coord: usize, size: usize, span: usize| -> (i64, u32) {
        let fx = (((2 * coord + 1) as i64 * span as i64 * 65536) / (2 * size as i64)) - 32768;
        let hi = ((span as i64 - 1) << 16).max(0);
        let fx = if fx < 0 {
            0
        } else if fx > hi {
            hi
        } else {
            fx
        };
        (fx >> 16, (fx & 0xFFFF) as u32)
    };
    let clamp = |i: i64, span: usize| -> i64 {
        if i < 0 {
            0
        } else if i >= span as i64 {
            span as i64 - 1
        } else {
            i
        }
    };

    for cy in y_start..y_end {
        let cy_i = cy as i32;
        let dy = if cy_i < y + radius as i32 {
            radius as i32 - (cy_i - y)
        } else if cy_i >= y + th as i32 - radius as i32 {
            cy_i - (y + th as i32 - radius as i32)
        } else {
            0
        };
        if dy * dy > r2 {
            continue;
        }
        let (sy0, wy0) = map((cy_i - y) as usize, th, ih);
        let ya = clamp(sy0, ih);
        let yb = clamp(sy0 + 1, ih);
        let row = cy * stride;

        for cx in x_start..x_end {
            let cx_i = cx as i32;
            let dx = if cx_i < x + radius as i32 {
                radius as i32 - (cx_i - x)
            } else if cx_i >= x + tw as i32 - radius as i32 {
                cx_i - (x + tw as i32 - radius as i32)
            } else {
                0
            };
            if dx * dx + dy * dy > r2 {
                continue;
            }

            let (sx0, wx0) = map((cx_i - x) as usize, tw, iw);
            let xa = clamp(sx0, iw);
            let xb = clamp(sx0 + 1, iw);

            // Bilinear taps in premultiplied space: avoids halos when the icon
            // has transparent edges and lets the composite stay integer-only.
            let mut a_acc = 0u64;
            let mut c_acc = [0u64; 3];
            let y_taps = [(ya, 65536 - wy0 as u64), (yb, wy0 as u64)];
            let x_taps = [(xa, 65536 - wx0 as u64), (xb, wx0 as u64)];
            for (ty, tw_y) in y_taps {
                let base = (ty as usize * iw) * 4;
                for (tx, wx) in x_taps {
                    let o = base + tx as usize * 4;
                    let a = src[o + 3] as u64;
                    let w = tw_y * wx;
                    a_acc += a * w;
                    c_acc[0] += src[o] as u64 * a * w;
                    c_acc[1] += src[o + 1] as u64 * a * w;
                    c_acc[2] += src[o + 2] as u64 * a * w;
                }
            }

            // Taps are weighted in 2-D, so the weights sum to 2^32.
            let alpha = ((a_acc + (1 << 31)) >> 32).min(255) as u32;
            if alpha == 0 {
                continue;
            }
            let dst = buf[row + cx];
            let bg = [
                ((dst >> 16) & 0xFF),
                ((dst >> 8) & 0xFF),
                (dst & 0xFF),
            ];
            let inv = 255 - alpha;
            let mut out = [0u32; 3];
            for ch in 0..3 {
                let pm = (c_acc[ch] / a_acc.max(1)) as u32;
                out[ch] = (pm * alpha + bg[ch] * inv + 127) / 255;
            }
            buf[row + cx] =
                (0xFF << 24) | (out[0] << 16) | (out[1] << 8) | out[2];
        }
    }
}

#[inline]
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
            for cx in x..x_end {
                buf[row + cx] = color;
            }
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
        let cy_i = cy as i32;
        let dy = if cy_i < y + radius as i32 {
            radius as i32 - (cy_i - y)
        } else if cy_i >= y + rh as i32 - radius as i32 {
            cy_i - (y + rh as i32 - radius as i32)
        } else {
            0
        };

        let row = cy * stride;
        for cx in x_start..x_end {
            let cx_i = cx as i32;
            let dx = if cx_i < x + radius as i32 {
                radius as i32 - (cx_i - x)
            } else if cx_i >= x + rw as i32 - radius as i32 {
                cx_i - (x + rw as i32 - radius as i32)
            } else {
                0
            };

            if dx * dx + dy * dy <= r2 {
                if alpha == 255 {
                    buf[row + cx] = color;
                } else {
                    buf[row + cx] = blend_alpha(buf[row + cx], color, alpha as u8);
                }
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



/// Typography Font Weight for Lawnchair 17 / Material Design 3 type scale hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
    Regular,
    Medium,
    Bold,
}

// 8x16 High-Definition Anti-Aliased Bitmap Font with full lowercase descenders (g, j, p, q, y)
static FONT_8X16: [[u8; 16]; 96] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 32 (0x20) ' '
    [0x00, 0x00, 0x18, 0x3c, 0x3c, 0x3c, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00], // 33 (0x21) '!'
    [0x00, 0x66, 0x66, 0x66, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 34 (0x22) '"'
    [0x00, 0x00, 0x00, 0x6c, 0x6c, 0xfe, 0x6c, 0x6c, 0x6c, 0xfe, 0x6c, 0x6c, 0x00, 0x00, 0x00, 0x00], // 35 (0x23) '#'
    [0x18, 0x18, 0x7c, 0xc6, 0xc2, 0xc0, 0x7c, 0x06, 0x06, 0x86, 0xc6, 0x7c, 0x18, 0x18, 0x00, 0x00], // 36 (0x24) '$'
    [0x00, 0x00, 0x00, 0x00, 0xc2, 0xc6, 0x0c, 0x18, 0x30, 0x60, 0xc6, 0x86, 0x00, 0x00, 0x00, 0x00], // 37 (0x25) '%'
    [0x00, 0x00, 0x38, 0x6c, 0x6c, 0x38, 0x76, 0xdc, 0xcc, 0xcc, 0xcc, 0x76, 0x00, 0x00, 0x00, 0x00], // 38 (0x26) '&'
    [0x00, 0x30, 0x30, 0x30, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 39 (0x27) "'"
    [0x00, 0x00, 0x0c, 0x18, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x18, 0x0c, 0x00, 0x00, 0x00, 0x00], // 40 (0x28) '('
    [0x00, 0x00, 0x30, 0x18, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00], // 41 (0x29) ')'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x66, 0x3c, 0xff, 0x3c, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 42 (0x2a) '*'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x7e, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 43 (0x2b) '+'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x18, 0x30, 0x00, 0x00, 0x00], // 44 (0x2c) ','
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 45 (0x2d) '-'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00], // 46 (0x2e) '.'
    [0x00, 0x00, 0x00, 0x00, 0x02, 0x06, 0x0c, 0x18, 0x30, 0x60, 0xc0, 0x80, 0x00, 0x00, 0x00, 0x00], // 47 (0x2f) '/'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xce, 0xde, 0xf6, 0xe6, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 48 (0x30) '0'
    [0x00, 0x00, 0x18, 0x38, 0x78, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x7e, 0x00, 0x00, 0x00, 0x00], // 49 (0x31) '1'
    [0x00, 0x00, 0x7c, 0xc6, 0x06, 0x0c, 0x18, 0x30, 0x60, 0xc0, 0xc6, 0xfe, 0x00, 0x00, 0x00, 0x00], // 50 (0x32) '2'
    [0x00, 0x00, 0x7c, 0xc6, 0x06, 0x06, 0x3c, 0x06, 0x06, 0x06, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 51 (0x33) '3'
    [0x00, 0x00, 0x0c, 0x1c, 0x3c, 0x6c, 0xcc, 0xfe, 0x0c, 0x0c, 0x0c, 0x1e, 0x00, 0x00, 0x00, 0x00], // 52 (0x34) '4'
    [0x00, 0x00, 0xfe, 0xc0, 0xc0, 0xc0, 0xfc, 0x06, 0x06, 0x06, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 53 (0x35) '5'
    [0x00, 0x00, 0x38, 0x60, 0xc0, 0xc0, 0xfc, 0xc6, 0xc6, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 54 (0x36) '6'
    [0x00, 0x00, 0xfe, 0xc6, 0x06, 0x06, 0x0c, 0x18, 0x30, 0x30, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00], // 55 (0x37) '7'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xc6, 0x7c, 0xc6, 0xc6, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 56 (0x38) '8'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xc6, 0x7e, 0x06, 0x06, 0x06, 0x0c, 0x78, 0x00, 0x00, 0x00, 0x00], // 57 (0x39) '9'
    [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // 58 (0x3a) ':'
    [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00], // 59 (0x3b) ';'
    [0x00, 0x00, 0x00, 0x06, 0x0c, 0x18, 0x30, 0x60, 0x30, 0x18, 0x0c, 0x06, 0x00, 0x00, 0x00, 0x00], // 60 (0x3c) '<'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7e, 0x00, 0x00, 0x7e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 61 (0x3d) '='
    [0x00, 0x00, 0x00, 0x60, 0x30, 0x18, 0x0c, 0x06, 0x0c, 0x18, 0x30, 0x60, 0x00, 0x00, 0x00, 0x00], // 62 (0x3e) '>'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0x0c, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00], // 63 (0x3f) '?'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xc6, 0xde, 0xde, 0xde, 0xdc, 0xc0, 0x7c, 0x00, 0x00, 0x00, 0x00], // 64 (0x40) '@'
    [0x00, 0x00, 0x10, 0x38, 0x6c, 0xc6, 0xc6, 0xfe, 0xc6, 0xc6, 0xc6, 0xc6, 0x00, 0x00, 0x00, 0x00], // 65 (0x41) 'A'
    [0x00, 0x00, 0xfc, 0x66, 0x66, 0x66, 0x7c, 0x66, 0x66, 0x66, 0x66, 0xfc, 0x00, 0x00, 0x00, 0x00], // 66 (0x42) 'B'
    [0x00, 0x00, 0x3c, 0x66, 0xc2, 0xc0, 0xc0, 0xc0, 0xc0, 0xc2, 0x66, 0x3c, 0x00, 0x00, 0x00, 0x00], // 67 (0x43) 'C'
    [0x00, 0x00, 0xf8, 0x6c, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x6c, 0xf8, 0x00, 0x00, 0x00, 0x00], // 68 (0x44) 'D'
    [0x00, 0x00, 0xfe, 0x66, 0x62, 0x68, 0x78, 0x68, 0x60, 0x62, 0x66, 0xfe, 0x00, 0x00, 0x00, 0x00], // 69 (0x45) 'E'
    [0x00, 0x00, 0xfe, 0x66, 0x62, 0x68, 0x78, 0x68, 0x60, 0x60, 0x60, 0xf0, 0x00, 0x00, 0x00, 0x00], // 70 (0x46) 'F'
    [0x00, 0x00, 0x3c, 0x66, 0xc2, 0xc0, 0xc0, 0xde, 0xc6, 0xc6, 0x66, 0x3a, 0x00, 0x00, 0x00, 0x00], // 71 (0x47) 'G'
    [0x00, 0x00, 0xc6, 0xc6, 0xc6, 0xc6, 0xfe, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0x00, 0x00, 0x00, 0x00], // 72 (0x48) 'H'
    [0x00, 0x00, 0x3c, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3c, 0x00, 0x00, 0x00, 0x00], // 73 (0x49) 'I'
    [0x00, 0x00, 0x1e, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0xcc, 0xcc, 0xcc, 0x78, 0x00, 0x00, 0x00, 0x00], // 74 (0x4a) 'J'
    [0x00, 0x00, 0xe6, 0x66, 0x66, 0x6c, 0x78, 0x78, 0x6c, 0x66, 0x66, 0xe6, 0x00, 0x00, 0x00, 0x00], // 75 (0x4b) 'K'
    [0x00, 0x00, 0xf0, 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x62, 0x66, 0xfe, 0x00, 0x00, 0x00, 0x00], // 76 (0x4c) 'L'
    [0x00, 0x00, 0xc3, 0xe7, 0xff, 0xff, 0xdb, 0xc3, 0xc3, 0xc3, 0xc3, 0xc3, 0x00, 0x00, 0x00, 0x00], // 77 (0x4d) 'M'
    [0x00, 0x00, 0xc6, 0xe6, 0xf6, 0xfe, 0xde, 0xce, 0xc6, 0xc6, 0xc6, 0xc6, 0x00, 0x00, 0x00, 0x00], // 78 (0x4e) 'N'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 79 (0x4f) 'O'
    [0x00, 0x00, 0xfc, 0x66, 0x66, 0x66, 0x7c, 0x60, 0x60, 0x60, 0x60, 0xf0, 0x00, 0x00, 0x00, 0x00], // 80 (0x50) 'P'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xd6, 0xde, 0x7c, 0x0c, 0x0e, 0x00, 0x00], // 81 (0x51) 'Q'
    [0x00, 0x00, 0xfc, 0x66, 0x66, 0x66, 0x7c, 0x6c, 0x66, 0x66, 0x66, 0xe6, 0x00, 0x00, 0x00, 0x00], // 82 (0x52) 'R'
    [0x00, 0x00, 0x7c, 0xc6, 0xc6, 0x60, 0x38, 0x0c, 0x06, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 83 (0x53) 'S'
    [0x00, 0x00, 0xff, 0xdb, 0x99, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3c, 0x00, 0x00, 0x00, 0x00], // 84 (0x54) 'T'
    [0x00, 0x00, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 85 (0x55) 'U'
    [0x00, 0x00, 0xc3, 0xc3, 0xc3, 0xc3, 0xc3, 0xc3, 0xc3, 0x66, 0x3c, 0x18, 0x00, 0x00, 0x00, 0x00], // 86 (0x56) 'V'
    [0x00, 0x00, 0xc3, 0xc3, 0xc3, 0xc3, 0xc3, 0xdb, 0xdb, 0xff, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00], // 87 (0x57) 'W'
    [0x00, 0x00, 0xc3, 0xc3, 0x66, 0x3c, 0x18, 0x18, 0x3c, 0x66, 0xc3, 0xc3, 0x00, 0x00, 0x00, 0x00], // 88 (0x58) 'X'
    [0x00, 0x00, 0xc3, 0xc3, 0xc3, 0x66, 0x3c, 0x18, 0x18, 0x18, 0x18, 0x3c, 0x00, 0x00, 0x00, 0x00], // 89 (0x59) 'Y'
    [0x00, 0x00, 0xff, 0xc3, 0x86, 0x0c, 0x18, 0x30, 0x60, 0xc1, 0xc3, 0xff, 0x00, 0x00, 0x00, 0x00], // 90 (0x5a) 'Z'
    [0x00, 0x00, 0x3c, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x3c, 0x00, 0x00, 0x00, 0x00], // 91 (0x5b) '['
    [0x00, 0x00, 0x00, 0x80, 0xc0, 0xe0, 0x70, 0x38, 0x1c, 0x0e, 0x06, 0x02, 0x00, 0x00, 0x00, 0x00], // 92 (0x5c) '\\'
    [0x00, 0x00, 0x3c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x3c, 0x00, 0x00, 0x00, 0x00], // 93 (0x5d) ']'
    [0x10, 0x38, 0x6c, 0xc6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 94 (0x5e) '^'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00], // 95 (0x5f) '_'
    [0x30, 0x30, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 96 (0x60) '`'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x0c, 0x7c, 0xcc, 0xcc, 0xcc, 0x76, 0x00, 0x00, 0x00, 0x00], // 97 (0x61) 'a'
    [0x00, 0x00, 0xe0, 0x60, 0x60, 0x78, 0x6c, 0x66, 0x66, 0x66, 0x66, 0x7c, 0x00, 0x00, 0x00, 0x00], // 98 (0x62) 'b'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7c, 0xc6, 0xc0, 0xc0, 0xc0, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 99 (0x63) 'c'
    [0x00, 0x00, 0x1c, 0x0c, 0x0c, 0x3c, 0x6c, 0xcc, 0xcc, 0xcc, 0xcc, 0x76, 0x00, 0x00, 0x00, 0x00], // 100 (0x64) 'd'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7c, 0xc6, 0xfe, 0xc0, 0xc0, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 101 (0x65) 'e'
    [0x00, 0x00, 0x38, 0x6c, 0x64, 0x60, 0xf0, 0x60, 0x60, 0x60, 0x60, 0xf0, 0x00, 0x00, 0x00, 0x00], // 102 (0x66) 'f'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x76, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0x7c, 0x0c, 0xcc, 0x78, 0x00], // 103 (0x67) 'g'
    [0x00, 0x00, 0xe0, 0x60, 0x60, 0x6c, 0x76, 0x66, 0x66, 0x66, 0x66, 0xe6, 0x00, 0x00, 0x00, 0x00], // 104 (0x68) 'h'
    [0x00, 0x00, 0x18, 0x18, 0x00, 0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3c, 0x00, 0x00, 0x00, 0x00], // 105 (0x69) 'i'
    [0x00, 0x00, 0x06, 0x06, 0x00, 0x0e, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x66, 0x66, 0x3c, 0x00], // 106 (0x6a) 'j'
    [0x00, 0x00, 0xe0, 0x60, 0x60, 0x66, 0x6c, 0x78, 0x78, 0x6c, 0x66, 0xe6, 0x00, 0x00, 0x00, 0x00], // 107 (0x6b) 'k'
    [0x00, 0x00, 0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3c, 0x00, 0x00, 0x00, 0x00], // 108 (0x6c) 'l'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xe6, 0xff, 0xdb, 0xdb, 0xdb, 0xdb, 0xdb, 0x00, 0x00, 0x00, 0x00], // 109 (0x6d) 'm'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xdc, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00], // 110 (0x6e) 'n'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7c, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 111 (0x6f) 'o'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xdc, 0x66, 0x66, 0x66, 0x66, 0x66, 0x7c, 0x60, 0x60, 0xf0, 0x00], // 112 (0x70) 'p'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x76, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0x7c, 0x0c, 0x0c, 0x1e, 0x00], // 113 (0x71) 'q'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xdc, 0x76, 0x66, 0x60, 0x60, 0x60, 0xf0, 0x00, 0x00, 0x00, 0x00], // 114 (0x72) 'r'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x7c, 0xc6, 0x60, 0x38, 0x0c, 0xc6, 0x7c, 0x00, 0x00, 0x00, 0x00], // 115 (0x73) 's'
    [0x00, 0x00, 0x10, 0x30, 0x30, 0xfc, 0x30, 0x30, 0x30, 0x30, 0x36, 0x1c, 0x00, 0x00, 0x00, 0x00], // 116 (0x74) 't'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0xcc, 0x76, 0x00, 0x00, 0x00, 0x00], // 117 (0x75) 'u'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xc3, 0xc3, 0xc3, 0xc3, 0x66, 0x3c, 0x18, 0x00, 0x00, 0x00, 0x00], // 118 (0x76) 'v'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xc3, 0xc3, 0xc3, 0xdb, 0xdb, 0xff, 0x66, 0x00, 0x00, 0x00, 0x00], // 119 (0x77) 'w'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xc3, 0x66, 0x3c, 0x18, 0x3c, 0x66, 0xc3, 0x00, 0x00, 0x00, 0x00], // 120 (0x78) 'x'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0xc6, 0x7e, 0x06, 0x0c, 0xf8, 0x00], // 121 (0x79) 'y'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0xfe, 0xcc, 0x18, 0x30, 0x60, 0xc6, 0xfe, 0x00, 0x00, 0x00, 0x00], // 122 (0x7a) 'z'
    [0x00, 0x00, 0x0e, 0x18, 0x18, 0x18, 0x70, 0x18, 0x18, 0x18, 0x18, 0x0e, 0x00, 0x00, 0x00, 0x00], // 123 (0x7b) '{'
    [0x00, 0x00, 0x18, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00], // 124 (0x7c) '|'
    [0x00, 0x00, 0x70, 0x18, 0x18, 0x18, 0x0e, 0x18, 0x18, 0x18, 0x18, 0x70, 0x00, 0x00, 0x00, 0x00], // 125 (0x7d) '}'
    [0x00, 0x00, 0x76, 0xdc, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 126 (0x7e) '~'
    [0x00, 0x00, 0x00, 0x00, 0x10, 0x38, 0x6c, 0xc6, 0xc6, 0xc6, 0xfe, 0x00, 0x00, 0x00, 0x00, 0x00], // 127 (0x7f) '\x7f'
];

const GLYPH_SPECS: [(u8, u8, u8); 96] = [
    (0, 0, 4), // 32 ' '
    (2, 4, 5), // 33 '!'
    (1, 6, 7), // 34 '"'
    (0, 7, 9), // 35 '#'
    (0, 7, 8), // 36 '$'
    (0, 7, 9), // 37 '%'
    (0, 7, 8), // 38 '&'
    (1, 3, 4), // 39 "'"
    (2, 4, 5), // 40 '('
    (2, 4, 5), // 41 ')'
    (0, 8, 9), // 42 '*'
    (1, 6, 7), // 43 '+'
    (2, 3, 4), // 44 ','
    (1, 6, 7), // 45 '-'
    (3, 2, 4), // 46 '.'
    (0, 7, 8), // 47 '/'
    (0, 7, 8), // 48 '0'
    (1, 6, 7), // 49 '1'
    (0, 7, 8), // 50 '2'
    (0, 7, 8), // 51 '3'
    (0, 7, 8), // 52 '4'
    (0, 7, 8), // 53 '5'
    (0, 7, 8), // 54 '6'
    (0, 7, 8), // 55 '7'
    (0, 7, 8), // 56 '8'
    (0, 7, 8), // 57 '9'
    (3, 2, 4), // 58 ':'
    (2, 3, 4), // 59 ';'
    (1, 6, 7), // 60 '<'
    (1, 6, 7), // 61 '='
    (1, 6, 7), // 62 '>'
    (0, 7, 8), // 63 '?'
    (0, 7, 9), // 64 '@'
    (0, 7, 8), // 65 'A'
    (0, 7, 8), // 66 'B'
    (0, 7, 8), // 67 'C'
    (0, 7, 8), // 68 'D'
    (0, 7, 8), // 69 'E'
    (0, 7, 8), // 70 'F'
    (0, 7, 8), // 71 'G'
    (0, 7, 8), // 72 'H'
    (2, 4, 5), // 73 'I'
    (0, 7, 8), // 74 'J'
    (0, 7, 8), // 75 'K'
    (0, 7, 8), // 76 'L'
    (0, 8, 9), // 77 'M'
    (0, 7, 8), // 78 'N'
    (0, 7, 8), // 79 'O'
    (0, 7, 8), // 80 'P'
    (0, 7, 8), // 81 'Q'
    (0, 7, 8), // 82 'R'
    (0, 7, 8), // 83 'S'
    (0, 8, 9), // 84 'T'
    (0, 7, 8), // 85 'U'
    (0, 8, 9), // 86 'V'
    (0, 8, 9), // 87 'W'
    (0, 8, 9), // 88 'X'
    (0, 8, 9), // 89 'Y'
    (0, 8, 9), // 90 'Z'
    (2, 4, 5), // 91 '['
    (0, 7, 8), // 92 '\\'
    (2, 4, 5), // 93 ']'
    (0, 7, 8), // 94 '^'
    (0, 8, 9), // 95 '_'
    (2, 3, 4), // 96 '`'
    (0, 7, 8), // 97 'a'
    (0, 7, 8), // 98 'b'
    (0, 7, 8), // 99 'c'
    (0, 7, 8), // 100 'd'
    (0, 7, 8), // 101 'e'
    (0, 6, 7), // 102 'f'
    (0, 7, 8), // 103 'g'
    (0, 7, 8), // 104 'h'
    (2, 4, 5), // 105 'i'
    (1, 6, 7), // 106 'j'
    (0, 7, 8), // 107 'k'
    (2, 4, 5), // 108 'l'
    (0, 8, 9), // 109 'm'
    (0, 7, 8), // 110 'n'
    (0, 7, 8), // 111 'o'
    (0, 7, 8), // 112 'p'
    (0, 7, 8), // 113 'q'
    (0, 7, 8), // 114 'r'
    (0, 7, 8), // 115 's'
    (0, 7, 8), // 116 't'
    (0, 7, 8), // 117 'u'
    (0, 8, 9), // 118 'v'
    (0, 8, 9), // 119 'w'
    (0, 8, 9), // 120 'x'
    (0, 7, 8), // 121 'y'
    (0, 7, 8), // 122 'z'
    (1, 6, 7), // 123 '{'
    (3, 2, 4), // 124 '|'
    (1, 6, 7), // 125 '}'
    (0, 7, 8), // 126 '~'
    (0, 7, 8), // 127 '\x7f'
];

/// Calculate exact pixel rendered width for a text string under a given scale with proportional character metrics.
pub fn text_width(text: &str, scale: usize) -> usize {
    let scale = scale.max(1);
    let mut total = 0;
    for b in text.bytes() {
        if (32..128).contains(&b) {
            let (_, _, adv) = GLYPH_SPECS[(b - 32) as usize];
            total += adv as usize * scale;
        } else {
            total += 8 * scale;
        }
    }
    total
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
    let suffix = b" APPS";
    for &b in suffix {
        buf[out_len] = b;
        out_len += 1;
    }
    std::str::from_utf8(&buf[..out_len]).unwrap_or("0 APPS")
}

#[allow(clippy::too_many_arguments)]
pub fn draw_text_weighted_i32(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    mut x: i32,
    y: i32,
    text: &str,
    color: u32,
    scale: usize,
    weight: FontWeight,
) {
    let scale = scale.max(1);
    for b in text.bytes() {
        if (32..128).contains(&b) {
            let idx = (b - 32) as usize;
            let glyph = &FONT_8X16[idx];
            let (off, gw, adv) = GLYPH_SPECS[idx];
            let off = off as usize;
            let gw = gw as usize;

            if x + (adv as i32 * scale as i32) > 0 && x < w as i32 {
                let extra_w = match weight {
                    FontWeight::Regular => 0,
                    FontWeight::Medium | FontWeight::Bold => 1,
                };
                for row in 0..16 {
                    let raw_row = glyph[row];
                    let row_bits = match weight {
                        FontWeight::Regular => raw_row,
                        FontWeight::Medium | FontWeight::Bold => raw_row | (raw_row >> 1),
                    };

                    if row_bits == 0 {
                        continue;
                    }

                    for col in 0..(gw + extra_w) {
                        let col_idx = off + col;
                        if col_idx < 8 {
                            let bit_mask = 0x80 >> col_idx;
                            if (row_bits & bit_mask) != 0 {
                                let px = x + (col * scale) as i32;
                                let py = y + (row * scale) as i32;

                                if scale == 1 {
                                    if px >= 0 && (px as usize) < w && py >= 0 && (py as usize) < h {
                                        let bg = buf[(py as usize) * stride + (px as usize)];
                                        buf[(py as usize) * stride + (px as usize)] = blend_alpha(bg, color, 255);
                                    }
                                } else {
                                    // Scale >= 2: Anti-aliased corner-smoothed subpixel block for high-definition typography
                                    let has_up = row > 0 && ((glyph[row - 1] & bit_mask) != 0);
                                    let has_down = row + 1 < 16 && ((glyph[row + 1] & bit_mask) != 0);
                                    let has_left = col_idx > 0 && ((row_bits & (0x80 >> (col_idx - 1))) != 0);
                                    let has_right = col_idx + 1 < 8 && ((row_bits & (0x80 >> (col_idx + 1))) != 0);

                                    for sy in 0..scale {
                                        let cur_y = py + sy as i32;
                                        if cur_y < 0 || (cur_y as usize) >= h { continue; }
                                        let row_idx = (cur_y as usize) * stride;

                                        let is_top_edge = sy == 0;
                                        let is_bot_edge = sy == scale - 1;

                                        for sx in 0..scale {
                                            let cur_x = px + sx as i32;
                                            if cur_x < 0 || (cur_x as usize) >= w { continue; }
                                            let cur_x_u = cur_x as usize;

                                            let is_left_edge = sx == 0;
                                            let is_right_edge = sx == scale - 1;

                                            let is_corner = (is_top_edge && is_left_edge && !has_up && !has_left)
                                                || (is_top_edge && is_right_edge && !has_up && !has_right)
                                                || (is_bot_edge && is_left_edge && !has_down && !has_left)
                                                || (is_bot_edge && is_right_edge && !has_down && !has_right);

                                            if is_corner {
                                                let bg = buf[row_idx + cur_x_u];
                                                buf[row_idx + cur_x_u] = blend_alpha(bg, color, 140);
                                            } else {
                                                buf[row_idx + cur_x_u] = color;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            x += adv as i32 * scale as i32;
        } else {
            x += 8 * scale as i32;
        }
    }
}

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
    let total_w = text_width(text, scale) as i32;
    let x = center_x - total_w / 2;
    draw_text_weighted_i32(buf, stride, w, h, x, y, text, color, scale, weight);
}

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
    draw_text_weighted_i32(buf, stride, w, h, x as i32, y as i32, text, color, scale, FontWeight::Regular);
}

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
    pub fn drawer() -> Self {
        Self {
            stiffness: 300.0,
            damping_ratio: 0.85,
            value_threshold: 0.001,
        }
    }

    /// Home screen page swipe & boundary overscroll spring configuration.
    pub fn page_swipe() -> Self {
        Self {
            stiffness: 280.0,
            damping_ratio: 0.75, // Authentic Lawnchair 17 subtle overshoot rebound
            value_threshold: 0.5,
        }
    }

    /// Touch press bounce compression & release spring configuration.
    pub fn icon_bounce() -> Self {
        Self {
            stiffness: 450.0,
            damping_ratio: Self::DAMPING_RATIO_MEDIUM_BOUNCY, // Bouncy tactile rebound (0.92x -> 1.013x -> 1.0x)
            value_threshold: 0.002,
        }
    }

    /// App launch expansion animation spring configuration.
    pub fn app_launch() -> Self {
        Self {
            stiffness: 320.0,
            damping_ratio: 0.90, // Smooth expansion from icon to fullscreen
            value_threshold: 0.001,
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

        let delta_t = dt as f64;
        let displacement = (self.value - self.target) as f64;
        let velocity = self.velocity as f64;
        let stiffness = self.config.stiffness as f64;
        let damping_ratio = self.config.damping_ratio as f64;
        let natural_freq = stiffness.sqrt();

        let (new_disp, new_vel) = if damping_ratio > 1.0 {
            // Overdamped
            let gamma_plus = -damping_ratio * natural_freq + natural_freq * (damping_ratio * damping_ratio - 1.0).sqrt();
            let gamma_minus = -damping_ratio * natural_freq - natural_freq * (damping_ratio * damping_ratio - 1.0).sqrt();
            let coeff_b = (gamma_minus * displacement - velocity) / (gamma_minus - gamma_plus);
            let coeff_a = displacement - coeff_b;
            let d = coeff_a * (gamma_minus * delta_t).exp() + coeff_b * (gamma_plus * delta_t).exp();
            let v = coeff_a * gamma_minus * (gamma_minus * delta_t).exp() + coeff_b * gamma_plus * (gamma_plus * delta_t).exp();
            (d, v)
        } else if (damping_ratio - 1.0).abs() < 1e-4 {
            // Critically damped
            let coeff_a = displacement;
            let coeff_b = velocity + natural_freq * displacement;
            let decay = (-natural_freq * delta_t).exp();
            let d = (coeff_a + coeff_b * delta_t) * decay;
            let v = (coeff_b - natural_freq * (coeff_a + coeff_b * delta_t)) * decay;
            (d, v)
        } else {
            // Underdamped (Lawnchair 17 standard bouncy spring)
            let damped_freq = natural_freq * (1.0 - damping_ratio * damping_ratio).sqrt();
            let cos_coeff = displacement;
            let sin_coeff = (1.0 / damped_freq) * (damping_ratio * natural_freq * displacement + velocity);
            let decay = (-damping_ratio * natural_freq * delta_t).exp();
            let cos_val = (damped_freq * delta_t).cos();
            let sin_val = (damped_freq * delta_t).sin();
            let d = decay * (cos_coeff * cos_val + sin_coeff * sin_val);
            let v = d * (-natural_freq * damping_ratio)
                + decay * (-damped_freq * cos_coeff * sin_val + damped_freq * sin_coeff * cos_val);
            (d, v)
        };

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
}

/// Authentic Android/Lawnchair 17 boundary overscroll resistance curve.
/// Dampens finger drag when pulling past the first or last home screen page.
pub fn apply_overscroll_resistance(drag_delta: f32, screen_width: f32) -> f32 {
    let max_overscroll = screen_width * 0.35;
    let sign = if drag_delta >= 0.0 { 1.0 } else { -1.0 };
    let abs_delta = drag_delta.abs();
    sign * (max_overscroll * (abs_delta / (abs_delta + max_overscroll * 1.5)))
}

/// Authentic Lawnchair 17 / Material You (Monet) Dynamic Tonal Palette.
/// Derived with zero dynamic heap allocations on the hot render path.
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

/// Lawnchair 17 / Pixel Launcher Material You Curved Digital Clock Renderer.
/// Renders smooth, modern rounded pill digits with zero blocky pixelation.
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
    let digit_w = (digit_h * 54) / 100;
    let stroke = ((digit_h * 15) / 100).max(4);
    let spacing = (digit_h * 12) / 100;
    let colon_w = (digit_h * 18) / 100;

    let mut total_w = 0;
    for ch in time_str.chars() {
        if ch == ':' {
            total_w += colon_w + spacing;
        } else if ch.is_ascii_digit() {
            total_w += digit_w + spacing;
        } else {
            total_w += digit_w / 2 + spacing;
        }
    }
    if total_w > 0 {
        total_w -= spacing;
    }

    let mut cur_x = center_x.saturating_sub(total_w / 2);
    let top_y = center_y.saturating_sub(digit_h / 2);

    for ch in time_str.chars() {
        match ch {
            ':' => {
                let dot_r = stroke / 2;
                let dot_x = cur_x + colon_w / 2;
                let dot_y1 = top_y + digit_h / 3;
                let dot_y2 = top_y + (digit_h * 2) / 3;
                draw_rounded_rect(buf, stride, w, h, dot_x.saturating_sub(dot_r), dot_y1.saturating_sub(dot_r), stroke, stroke, dot_r, color);
                draw_rounded_rect(buf, stride, w, h, dot_x.saturating_sub(dot_r), dot_y2.saturating_sub(dot_r), stroke, stroke, dot_r, color);
                cur_x += colon_w + spacing;
            }
            '0'..='9' => {
                let d = ch as u8 - b'0';
                draw_clock_digit(buf, stride, w, h, cur_x, top_y, digit_w, digit_h, stroke, color, d);
                cur_x += digit_w + spacing;
            }
            _ => {
                cur_x += digit_w / 2 + spacing;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_clock_digit(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    dw: usize,
    dh: usize,
    st: usize,
    color: u32,
    digit: u8,
) {
    let r = st / 2;
    let mid_y = y + dh / 2 - st / 2;
    let bot_y = y + dh - st;
    let right_x = x + dw - st;

    let h_top = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, x, y, dw, st, r, color);
    let h_mid = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, x, mid_y, dw, st, r, color);
    let h_bot = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, x, bot_y, dw, st, r, color);

    let v_left_full = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, x, y, st, dh, r, color);
    let v_right_full = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, right_x, y, st, dh, r, color);
    let v_left_top = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, x, y, st, dh / 2 + r, r, color);
    let v_left_bot = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, x, mid_y, st, dh / 2 + r, r, color);
    let v_right_top = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, right_x, y, st, dh / 2 + r, r, color);
    let v_right_bot = |buf: &mut [u32]| draw_rounded_rect(buf, stride, w, h, right_x, mid_y, st, dh / 2 + r, r, color);

    match digit {
        0 => {
            v_left_full(buf);
            v_right_full(buf);
            h_top(buf);
            h_bot(buf);
        }
        1 => {
            v_right_full(buf);
            draw_rounded_rect(buf, stride, w, h, x + dw / 3, y, dw * 2 / 3, st, r, color);
        }
        2 => {
            h_top(buf);
            v_right_top(buf);
            h_mid(buf);
            v_left_bot(buf);
            h_bot(buf);
        }
        3 => {
            h_top(buf);
            h_mid(buf);
            h_bot(buf);
            v_right_full(buf);
        }
        4 => {
            v_left_top(buf);
            h_mid(buf);
            v_right_full(buf);
        }
        5 => {
            h_top(buf);
            v_left_top(buf);
            h_mid(buf);
            v_right_bot(buf);
            h_bot(buf);
        }
        6 => {
            v_left_full(buf);
            h_top(buf);
            h_mid(buf);
            v_right_bot(buf);
            h_bot(buf);
        }
        7 => {
            h_top(buf);
            v_right_full(buf);
        }
        8 => {
            v_left_full(buf);
            v_right_full(buf);
            h_top(buf);
            h_mid(buf);
            h_bot(buf);
        }
        9 => {
            v_left_top(buf);
            h_top(buf);
            h_mid(buf);
            v_right_full(buf);
            h_bot(buf);
        }
        _ => {}
    }
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

    #[test]
    fn test_font_8x16_descenders_and_ascenders() {
        // 'g' (103 - 32 = 71) must have active pixels below row 12 (rows 13..15)
        let glyph_g = &FONT_8X16[(b'g' - 32) as usize];
        let has_descender_g = glyph_g[13] != 0 || glyph_g[14] != 0 || glyph_g[15] != 0;
        assert!(has_descender_g, "'g' must descend below baseline");

        let glyph_p = &FONT_8X16[(b'p' - 32) as usize];
        let has_descender_p = glyph_p[13] != 0 || glyph_p[14] != 0 || glyph_p[15] != 0;
        assert!(has_descender_p, "'p' must descend below baseline");

        let glyph_y = &FONT_8X16[(b'y' - 32) as usize];
        let has_descender_y = glyph_y[13] != 0 || glyph_y[14] != 0 || glyph_y[15] != 0;
        assert!(has_descender_y, "'y' must descend below baseline");

        let glyph_j = &FONT_8X16[(b'j' - 32) as usize];
        let has_descender_j = glyph_j[13] != 0 || glyph_j[14] != 0 || glyph_j[15] != 0;
        assert!(has_descender_j, "'j' must descend below baseline");

        // Ascender check: 'd' and 'h' must have active pixels in rows 2..4
        let glyph_d = &FONT_8X16[(b'd' - 32) as usize];
        let has_ascender_d = glyph_d[2] != 0 || glyph_d[3] != 0;
        assert!(has_ascender_d, "'d' must ascend above x-height");
    }

    #[test]
    fn test_font_weight_drawing() {
        let mut buf_reg = vec![0xFF000000u32; 32 * 32];
        let mut buf_bold = vec![0xFF000000u32; 32 * 32];
        draw_text_weighted(&mut buf_reg, 32, 32, 32, 0, 0, "A", 0xFFFFFFFF, 1, FontWeight::Regular);
        draw_text_weighted(&mut buf_bold, 32, 32, 32, 0, 0, "A", 0xFFFFFFFF, 1, FontWeight::Bold);
        let count_reg = buf_reg.iter().filter(|&&p| p == 0xFFFFFFFF).count();
        let count_bold = buf_bold.iter().filter(|&&p| p == 0xFFFFFFFF).count();
        assert!(count_bold > count_reg, "Bold font must have more active pixels than regular font (bold: {}, reg: {})", count_bold, count_reg);
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

    #[test]
    fn test_overscroll_resistance_curve() {
        let screen_w = 1080.0;
        let r1 = apply_overscroll_resistance(50.0, screen_w);
        let r2 = apply_overscroll_resistance(200.0, screen_w);
        let r3 = apply_overscroll_resistance(1000.0, screen_w);
        assert!(r1 > 0.0 && r1 < 50.0, "Resistance must damp movement (delta: 50 -> resisted: {})", r1);
        assert!(r2 > r1, "Higher drag delta produces higher resisted offset");
        assert!(r3 < screen_w * 0.35, "Resistance must asymptote below max overscroll bound");

        // Negative delta preserves direction
        let r_neg = apply_overscroll_resistance(-100.0, screen_w);
        assert!(r_neg < 0.0);
        assert_eq!(r_neg.abs(), apply_overscroll_resistance(100.0, screen_w));
    }
}

