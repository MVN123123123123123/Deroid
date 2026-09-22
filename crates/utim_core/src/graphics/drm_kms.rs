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
    pad: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
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
            let mut m = DrmModeModeInfo::default();
            m.clock = 74250;
            m.hdisplay = 1080;
            m.hsync_start = 1120;
            m.hsync_end = 1140;
            m.htotal = 1200;
            m.vdisplay = 2400;
            m.vsync_start = 2410;
            m.vsync_end = 2420;
            m.vtotal = 2450;
            m.vrefresh = 60;
            m.mode_type = DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED;
            let name_bytes = b"1080x2400\0";
            m.name[..name_bytes.len()].copy_from_slice(name_bytes);
            m
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
            return Err(io::Error::last_os_error());
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
            return Err(io::Error::last_os_error());
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

        let _ = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut crtc) };

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

    /// Render the entire Universal Treble Mobile Launcher and SystemUI shell
    pub fn render_mobile_ui(&mut self, time_str: &str, is_locked: bool) {
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
        draw_text(buf, stride, w, h, 24, 14, time_str, 0xFFFFFFFF, 2);

        // Status Icons (Right side of status bar)
        let icon_right = w - 24;
        draw_battery(buf, stride, w, h, icon_right - 40, 14, 98);
        draw_wifi(buf, stride, w, h, icon_right - 80, 14);
        draw_text(buf, stride, w, h, icon_right - 125, 14, "5G", 0xFFFFFFFF, 2);

        if is_locked {
            // Lock Screen UI
            let center_x = w / 2;
            let center_y = h / 3;
            draw_text_centered(buf, stride, w, h, center_x, center_y, time_str, 0xFFFFFFFF, 6);
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
                "Swipe up to unlock",
                0xFF8899A6,
                2,
            );
            draw_padlock(buf, stride, w, h, center_x, center_y - 80);
            return;
        }

        // 3. Digital Clock & Date Widget on Home Screen
        let widget_y = 120;
        draw_text_centered(buf, stride, w, h, w / 2, widget_y, time_str, 0xFFFFFFFF, 5);
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

        // 4. Google / Treble Search Pill Widget
        let search_y = widget_y + 115;
        let search_w = w - 64;
        let search_x = 32;
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            search_x,
            search_y,
            search_w,
            56,
            28,
            0x2A3345,
        );
        draw_text(buf, stride, w, h, search_x + 20, search_y + 16, "G", 0x4285F4, 3);
        draw_text(
            buf,
            stride,
            w,
            h,
            search_x + 55,
            search_y + 18,
            "Search apps, web...",
            0x8A99AD,
            2,
        );
        draw_text(buf, stride, w, h, search_x + search_w - 36, search_y + 16, "*", 0xEA4335, 3);

        // 5. App Grid Icons (4 columns x 3 rows)
        let grid_top = search_y + 90;
        let cols = 4;
        let col_width = w / cols;
        let icon_size = 64;

        let apps = [
            ("Phone", 0x10B981, "P"),
            ("Messages", 0x3B82F6, "M"),
            ("Browser", 0x06B6D4, "B"),
            ("Camera", 0xF43F5E, "C"),
            ("Gallery", 0x8B5CF6, "G"),
            ("Settings", 0x64748B, "S"),
            ("Files", 0xF59E0B, "F"),
            ("Music", 0xD946EF, "M"),
            ("Terminal", 0x1E293B, ">"),
            ("Treble OS", 0x6366F1, "U"),
            ("Contacts", 0x14B8A6, "C"),
            ("Clock", 0xEF4444, "T"),
        ];

        for (idx, (name, color, glyph)) in apps.iter().enumerate() {
            let row = idx / cols;
            let col = idx % cols;
            let cx = col * col_width + col_width / 2;
            let cy = grid_top + row * 115;

            let ix = cx.saturating_sub(icon_size / 2);
            let iy = cy.saturating_sub(icon_size / 2);
            draw_rounded_rect(buf, stride, w, h, ix, iy, icon_size, icon_size, 16, *color);
            draw_text_centered(buf, stride, w, h, cx, cy - 10, glyph, 0xFFFFFFFF, 3);
            draw_text_centered(buf, stride, w, h, cx, cy + 42, name, 0xFFE2E8F0, 1);
        }

        // 6. Persistent Hotseat Dock at Bottom
        let dock_h = 100;
        let dock_y = h - dock_h - 40;
        let dock_w = w - 40;
        let dock_x = 20;

        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            dock_x,
            dock_y,
            dock_w,
            dock_h,
            32,
            0x182236,
        );

        let dock_apps = [
            ("Phone", 0x10B981, "P"),
            ("Messages", 0x3B82F6, "M"),
            ("Apps", 0x475569, ":"),
            ("Browser", 0x06B6D4, "B"),
            ("Camera", 0xF43F5E, "C"),
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

        // 7. Gesture Navigation Bar (Pill at bottom)
        let nav_y = h - 20;
        let nav_w = 140;
        let nav_x = (w - nav_w) / 2;
        draw_rounded_rect(buf, stride, w, h, nav_x, nav_y, nav_w, 5, 2, 0xFFFFFFFF);
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
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, self.fb_id);
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

#[inline]
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
    table[0] = [0x00, 0x00, 0x00, 0x00, 0x00];
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
    table[12] = [0x00, 0x50, 0x30, 0x00, 0x00]; // ,
    table[14] = [0x00, 0x60, 0x60, 0x00, 0x00]; // .
    table[13] = [0x08, 0x08, 0x08, 0x08, 0x08]; // -
    table[15] = [0x20, 0x10, 0x08, 0x04, 0x02]; // /
    table[10] = [0x14, 0x08, 0x3e, 0x08, 0x14]; // *
    table[5] = [0x23, 0x13, 0x08, 0x64, 0x62];  // %
    table[30] = [0x00, 0x41, 0x22, 0x14, 0x08]; // >
    table[63] = [0x00, 0x41, 0x00, 0x41, 0x00]; // |
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
    table
};

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
        if b >= 32 && b < 128 {
            let glyph = &FONT_5X7[(b - 32) as usize];
            for col in 0..5 {
                let col_bits = glyph[col];
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
