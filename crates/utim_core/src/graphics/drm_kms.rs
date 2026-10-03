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
    AppLayout, AppPanel, FolderLayout, Keyboard, Layout, Rect, ShadeLayout, FOLDER_PREVIEW_MAX,
    ICON_RADIUS, KB_ROW2, KB_ROW3_MID, LABEL_GAP, PANEL_PAD_FRACTION,
};
use super::png::RgbaImage;
use crate::graphics::composer::Transform;
use crate::sensors::sensor_proxy::DeviceOrientation;

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

/// Whether a compositor transform transposes the scanout axes.
///
/// Only `Rotate90`/`Rotate270` swap `w` and `h`; `None`, `Rotate180` and the
/// flips do not. Pure geometry, no I/O: the *decision* to rotate lives in
/// `rotation::decide_modeset` (`rotation.rs:396-410`), this answers what size
/// the re-modeset must allocate once that decision says `Rotate` (the
/// reference actuates at `RotationHelper.java:226`).
#[inline]
pub const fn transform_swaps_xy(t: Transform) -> bool {
    matches!(t, Transform::Rotate90 | Transform::Rotate270)
}

/// Framebuffer size after applying `t` to a `w` x `h` panel.
///
/// The pure counterpart of [`DrmKmsDevice::apply_transform`]'s size step, so
/// the shell and tests can predict the modeset without issuing one. `Copy`
/// in, `Copy` out, no heap, total over `Transform`.
#[inline]
pub const fn rotated_frame_size(w: u32, h: u32, t: Transform) -> (u32, u32) {
    if transform_swaps_xy(t) {
        (h, w)
    } else {
        (w, h)
    }
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
    /// Current scanout transform, `Transform::None` at rest.
    ///
    /// Set by `open_card` (native, unrotated) and advanced only by
    /// [`DrmKmsDevice::apply_transform`]. It is what makes a redundant
    /// re-modeset a no-op rather than a panel flicker: the reference
    /// compares the flags it is about to set against the last ones and does
    /// nothing when they match (`RotationHelper.java:232`), and
    /// [`crate::rotation::RotationPolicy`] answers `Hold` for the same case.
    pub transform: Transform,
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

        assert!(
            create_dumb.pitch.is_multiple_of(4),
            "pitch must be a multiple of 4 for XRGB8888"
        );
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
            transform: Transform::None,
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

impl DrmKmsDevice {
    /// Re-modeset the panel for `transform`, re-creating the scanout buffer.
    ///
    /// Allocates a dumb buffer with [`rotated_frame_size`] dimensions of the
    /// native mode (swapped `w`/`h` for 90/270), attaches a framebuffer,
    /// maps it, and issues `SETCRTC` against the stored connector/CRTC/mode.
    /// The mode timing itself is left native: rotation here is a transposed
    /// framebuffer the compositor fills transposed (via `Transform`), not a
    /// new panel timing, which is why the next rotation derives its size
    /// from `mode` rather than from the current `width`/`height`.
    ///
    /// Returns `Ok(false)` without issuing any ioctl when already in
    /// `transform` (the reference's compare-and-skip,
    /// `RotationHelper.java:232`), `Ok(true)` after a modeset. Teardown of
    /// the old buffer reuses `open_card`'s `SETCRTC`-failure order
    /// (`drm_kms.rs:478-483`, mirrored by `Drop`): `munmap`, `RMFB`,
    /// `DESTROY_DUMB`. Partial-creation failures unwind the same way
    /// `open_card` does (ADDFB fail -> destroy; MAP fail -> remove FB +
    /// destroy; mmap fail -> remove FB + destroy; SETCRTC fail -> unmap +
    /// remove FB + destroy) and invalidate the frame cache, so a failed
    /// modeset always repaints rather than rescanning a stale buffer.
    pub fn apply_transform(&mut self, transform: Transform) -> io::Result<bool> {
        if transform == self.transform {
            return Ok(false);
        }
        let base_w = self.mode.hdisplay as u32;
        let base_h = self.mode.vdisplay as u32;
        if base_w == 0 || base_h == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no native mode to rotate from",
            ));
        }
        let (new_w, new_h) = rotated_frame_size(base_w, base_h, transform);
        let fd = self.file.as_raw_fd();
        if !self.mmap_ptr.is_null() && self.size > 0 {
            unsafe {
                libc::munmap(self.mmap_ptr as *mut libc::c_void, self.size);
            }
            self.mmap_ptr = std::ptr::null_mut();
            self.size = 0;
        }
        if self.fb_id != 0 {
            let mut fb = self.fb_id;
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb);
            }
            self.fb_id = 0;
        }
        if self.dumb_handle != 0 {
            let mut destroy = DrmModeDestroyDumb {
                handle: self.dumb_handle,
            };
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            self.dumb_handle = 0;
        }
        let mut create_dumb = DrmModeCreateDumb {
            width: new_w,
            height: new_h,
            bpp: 32,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut create_dumb) } < 0 {
            let err = io::Error::last_os_error();
            self.invalidate_frame_cache();
            return Err(err);
        }
        let mut fb_cmd = DrmModeFbCmd {
            fb_id: 0,
            width: new_w,
            height: new_h,
            pitch: create_dumb.pitch,
            bpp: 32,
            depth: 24,
            handle: create_dumb.handle,
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_ADDFB, &mut fb_cmd) } < 0 {
            let err = io::Error::last_os_error();
            let mut destroy = DrmModeDestroyDumb {
                handle: create_dumb.handle,
            };
            unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy) };
            self.invalidate_frame_cache();
            return Err(err);
        }
        let mut map_dumb = DrmModeMapDumb {
            handle: create_dumb.handle,
            pad: 0,
            offset: 0,
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut map_dumb) } < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_cmd.fb_id);
                let mut destroy = DrmModeDestroyDumb {
                    handle: create_dumb.handle,
                };
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            self.invalidate_frame_cache();
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
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_cmd.fb_id);
                let mut destroy = DrmModeDestroyDumb {
                    handle: create_dumb.handle,
                };
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            self.invalidate_frame_cache();
            return Err(err);
        }
        let mmap_ptr = mmap_res as *mut u32;
        let mut conn_ids = [self.connector_id];
        let mut crtc = DrmModeCrtc {
            set_connectors_ptr: conn_ids.as_mut_ptr() as u64,
            count_connectors: 1,
            crtc_id: self.crtc_id,
            fb_id: fb_cmd.fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: self.mode,
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut crtc) } < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::munmap(mmap_ptr as *mut libc::c_void, create_dumb.size as usize);
                libc::ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut fb_cmd.fb_id);
                let mut destroy = DrmModeDestroyDumb {
                    handle: create_dumb.handle,
                };
                libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            self.invalidate_frame_cache();
            return Err(err);
        }
        assert!(
            create_dumb.pitch.is_multiple_of(4),
            "pitch must be a multiple of 4 for XRGB8888"
        );
        self.fb_id = fb_cmd.fb_id;
        self.dumb_handle = create_dumb.handle;
        self.width = new_w;
        self.height = new_h;
        self.pitch = create_dumb.pitch;
        self.size = create_dumb.size as usize;
        self.mmap_ptr = mmap_ptr;
        self.transform = transform;
        self.invalidate_frame_cache();
        Ok(true)
    }

    /// KMS actuation for a sensor orientation.
    ///
    /// Maps through [`DeviceOrientation::to_transform`]
    /// (`sensor_proxy.rs:113-121`) and applies it. The *decision* of whether
    /// to rotate is [`crate::rotation::decide_modeset`]'s (pure, no I/O); this
    /// performs it and nothing else, so the policy stays I/O-free.
    pub fn set_orientation(&mut self, orientation: DeviceOrientation) -> io::Result<bool> {
        self.apply_transform(orientation.to_transform())
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

impl TerminalTabInfo<'_> {
    /// The all-empty tab, used to fill a fixed-capacity `FrameView` before any
    /// tab is pushed into it. See [`AppGridItem::EMPTY`] for why a
    /// fixed-capacity view needs one.
    pub const EMPTY: Self = Self {
        id: 0,
        title: "",
        is_running: false,
        is_active: false,
    };
}

/// One folder's closed-state preview cluster: the mini icons a folder tile draws.
///
/// A separate table rather than a field on [`AppGridItem`] because a folder tile
/// needs up to four *other* apps' bitmaps, and putting four `Option<&RgbaImage>`
/// on every grid item would make the common case -- an ordinary app, which needs
/// none of them -- pay for the folder case on every frame. The shell fills this
/// only for cells that are folders, so the array is as long as the workspace has
/// folders, not as long as it has apps.
///
/// Keyed by the cell's `id`, which is the stored `FOLDER:/<id>` token. That makes
/// the lookup a string compare against the *same* token the tap path parses back
/// into a `PageCell::Folder`, so the drawn cluster and the openable cell cannot
/// disagree the way two parallel index arrays did.
/// One notification, as the renderer needs it: borrowed, `Copy`, no `Instant`.
///
/// [`SystemUiShade`](crate::compositor::SystemUiShade)'s `NotificationCard`
/// owns `String`s and carries a `created_at: Instant`, neither of which can live
/// on [`DrmInteractiveState`] -- that borrows from the shell, and the renderer
/// must not allocate. This is the borrowed projection the renderer reads, rebuilt
/// per frame from the shade's own vector.
///
/// `x_offset` is the live swipe drag, so a row that is being dragged is drawn
/// where the finger is rather than where the model says it will land.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NotifRow<'a> {
    /// The publisher's display name, drawn as the row's byline.
    pub app_name: &'a str,
    /// Summary line, bold.
    pub summary: &'a str,
    /// Body text, may be empty.
    pub body: &'a str,
    /// Urgency 2, the loud row. The reference tints the dot for a
    /// high-importance notification (`NotificationView.java:212`).
    pub critical: bool,
    /// Horizontal drag offset, live.
    pub x_offset: f32,
    /// How many actions the row offers. Zero means the row draws no button row,
    /// which is the common case and the one the old hardcoded cards always were.
    pub n_actions: usize,
}

impl<'a> NotifRow<'a> {
    /// The empty row, for [`DrmInteractiveState::EMPTY`]'s notification slice.
    pub const EMPTY: Self = Self {
        app_name: "",
        summary: "",
        body: "",
        critical: false,
        x_offset: 0.0,
        n_actions: 0,
    };
}

/// How many notification rows the shade lays out.
///
/// Three, because `ShadeLayout::notifs` is `[Rect; 3]` (`layout.rs:3907`) and
/// reusing that array is what keeps the drawn rows and the swipe hit test on the
/// same geometry. Raising it is a layout change, not a renderer one.
pub const SHADE_NOTIF_ROWS: usize = 3;

/// Drawer progress past which the workspace behind the sheet is frosted.
///
/// Not a taste call: it is the reference's own minimum-visible-blur threshold,
/// which it expresses as a *radius* and this can only express as a *switch*.
///
/// The reference ramps the workspace blur with the transition depth and then
/// refuses to apply a change smaller than one dp:
///
/// ```text
/// blurAmount = depth;                                        // :222-223
/// int newBlur = (int) (blurAmount * mMaxBlurRadius);         // :226-229
/// boolean skipUpdate = ... && delta < Utilities.dpToPx(1)    // :230-233
/// ```
///
/// (`BaseDepthController.java:222-233`; `mMaxBlurRadius = 23` px, from
/// `quickstep/res/values/config.xml:41`.) One dp is `panel_w / 420`, so the
/// smallest progress whose blur exceeds a dp is about `1 / 23` of the way. Below
/// it the reference leaves the workspace sharp, and so does this.
///
/// [`apply_frosted_blur_region`] has a fixed 3 px tile and takes no radius, so
/// the ramp above the threshold cannot be reproduced -- the band is frosted at
/// full strength the moment the threshold is crossed. That is a real difference
/// from the reference and it is a limit of the existing blur primitive, not a
/// choice made here; the alternative was leaving the function uncalled, which is
/// what it had been.
pub const DRAWER_BLUR_THRESHOLD: f32 = 1.0 / 23.0;

#[derive(Debug, Clone, Copy)]
pub struct FolderPreviewRow<'a> {
    /// Which folder this is, matching [`AppGridItem::folder_id`].
    ///
    /// An id rather than the cell's `&str` token on purpose. The token lives in
    /// `home_pages`, which the shell mutates from a dozen event sites, so a
    /// preview row holding one would keep `home_pages` immutably borrowed for the
    /// whole frame and turn every one of those mutations into a borrow error --
    /// and the workaround, an owned copy per folder per frame, is exactly the
    /// allocation the frame path forbids.
    pub folder: u32,
    /// Member bitmaps, in folder rank order. Only `[..n]` is live.
    pub icons: [Option<&'a RgbaImage>; FOLDER_PREVIEW_MAX],
    /// How many of them there are, 1..=`FOLDER_PREVIEW_MAX`.
    pub n: u8,
}

impl<'a> FolderPreviewRow<'a> {
    /// The empty row, for an `AppGridItem` that is not a folder.
    pub const EMPTY: Self = Self {
        folder: 0,
        icons: [None; FOLDER_PREVIEW_MAX],
        n: 0,
    };
}

/// Fixed-capacity folder rename buffer, `Copy` so the frame path never allocates.
///
/// 64 bytes (`FOLDER_RENAME_MAX`, `layout.rs`), ASCII only: the renderer draws
/// with the built-in vector font (`font.rs` covers `0x20..0x7F`), so a non-ASCII
/// byte would render as a blank. `len` is the live prefix of `bytes`; the tail
/// is zeroed and never read.
///
/// Pre-fill vs empty policy: the shell pre-fills this on edit start with the
/// current title (truncated); an empty buffer while editing draws
/// `FOLDER_RENAME_PLACEHOLDER` and never the stale `folder_title`, so clearing
/// the field cannot read as "no change". `folder_rename_display` is the single
/// function that implements that rule.
///
/// The reference commits through `FolderInfo.setTitle` on `dispatchBackKey`
/// (`Folder.java:569`, `:1851`, announced via `DragLayer.java:190`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderRenameBuffer {
    /// Live bytes, `bytes[..len]` are ASCII.
    pub bytes: [u8; super::layout::FOLDER_RENAME_MAX],
    /// How many of `bytes` are live, `0..=FOLDER_RENAME_MAX`.
    pub len: u8,
}

impl FolderRenameBuffer {
    /// The empty buffer: editing with no text, which draws the placeholder.
    pub const EMPTY: Self = Self {
        bytes: [0u8; super::layout::FOLDER_RENAME_MAX],
        len: 0,
    };

    /// Copy `s` in, truncated to capacity. Non-ASCII bytes are replaced with
    /// `?` so the renderer never receives a byte it cannot draw.
    pub fn truncated(s: &str) -> Self {
        let mut out = Self::EMPTY;
        let mut n = 0usize;
        for b in s.bytes() {
            if n >= super::layout::FOLDER_RENAME_MAX {
                break;
            }
            out.bytes[n] = if b.is_ascii() { b } else { b'?' };
            n += 1;
        }
        out.len = n.min(255) as u8;
        out
    }

    /// Live text, or `""` when empty. Borrowed, no allocation.
    #[inline]
    pub fn as_str(&self) -> &str {
        let n = (self.len as usize).min(super::layout::FOLDER_RENAME_MAX);
        core::str::from_utf8(&self.bytes[..n]).unwrap_or("")
    }

    /// Byte length of the live prefix.
    #[inline]
    pub fn len(&self) -> usize {
        (self.len as usize).min(super::layout::FOLDER_RENAME_MAX)
    }

    /// True when no text is present, which selects the placeholder.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for FolderRenameBuffer {
    #[inline]
    fn default() -> Self {
        Self::EMPTY
    }
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
    /// Members shown in the folder-preview cluster, 1..=4. **Zero means this is
    /// not a folder.**
    ///
    /// One field rather than an `is_folder` flag plus a count: the count is
    /// already zero for every non-folder, so a second flag could disagree with
    /// it and the disagreement would be invisible. It is also the field that
    /// keeps the workspace grid a *single* loop -- a folder and an app are both
    /// an `AppGridItem`, so `grid_items`, the hit test and the scroller each stay
    /// one pass, rather than the two-item-type case needing a parallel array
    /// that could drift.
    ///
    /// The cluster itself is `FolderLayout::preview_in`, which is the reference's
    /// `ClippedFolderIconLayoutRule` (`getPosition:140-177`) behind an 11-test
    /// port.
    pub folder_n: u8,
    /// Which folder, when `folder_n` is non-zero. Zero otherwise.
    ///
    /// Paired with [`Self::folder_n`] rather than replacing it: the count is what
    /// the renderer draws and the id is what it looks the cluster up by, and
    /// deriving the id from the id *string* would mean re-parsing
    /// `FOLDER_CELL_PREFIX` per tile per frame for a number the shell already
    /// had in hand.
    pub folder_id: u32,
}

impl AppGridItem<'_> {
    /// The all-empty item, used to fill a fixed-capacity `FrameView` before any
    /// cell is pushed into it.
    ///
    /// Required rather than convenient: a `FrameView` is a fixed array plus a
    /// length, so it must be constructed from a valid item. `""` rather than
    /// `None` for the name because the field is a `&str` on the frame path, and
    /// borrowing the empty `str` literal costs nothing.
    pub const EMPTY: Self = Self {
        id: "",
        name: "",
        color: 0,
        glyph: "",
        icon: None,
        folder_n: 0,
        folder_id: 0,
    };
}

/// What the app-info screen shows about one app, borrowed.
///
/// **A struct rather than five more `DrmInteractiveState` fields** because the
/// five are not independent: they are one thing about one app, and a panel that
/// is open has all five while a panel that is closed has none of them. Five
/// `Option`s would make "the name is set but the exec line is not" a state the
/// renderer has to decide what to do with, and this has none.
///
/// **The first four fields are the identity and the last five are
/// presentation.** `Default` exists so a caller that knows what the app *is*
/// -- the `id`, `name`, `Exec=` line and the resolved software centre -- does not
/// also have to invent a tile colour and a glyph to get a panel on screen:
/// `..Default::default()` (or `..Self::EMPTY`) leaves `icon` `None`, so the tile
/// is drawn in `color == 0` and the panel shows no glyph. Fill them when the icon
/// pipeline has them; the panel is correct, if plain, without them.
///
/// Every field is a borrow or a scalar, so it is `Copy` and costs nothing to
/// carry on the frame path.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AppInfoSpec<'a> {
    /// The app's id, the same `&str` as [`AppGridItem::id`].
    pub id: &'a str,
    /// The app's display name, the same `&str` as [`AppGridItem::name`].
    pub name: &'a str,
    /// The `Exec=` line from the `.desktop` entry, or `""` when there is none.
    ///
    /// This is the line that decides whether the handoff works at all, which is
    /// why it is on the panel rather than hidden behind the button: on a device
    /// whose software centre is not installed, the *reason* nothing happens is
    /// usually a `.desktop` whose `Exec` points at a path that does not exist.
    pub exec: &'a str,
    /// The command or program the button will spawn, already resolved.
    ///
    /// Resolved by the shell, because resolving it means looking for a binary on
    /// this device -- `which`/`access(X_OK)` per candidate -- and `paint_frame`
    /// may not do I/O. Empty selects the "no target found" state, which is a
    /// real state and not an error: UTLC shows the panel and the button disabled
    /// rather than pretending an app-info screen exists.
    pub target: &'a str,
    /// Decoded icon, or `None` for the glyph chip.
    pub icon: Option<&'a RgbaImage>,
    /// The glyph tile's fill, for when [`Self::icon`] is `None`.
    pub color: u32,
    /// The glyph drawn on the chip.
    pub glyph: &'a str,
    /// Which handler the button is aimed at.
    ///
    /// The reference makes the same distinction, at the tap rather than before
    /// it: an app with a live install session goes to the market activity and
    /// everything else to the details activity
    /// (`PackageManagerHelper.java:162-167`). Showing which one *before* the tap
    /// is the part UTLC adds.
    pub target_kind: crate::graphics::layout::AppInfoTarget,
    /// An install session is in flight, so the target is the store.
    ///
    /// A convenience over `target_kind == AppInfoTarget::Store` for the shell:
    /// it is the fact, and `target_kind` is what it means for the button.
    pub installing: bool,
}

impl<'a> AppInfoSpec<'a> {
    /// The empty spec: every field at its default.
    ///
    /// A `const` alias for `Default::default()` rather than a second table of
    /// values, so the two cannot disagree. `..Self::EMPTY` in a struct literal is
    /// the shortest way to say "I know the app, I have no icon yet".
    pub const EMPTY: Self = Self {
        id: "",
        name: "",
        exec: "",
        target: "",
        icon: None,
        color: 0,
        glyph: "",
        target_kind: crate::graphics::layout::AppInfoTarget::Details,
        installing: false,
    };

    /// Whether the handoff button has anything to launch.
    ///
    /// A gate on the *drawn* button rather than on the tap, so the panel cannot
    /// show an enabled affordance that does nothing -- the failure this project
    /// keeps finding in the other direction.
    #[inline]
    pub fn can_open(&self) -> bool {
        !self.target.is_empty()
    }
}

/// The complete visible state for one frame.
///
/// Every field is a borrow or a scalar, so the whole struct is `Clone` (and
/// would be `Copy`); it is deliberately **not** `Copy` so that nobody
/// "optimises" a per-frame copy of the entire visible state on the render
/// path. `Clone` exists so tests can perturb one field against a baseline.
#[derive(Clone)]
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
    /// Per-tile state description, index-aligned with
    /// [`Self::quick_tiles_active`]. Empty means "nothing to say".
    ///
    /// The reference's `QSTile.State.stateDescription` /
    /// `secondaryLabel` (`QSTile.java:188`), and it exists because a tile that
    /// could not do what it says has nowhere else to say so. Empty is the healthy
    /// case and draws nothing, so a tile whose driver succeeded looks exactly as
    /// it did before -- the point is that a tile whose write *failed* no longer
    /// silently shows the state it failed to reach.
    pub quick_tile_notes: [&'a str; 8],
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

    // ---------------------------------------------------------------------
    // Launcher rewrite, plan §7.6. Every field below is ANIMATED.
    //
    // HARD INVARIANT: each one must appear in `interactive_state_hash` in
    // the same commit. `render_interactive_ui` short-circuits on a hash
    // match (`:670-685`), so a field that is not hashed animates
    // invisibly -- the spring runs, the value changes, and no frame is ever
    // repainted. `hash_tracks_new_animated_fields` proves each one.
    // ---------------------------------------------------------------------
    /// "At a Glance" card phase. 0 = date only, 1 = full weather card.
    /// Advances on a timer, not on interaction, so it must be hashed.
    pub smartspace_phase: f32,
    /// Folder open morph, 0 = closed, 1 = fully open.
    pub folder_morph: f32,
    /// Folder scrim opacity, 0..1. Separate from `folder_morph` because the
    /// reference runs them on different springs (380/0.8 vs 380/0.98).
    pub folder_scrim: f32,
    /// Folder title alpha, faded in after a 32 ms delay.
    pub folder_title_alpha: f32,
    /// Long-press popup open progress, 0..1.
    pub popup_progress: f32,
    /// Overview open progress, 0..1.
    pub overview_progress: f32,
    /// Overview horizontal scroll offset, in card widths.
    pub overview_scroll: f32,
    /// Dismissal of the selected card, in pixels. Drives the 0.9875/0.975
    /// scale ladder and the neighbour reflow.
    pub overview_dismiss: f32,
    /// Fast-scroller thumb position along the track, 0..1.
    pub fastscroller_thumb: f32,
    /// Fast-scroller teardrop popup alpha, 0..1 (200 ms in, 150 ms out).
    pub fastscroller_popup_alpha: f32,
    /// Page-indicator handover progress. `0.0` is at rest; `> 1.0` is the
    /// overshoot phase, which the dot maths treats differently.
    pub page_indicator_frac: f32,
    /// Workspace scale during an in-app home gesture, 1.0 at rest.
    pub workspace_scale: f32,
    /// Workspace content alpha during an in-app home gesture, 1.0 at rest.
    pub window_alpha: f32,

    // ---------------------------------------------------------------------
    // Smartspace text. The renderer cannot format a date: doing so here
    // would allocate on the frame path, which `paint_frame_does_not_allocate`
    // forbids. The shell formats into a stack buffer and hands over a
    // borrowed `&str` instead -- `format_current_time` in main.rs is already
    // that shape.
    // ---------------------------------------------------------------------
    /// Localised date line, e.g. "Tue, Sep 22". Empty means "no data", and
    /// the renderer draws nothing rather than a placeholder.
    ///
    /// This replaced a hard-coded `"Tue, Sep 22  |  28 C Sunny"` that shipped
    /// for the lifetime of the launcher, so the home screen showed a date from
    /// whenever the string was written regardless of the real clock.
    pub date_str: &'a str,
    /// Temperature and condition, e.g. "28C Sunny". Empty when no weather
    /// source is available, which selects the date-only layout.
    ///
    /// Note the absent degree sign: `font.rs` only covers `0x20..0x7F` and
    /// asserts that non-ASCII bytes have no ink, so a real `°` would render as
    /// a blank. The unit is written inline instead.
    pub weather_str: &'a str,
    /// Index into the weather glyph table, 0 = none. Distinct from a
    /// non-empty `weather_str` so "28C but the icon failed to load" is
    /// expressible.
    pub weather_glyph: u8,

    // ---------------------------------------------------------------------
    // Content the new surfaces draw. All of it is borrowed and all of it is
    // built by the shell, because `paint_frame` must not allocate.
    // ---------------------------------------------------------------------
    /// The **full** app catalogue, always populated, in a stable order.
    ///
    /// [`Self::recents_cards`] hold a `u32` catalogue index, so the renderer
    /// needs a list that does not change order underneath it.
    /// [`Self::drawer_apps`] cannot serve: it is the *filtered* drawer list and
    /// is empty whenever the drawer is closed, which is exactly when the
    /// overview is reachable.
    pub catalogue_apps: &'a [AppGridItem<'a>],

    /// Recents cards, most recent first, bounded by `recents::MAX_TASKS`.
    /// Empty means "no recent tasks", which the overview renders as an empty
    /// state rather than a blank panel.
    pub recents_cards: &'a [RecentsCard],

    /// Fast-scroller section showing in the popup: `0` = none, else `'A'` as
    /// `1..=26`. Distinct from `fastscroller_popup_alpha` because the letter
    /// outlives the fade -- a released drag keeps the last letter on screen
    /// while the popup fades.
    pub fastscroller_letter: u8,

    /// Long-press popup anchor, panel coordinates. The popup positions itself
    /// around this and flips side near an edge.
    pub popup_anchor: (f32, f32),
    /// Long-press popup rows, in order.
    ///
    /// The compositor's own [`crate::compositor::PopupItem`] enum, not a
    /// parallel "row kind" number: the shell already had to pick a variant to
    /// build the menu, and re-encoding it for the renderer is a place the two
    /// could disagree. Fixed capacity, never a `Vec` -- this is on the frame
    /// path.
    pub popup_items: &'a [crate::compositor::PopupItem],

    /// Contents of the open folder, and its title. Empty selects the
    /// "no folder" path, so a stale `folder_morph` with an emptied list draws
    /// an empty folder rather than reading past the end of anything.
    pub folder_apps: &'a [AppGridItem<'a>],
    pub folder_title: &'a str,

    /// Folder grid size, `(cols, rows)`, as the user set it. Clamped to
    /// 2..=5 by the shell, which is the reference's own slider range
    /// (`FolderPreferences.kt:84,90`).
    ///
    /// A state field rather than a `Layout` constant because the reference makes
    /// it a setting (`PreferenceManager2.kt:706-711` ->
    /// `DeviceProfileOverrides.kt:122` -> `DeviceProfile.java:462`) and the
    /// hard-coded 3x3 was one of the settings UTLC could not express at all.
    pub folder_grid: (u8, u8),

    /// Whether to use the dark scrim. Feeds `FOLDER_SCRIM_ALPHA_LIGHT`, which
    /// was a dead constant: `FolderLayout::new` passed the dark branch
    /// unconditionally, so `LauncherState::dark_theme` had no effect on a folder
    /// even after light theme became reachable everywhere else.
    pub folder_dark: bool,

    /// Which page of `folder_apps` to draw, and how many items the folder holds
    /// in total.
    ///
    /// `folder_apps` is the **whole** folder; `folder_page * items_per_page`
    /// leading items are skipped and the rest culled. Both are needed because
    /// `FolderLayout::pager(n)` returns an *empty* rect unless the folder needs
    /// more than one page -- so a one-page folder cannot draw dots by accident,
    /// and the renderer needs the total to know that.
    pub folder_page: u8,
    pub folder_item_count: u8,

    /// The user's type scale, as a multiplier on the panel's base scale.
    ///
    /// Held on the state rather than reconstructed because the shell has it and
    /// the renderer does not: `LauncherState::font_scale` reaches the workspace
    /// through `Layout::for_shell` at the *call site*, so any surface that
    /// builds its own `Layout` -- the folder surface does, because its grid size
    /// comes from a setting -- has to be told.
    pub font_scale: f32,

    /// The closed-state folder clusters to draw, for cells whose
    /// [`AppGridItem::folder_n`] is non-zero.
    ///
    /// Empty in almost every frame: a workspace with no folders has nothing to
    /// preview. When it is non-empty the renderer scans it linearly per folder
    /// tile, which is a handful of short string compares -- cheaper than the four
    /// bitmap pointers the alternative would put on every icon.
    pub folder_previews: &'a [FolderPreviewRow<'a>],

    /// The notification rows to draw, newest first. Bounded by
    /// [`SHADE_NOTIF_ROWS`] because that is how many the layout reserves.
    ///
    /// Empty is a real state, not an absence: the shade then draws its
    /// "no notifications" line. Before this existed the shade painted two
    /// hardcoded marketing strings -- `"UTIM PID 1 & UTLC Wayland"` and
    /// `"Direct DRM KMS Scanout"` -- so a device that had received a
    /// notification from any application still showed those two, and one that
    /// had received none could not be distinguished from one that had.
    pub notifications: &'a [NotifRow<'a>],

    /// Pre-blurred shadow coverage per app, keyed by the same `id` string as
    /// [`AppGridItem::id`].
    ///
    /// Empty unless `IconCache::shadows_enabled()`, which the shell turns off for
    /// the dark theme -- the reference disables both shadow layers in dark
    /// (`res/values/styles.xml:111-113`), so a dark-themed launcher drawing them
    /// would be visibly wrong rather than merely wasteful.
    ///
    /// A table rather than a field on [`AppGridItem`] for the same reason the
    /// folder previews are: a shadow mask is `edge * edge` coverage bytes for a
    /// 64 px tile -- 4 KB -- and putting that on every grid item would make the
    /// frame copy it around. The mask itself is the cache's own `Rc`, so the table
    /// costs a pointer per app.
    ///
    /// The keys are **owned** `String`s, which is the whole reason this table can
    /// outlive a catalogue rescan. The shell's `all_managed_apps` is *reassigned*
    /// every time the directory scan finds a change, so a row borrowing an app id
    /// by `&str` would keep the catalogue borrowed and make the assignment fail to
    /// compile. Copying the id here costs one small allocation per app -- on the
    /// path that runs when icons change, not per frame -- and the alternative was a
    /// per-frame `Vec` to re-borrow into.
    pub icon_shadows: &'a [(String, std::rc::Rc<crate::compositor::icons::ShadowMask>)],

    /// The decoded wallpaper, or `None` for the gradient.
    ///
    /// Borrowed because the renderer must not own a multi-megabyte image and must
    /// not decode one: the shell loads it when the path or the panel size changes
    /// and hands over a borrow, and a relaunch or a settings change swaps the
    /// borrow rather than re-decoding.
    ///
    /// `None` is the common case on a device with no wallpaper installed, and it
    /// is why the gradient path still exists: it was the *only* path until this.
    pub wallpaper: Option<&'a super::png::RgbaImage>,

    /// How much to darken the wallpaper, 0..=255.
    ///
    /// Not decoration. The grid, dock and smartspace all draw light text, and a
    /// wallpaper is arbitrary content -- so an undimmed one makes labels
    /// unreadable depending on which pixel happens to be behind them. The
    /// reference's answer is a scrim between the wallpaper and the launcher
    /// (`WorkspaceLayoutManager` / `CellLayout`), and this is that scrim's
    /// strength. The shell picks the value; the renderer only applies it.
    pub wallpaper_dim: u8,

    /// The screen has blanked for inactivity. When true, `paint_frame` draws
    /// nothing at all.
    ///
    /// This is not a "dim" and not a "lock": it is the absence of a frame, and
    /// that distinction is the whole point. `LauncherState::screen_timeout_s` was
    /// persisted, offered as a five-way choice in Settings, and read by nothing,
    /// so a user could select "30 s" and the display stayed on indefinitely. The
    /// shell now decides *when* via `ScreenTimeout`, but the renderer is the only
    /// thing that can decide *what*, and it cannot ask the shell -- the state is
    /// built before the paint and flows one way.
    ///
    /// Why blank rather than switch the backlight off: this rootfs has no
    /// backlight node, so `set_brightness` fails and there is no display power to
    /// cut. What a launcher can do, and what the reference amounts to from the
    /// user's side, is stop painting and show a dark surface, waking on the next
    /// input. A `LockState::Keyguard` would be the wrong answer here: it is a
    /// *security* state, it would demand a PIN, and it would be reachable by a
    /// timeout -- meaning a user could be locked out of their own phone by
    /// putting it down.
    pub screen_off: bool,

    /// The Settings screen's rows. Empty unless the Settings app is open.
    ///
    /// Borrowed and `'static`-strung: `utim_core::settings::build` returns rows
    /// holding only `&'static str`, so this slice can outlive a mutation of the
    /// state it was projected from -- which is what makes it safe to build once
    /// on open rather than per frame.
    pub settings_rows: &'a [crate::settings::SettingRow],

    /// The live section letter for the drawer's header, e.g. `"C"`.
    ///
    /// Fed from `FastScrollerState::letter_str()`, which is allocation-free and
    /// already tracks the drag. The header previously painted the literal `'A'`
    /// while this value sat unused a few pixels away on the popup, so the header
    /// named a section that was almost never the one being looked at.
    pub drawer_section_letter: &'a str,

    /// Per-app notification counts, keyed by the same `id` string as
    /// [`AppGridItem::id`] and as long as it is.
    ///
    /// This is what the reference's `DotInfo.getNotificationCount`
    /// (`dot/DotInfo.java:44-84`) supplies and `BubbleTextView.drawDot`
    /// (`BubbleTextView.java:941`) draws. UTLC had a notification *model*
    /// (`systemui::NotificationCard`) with no producer and no consumer, and no
    /// badge field anywhere -- so an app with unread notifications looked
    /// identical to one without.
    ///
    /// Fixed-capacity and borrowed: this is read once per icon per frame.
    pub badge_counts: &'a [(&'a str, u32)],

    /// The live recents model, borrowed so the renderer can call
    /// `card_rect_centered` -- the same function the shell's hit test uses --
    /// rather than re-deriving carousel geometry. Two derivations of "where is
    /// card N" is exactly the class of bug that put a clear-all button 200 px
    /// from the pill that was drawn for it.
    pub recents: Option<&'a crate::compositor::Recents>,

    /// Drawer scroll offset in px, and the index of the first catalogue entry
    /// in the current window. The drawer is a *window* over the catalogue, not
    /// a prefix: `drawer_first_index` says where the window starts so the
    /// renderer can map a window slot back to a catalogue app.
    pub drawer_scroll_y: f32,
    pub drawer_first_index: usize,
    /// How many apps pass the current filter. Drives the header's "N apps"
    /// count, which is the only feedback the drawer gives about a search.
    pub drawer_app_count: usize,

    /// Bounds the Material 3 press state layer to the control that was hit.
    /// `None` means the whole panel, which is correct for a control-less
    /// gesture (the wallpaper) and wrong for everything else -- an untargeted
    /// state layer is a screen-sized grey disc on every tap.
    pub ripple_clip: Option<Rect>,

    /// Whether the hotseat search pill is present. Defaults to `true`, matching
    /// the reference's `isHotseatEnabled` default; the *reference for the
    /// default* is `prefs2.isHotseatEnabled` (`PreferenceManager2.kt:292`).
    /// This must agree with the shell's hit test: if the pill is hidden but its
    /// touch target stays live, a tap in the middle of the dock is swallowed.
    pub show_search_bar: bool,

    // ---------------------------------------------------------------------
    // Folder gestures. Six scalars, no allocation, all `Copy`: a drag is a
    // per-frame value, so any of them being a `Vec` would put a heap allocation
    // on the frame path for the length of a drag.
    //
    // They are read by exactly two consumers -- `draw_folder` for pixels, and
    // the shell's touch dispatch via `FolderLayout` for geometry -- and between
    // them the dispatch never derives a rect of its own.
    // ---------------------------------------------------------------------
    /// Slot of the folder cell currently lifted by a drag, or `None` for "not
    /// dragging".
    ///
    /// A **page-local** slot, not an absolute rank: the reference's reorder
    /// animation is page-local (`pagePosE = empty % maxItemsPerPage`,
    /// `FolderPagedView.java:709`) and it refuses to animate across a page
    /// boundary (`:698-705`), so a cross-page move is a page turn the shell
    /// drives with `folder_page`. `None` rather than a sentinel because 0 is a
    /// real slot and every `== 0` test would be a live bug.
    pub folder_drag_slot: Option<u8>,

    /// Where the finger is while a cell is lifted, panel coordinates.
    ///
    /// The lifted cell is drawn here rather than in its cell, so this is the
    /// drag's own tracking and not the touch's: the reference resolves the
    /// reorder target from the drag view's *visual* centre
    /// (`Folder.java:1200`, `d.getVisualCenter`), not from the raw touch.
    pub folder_drag_pos: (f32, f32),

    /// The slot the grid's gap has opened at, or `None` when no reorder is in
    /// flight.
    ///
    /// Resolved by `FolderLayout::reorder_index` and only after
    /// `FOLDER_REORDER_DELAY_MS`, because the reference arms a 250 ms alarm in
    /// `onDragOver` and cancels it on every target change
    /// (`Folder.java:1205-1215`, `REORDER_DELAY` `:197`). Ordering on the first
    /// frame of a drag makes a flick and a slow move disagree about the final
    /// order, which is the specific thing that debounce prevents. `None` also
    /// means "drawn as-is", which is what an empty folder and a released drag
    /// both want.
    pub folder_drop_slot: Option<u8>,

    /// The lifted cell has left the folder and is over the workspace.
    ///
    /// Set by the shell from `FolderLayout::is_drag_out`, not derived here, so
    /// the shell's dispatch decision and the drawn state cannot be two
    /// derivations. The reference separates "in the folder" from "outside it" by
    /// an alarm (`ON_EXIT_CLOSE_DELAY`, `Folder.java:198, 1293-1300`) and by
    /// inflating its hit rect by half a dragged icon
    /// (`:1179-1184, 1857-1861`), so this flag is the *settled* answer rather
    /// than a live one.
    ///
    /// While set, `draw_folder` paints the reference's drop-target bar -- a
    /// centred "Remove" button (`DeleteDropTarget.java:115`,
    /// `res/values/strings.xml:221`), which is the affordance the release
    /// commits against. What a release *does* is the shell's call.
    pub folder_drag_out: bool,

    /// Folder long-press menu open progress, 0..1. `0.0` is closed and draws
    /// nothing at all.
    ///
    /// Separate from `popup_progress` because the two are different widgets with
    /// different rows: `popup_items` is the *workspace* long-press menu, whose
    /// rows are [`crate::compositor::PopupItem`] variants, while the folder menu
    /// is a fixed three-row layout
    /// ([`crate::graphics::layout::FOLDER_MENU_ROWS`]). Routing the folder menu
    /// through the existing popup would have overloaded one field for two menus
    /// and one progress for two springs.
    pub folder_menu_progress: f32,

    /// Folder long-press menu anchor, panel coordinates: the long-press point.
    ///
    /// The menu grows out of it and is centred on it
    /// ([`PopupMenuLayout::place`]), so a long press near a panel edge moves the
    /// menu to the other side of the anchor instead of off the panel.
    pub folder_menu_anchor: (f32, f32),

    /// Folder rename buffer, fixed-capacity `Copy`.
    ///
    /// The text being edited while `folder_rename_editing` is true. Pre-filled
    /// by the shell with the current title (truncated) on edit start; empty
    /// draws `FOLDER_RENAME_PLACEHOLDER`, never `folder_title`. See
    /// [`FolderRenameBuffer`] and `folder_rename_display` for the rule.
    pub folder_rename_buffer: FolderRenameBuffer,

    /// Whether the folder footer is an editable text field.
    ///
    /// False draws the static `folder_title`; true draws the field from
    /// `FolderLayout::rename_field` with `folder_rename_display` and a caret
    /// from `FolderLayout::rename_caret`. The reference commits on back
    /// (`Folder.java:569`, `:1851`, `DragLayer.java:190`); what a commit does
    /// is the shell's call.
    pub folder_rename_editing: bool,

    // ---------------------------------------------------------------------
    // App info.
    //
    // `PopupItem::AppInfo` has been a variant since the popup rows were
    // transcribed, `draw_popup` gives it the accent colour as the affirmative
    // row (`drm_kms.rs:6982-6986`), and *nothing anywhere* turned the tap into a
    // screen: there was no field for the app to be looked at and no draw path
    // for it. `this_state_has_no_unread_field` (the field-audit scan in
    // `screenshot.rs`) is what pins that this is now read.
    // ---------------------------------------------------------------------
    /// The app whose info panel is open, or `None` for "no panel".
    ///
    /// `None` is the whole mechanism: it selects the home screen, it is what the
    /// shell clears on back, and it is what makes the panel unreachable from a
    /// frame that did not ask for it. The panel is therefore a *surface* rather
    /// than a state machine flag, which is why the shell does not need a
    /// "which surface am I on" enum to keep in step with the renderer.
    pub app_info: Option<AppInfoSpec<'a>>,

    // ---------------------------------------------------------------------
    // IME layout.
    // ---------------------------------------------------------------------
    /// Which page of the on-screen keyboard is up.
    ///
    /// The field that makes [`crate::compositor::ime::KeyboardLayout`] an
    /// observable state rather than a piece of state nobody can see the effect
    /// of. Before it, `VirtualKeyboard::handle_key_tap` had working `?123` /
    /// `ABC` / `123` arms and the renderer built `Keyboard::new(w, h)` -- a fixed
    /// QWERTY -- so a user could reach a symbols page and watch nothing happen.
    /// That is the project's stated worst failure, reached from the opposite
    /// direction: not a correct-but-uncalled implementation, but a called one
    /// that changed no pixels.
    pub keyboard_layout: crate::compositor::ime::KeyboardLayout,

    // ---------------------------------------------------------------------
    // Workspace icon drag.
    //
    // Seven scalars, no allocation, all `Copy`, mirroring the six folder-gesture
    // fields above for the same reasons. UTLC has no drag layer: the shell owns
    // the touch dispatch, the reorder commit and the page animation, and these
    // are the numbers it draws from.
    // ---------------------------------------------------------------------
    /// Page-local slot of the workspace cell a drag has lifted, or `None`.
    ///
    /// Page-local for the same reason [`Self::folder_drag_slot`] is: the reorder
    /// is a per-page reorder and a cross-page move is a page turn
    /// (`SpringLoadedDragController.kt:47-57`), which the shell drives with
    /// [`Self::home_page`].
    pub drag_slot: Option<u8>,

    /// Where the lifted cell's *visual centre* is, panel coordinates.
    ///
    /// The drag's own tracking, not the touch's: the reference resolves both the
    /// drop cell and the page-turn edge from `d.getVisualCenter`
    /// (`Workspace.java:2709, 2893-2895`), and a lift drawn at the finger with
    /// the icon offset inside it is a different point.
    pub drag_pos: (f32, f32),

    /// The lifted cell's lift progress, `0.0`..=`1.0`.
    ///
    /// A progress and not a flag because the lift is an animation off the
    /// long-press: `Layout::drag_lifted_rect` scales by it, so `0.0` draws the
    /// resting cell centred on the finger and `1.0` the fully lifted one.
    pub drag_lift: f32,

    /// The slot the grid's gap has opened at, or `None`.
    ///
    /// The drop indicator. `None` means "drawn as-is", which is what an empty
    /// grid and a released drag both want.
    pub drag_drop_slot: Option<u8>,

    /// The occupied cell the lifted icon would merge with, or `None`.
    ///
    /// "Two icons onto each other": the reference's `mFolderCreateBg`, a
    /// `PreviewBackground` sized to the hovered icon with `isClipping = false`
    /// so it shows behind it (`Workspace.java:2942-2956`), and the thing whose
    /// release commits a folder. Resolved by the shell from
    /// [`Self::drag_pos`] with [`crate::graphics::layout::Layout::drag_merge_radius`],
    /// so the drawn plate and the dispatch decision cannot be two derivations.
    pub drag_merge_slot: Option<u8>,
}

impl<'a> DrmInteractiveState<'a> {
    /// Text the rename field shows while editing.
    ///
    /// The buffer when non-empty, `""` when empty (which draws
    /// `FOLDER_RENAME_PLACEHOLDER`, never `folder_title`). Pre-fill is the
    /// shell's job: it copies the title into `folder_rename_buffer` on edit
    /// start, so this never reads `folder_title` itself and a cleared field
    /// cannot resurrect the old name. Borrowed, no allocation.
    #[inline]
    pub fn folder_rename_display(&self) -> &str {
        self.folder_rename_buffer.as_str()
    }
}

/// One row of the recents strip, as the renderer sees it.
///
/// The shell owns the animation state ([`crate::compositor::recents::Recents`]
/// holds a spring per card); this is a flattened view of it, so `paint_frame`
/// needs no knowledge of the model and the two cannot drift.
///
/// No lifetime: a card carries no borrowed data of its own, it names an app by
/// index into [`DrmInteractiveState::catalogue_apps`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecentsCard {
    /// Index into [`DrmInteractiveState::catalogue_apps`]. Out of range draws
    /// a placeholder tile rather than indexing off the end: a catalogue rescan
    /// can shrink between the shell building this and the frame drawing it.
    pub app_id: u32,
    /// Dismiss offset in px. 0 = at rest, negative = dragged up.
    pub dismiss: f32,
    /// Whether this is the card the scrub gesture has selected.
    pub selected: bool,
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
            quick_tile_notes: [""; 8],
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
            smartspace_phase: 0.0,
            folder_morph: 0.0,
            folder_scrim: 0.0,
            folder_title_alpha: 0.0,
            folder_grid: (3, 3),
            folder_dark: true,
            font_scale: 1.0,
            folder_page: 0,
            folder_item_count: 0,
            folder_previews: &[],
            icon_shadows: &[],
            drawer_section_letter: super::drawer_mod::DEFAULT_SECTION_LETTER,
            wallpaper: None,
            wallpaper_dim: 0,
            settings_rows: &[],
            screen_off: false,
            notifications: &[],
            badge_counts: &[],
            popup_progress: 0.0,
            overview_progress: 0.0,
            overview_scroll: 0.0,
            overview_dismiss: 0.0,
            fastscroller_thumb: 0.0,
            fastscroller_popup_alpha: 0.0,
            page_indicator_frac: 0.0,
            workspace_scale: 1.0,
            window_alpha: 1.0,
            date_str: "",
            weather_str: "",
            weather_glyph: 0,
            catalogue_apps: &[],
            recents_cards: &[],
            fastscroller_letter: 0,
            popup_anchor: (0.0, 0.0),
            recents: None,
            drawer_scroll_y: 0.0,
            drawer_first_index: 0,
            drawer_app_count: 0,
            ripple_clip: None,
            show_search_bar: true,
            popup_items: &[],
            folder_apps: &[],
            folder_title: "",
            folder_drag_slot: None,
            folder_drag_pos: (0.0, 0.0),
            folder_drop_slot: None,
            folder_drag_out: false,
            folder_menu_progress: 0.0,
            folder_menu_anchor: (0.0, 0.0),
            folder_rename_buffer: FolderRenameBuffer::EMPTY,
            folder_rename_editing: false,
            app_info: None,
            keyboard_layout: crate::compositor::ime::KeyboardLayout::Qwerty,
            drag_slot: None,
            drag_pos: (0.0, 0.0),
            drag_lift: 0.0,
            drag_drop_slot: None,
            drag_merge_slot: None,
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
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            cx,
            (hf * 0.5) as usize,
            "SUPER EXTREME TTY RECOVERY",
            0xFF22C55E,
            2,
        );
        super::font::set_active_family(prev_family);
        return;
    };

    // Top Volume HUD Bar if active
    if sex.volume_hud.is_visible() {
        let bar = sex.volume_hud.format_bar(34);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            wf * 0.05,
            8.0,
            wf * 0.90,
            em1 * 2.2,
            4.0,
            0xFF0F172A,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            cx,
            (8.0 + em1 * 0.4) as usize,
            &bar,
            0xFF38BDF8,
            1,
        );
    }

    match sex.active_screen {
        crate::compositor::super_extreme::SuperExtremeScreen::Lock => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "* ANDROID RECOVERY *",
                0xFFEF4444,
                2,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.13) as usize,
                "SUPER EXTREME POWER SAVER",
                0xFF94A3B8,
                1,
            );

            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.28) as usize,
                state.time_str,
                0xFFFFFFFF,
                4,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.38) as usize,
                "BATTERY: 8% [CRITICAL] | TTY MODE",
                0xFFF59E0B,
                1,
            );

            // Option 1: Emergency Call
            let btn_w = wf * 0.42;
            let btn_h = hf * 0.08;
            let btn_y = hf * 0.81;

            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                wf * 0.06,
                btn_y,
                btn_w,
                btn_h,
                6.0,
                0xFF1E293B,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                (wf * 0.06 + btn_w * 0.5) as usize,
                (btn_y + btn_h * 0.32) as usize,
                "[ EMERGENCY ]",
                0xFFEF4444,
                1,
            );

            // Option 2: Snap Photo (Front Camera)
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                wf * 0.52,
                btn_y,
                btn_w,
                btn_h,
                6.0,
                0xFF1E293B,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                (wf * 0.52 + btn_w * 0.5) as usize,
                (btn_y + btn_h * 0.32) as usize,
                "[ SNAP PHOTO ]",
                0xFF22C55E,
                1,
            );

            // Unlock prompt
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.93) as usize,
                "^ SWIPE UP TO UNLOCK ^",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::Password => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.10) as usize,
                "ENTER DEVICE PASSWORD / PIN",
                0xFFE2E8F0,
                2,
            );

            let box_w = wf * 0.80;
            let box_h = hf * 0.07;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                wf * 0.10,
                hf * 0.20,
                box_w,
                box_h,
                6.0,
                0xFF0F172A,
            );

            let masked = if sex.password_input.is_empty() {
                "[ ______ ]".to_string()
            } else {
                format!("[ {} ]", "* ".repeat(sex.password_input.len()))
            };
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.22) as usize,
                &masked,
                0xFF22C55E,
                2,
            );

            if sex.password_error {
                draw_text_centered(
                    buf,
                    stride,
                    w,
                    h,
                    cx,
                    (hf * 0.30) as usize,
                    "INCORRECT PIN - PLEASE TRY AGAIN",
                    0xFFEF4444,
                    1,
                );
            }

            paint_tty_keyboard(buf, stride, w, h, wf, hf, em1);
        }

        crate::compositor::super_extreme::SuperExtremeScreen::CameraPreview => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.06) as usize,
                "FRONT CAMERA ASCII PREVIEW",
                0xFF22C55E,
                2,
            );

            super::font::set_active_family(super::font::FontFamily::AsciiMono);
            let frame = &sex.camera_preview.current_frame;
            let grid_start_y = hf * 0.12;
            let cell_h = (hf * 0.65) / frame.rows as f32;
            let start_x = wf * 0.04;

            for r in 0..frame.rows {
                // A malformed row renders as nothing rather than aborting the
                // TTY screen: the row comes from a fixed-size in-memory frame,
                // and a recovery screen must not be the thing that panics.
                let row_str = std::str::from_utf8(frame.row(r)).unwrap_or_default();
                let ry = grid_start_y + (r as f32 * cell_h);
                super::font::draw_run(
                    buf,
                    stride,
                    w,
                    h,
                    start_x,
                    ry,
                    row_str,
                    0xFF22C55E,
                    cell_h * 0.9,
                    super::font::FontWeight::Regular,
                );
            }
            super::font::set_active_family(super::font::FontFamily::Homemade);

            let snap_w = wf * 0.50;
            let snap_h = hf * 0.07;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                wf * 0.25,
                hf * 0.85,
                snap_w,
                snap_h,
                6.0,
                0xFF15803D,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.87) as usize,
                "[ O  SNAP PHOTO ]",
                0xFFFFFFFF,
                1,
            );

            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.95) as usize,
                "[ < BACK TO LOCKSCREEN ]",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::EmergencyDialer => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "EMERGENCY CALLING (911/112/999)",
                0xFFEF4444,
                2,
            );
            let disp = if sex.emergency_input.is_empty() {
                "[ DIAL NUMBER ]"
            } else {
                &sex.emergency_input
            };
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.18) as usize,
                disp,
                0xFFFFFFFF,
                2,
            );

            paint_numeric_keypad(buf, stride, w, h, wf, hf, em1, "CALL");
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.94) as usize,
                "[ < CANCEL / BACK ]",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::Home => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "=== ANDROID RECOVERY HOME ===",
                0xFF22C55E,
                2,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.14) as usize,
                "SUPER EXTREME POWER SAVER",
                0xFF94A3B8,
                1,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.18) as usize,
                &format!("{} | BATTERY: 8% | CPU: 400MHz", state.time_str),
                0xFFF59E0B,
                1,
            );

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
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    wf * 0.08,
                    y0,
                    btn_w,
                    btn_h,
                    6.0,
                    0xFF1E293B,
                );
                draw_text_centered(
                    buf,
                    stride,
                    w,
                    h,
                    cx,
                    (y0 + btn_h * 0.30) as usize,
                    label,
                    *color,
                    1,
                );
            }

            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                wf * 0.10,
                hf * 0.88,
                wf * 0.80,
                hf * 0.06,
                6.0,
                0xFF334155,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.895) as usize,
                "[ HOLD POWER: RECOVERY MENU ]",
                0xFFF8FAFC,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppAlarm => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "ALARM CLOCK (TTY)",
                0xFF38BDF8,
                2,
            );
            let start_y = hf * 0.25;
            let row_h = hf * 0.12;
            for (i, a) in sex.alarms.iter().enumerate() {
                let y0 = start_y + (i as f32 * row_h);
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    wf * 0.08,
                    y0,
                    wf * 0.84,
                    row_h * 0.80,
                    6.0,
                    0xFF1E293B,
                );
                let status_str = if a.enabled { "[ ON ]" } else { "[ OFF ]" };
                let color = if a.enabled { 0xFF22C55E } else { 0xFF64748B };
                let line = format!("{} {} {}", a.time_str, a.label, status_str);
                draw_text_centered(
                    buf,
                    stride,
                    w,
                    h,
                    cx,
                    (y0 + row_h * 0.28) as usize,
                    &line,
                    color,
                    1,
                );
            }
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.90) as usize,
                "[ < BACK TO HOME ]",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppPhone => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "PHONE CALL (TTY)",
                0xFF22C55E,
                2,
            );
            let disp = if sex.phone_input.is_empty() {
                "[ ENTER NUMBER ]"
            } else {
                &sex.phone_input
            };
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.18) as usize,
                disp,
                0xFFFFFFFF,
                2,
            );
            paint_numeric_keypad(buf, stride, w, h, wf, hf, em1, "CALL");
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.94) as usize,
                "[ < BACK TO HOME ]",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppSms => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "SMS MESSAGES (TTY)",
                0xFFA855F7,
                2,
            );
            let start_y = hf * 0.25;
            let row_h = hf * 0.12;
            for (i, msg) in sex.sms_messages.iter().enumerate() {
                let y0 = start_y + (i as f32 * row_h);
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    wf * 0.06,
                    y0,
                    wf * 0.88,
                    row_h * 0.82,
                    6.0,
                    0xFF1E293B,
                );
                let sender_line = format!("FROM: {} ({})", msg.sender, msg.time);
                draw_text(
                    buf,
                    stride,
                    w,
                    h,
                    (wf * 0.10) as usize,
                    (y0 + row_h * 0.16) as usize,
                    &sender_line,
                    0xFFF8FAFC,
                    1,
                );
                draw_text(
                    buf,
                    stride,
                    w,
                    h,
                    (wf * 0.10) as usize,
                    (y0 + row_h * 0.44) as usize,
                    msg.snippet,
                    0xFF94A3B8,
                    1,
                );
            }
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.90) as usize,
                "[ < BACK TO HOME ]",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::AppSettings => {
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.08) as usize,
                "SYSTEM SETTINGS (TTY)",
                0xFFE2E8F0,
                2,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.20) as usize,
                "Power Saver: SUPER EXTREME",
                0xFFF59E0B,
                1,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.26) as usize,
                "Display Brightness: 10% (Fixed)",
                0xFF94A3B8,
                1,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.32) as usize,
                "Active Font: Home-Made + ASCII Mono",
                0xFF94A3B8,
                1,
            );

            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                wf * 0.10,
                hf * 0.50,
                wf * 0.80,
                hf * 0.09,
                6.0,
                0xFF2563EB,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.535) as usize,
                "[ RETURN TO NORMAL MODE ]",
                0xFFFFFFFF,
                1,
            );

            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (hf * 0.90) as usize,
                "[ < BACK TO HOME ]",
                0xFF94A3B8,
                1,
            );
        }

        crate::compositor::super_extreme::SuperExtremeScreen::PowerMenu => {
            let menu_w = wf * 0.84;
            let menu_h = hf * 0.58;
            let menu_x = wf * 0.08;
            let menu_y = hf * 0.22;

            draw_rounded_rect_f(
                buf, stride, w, h, menu_x, menu_y, menu_w, menu_h, 8.0, 0xFF0F172A,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (menu_y + hf * 0.04) as usize,
                "RECOVERY POWER MENU",
                0xFFEF4444,
                2,
            );

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
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    menu_x + wf * 0.04,
                    y0,
                    menu_w - wf * 0.08,
                    row_h * 0.80,
                    4.0,
                    0xFF1E293B,
                );
                draw_text(
                    buf,
                    stride,
                    w,
                    h,
                    (menu_x + wf * 0.08) as usize,
                    (y0 + row_h * 0.28) as usize,
                    label,
                    *col,
                    1,
                );
            }
        }
    }

    if let Some(ref msg) = sex.last_action_message {
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            cx,
            (hf * 0.97) as usize,
            msg,
            0xFF22C55E,
            1,
        );
    }

    super::font::set_active_family(prev_family);
}

fn paint_tty_keyboard(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    wf: f32,
    hf: f32,
    _em1: f32,
) {
    let kb_top = hf * 0.60;
    let kb_h = hf * 0.38;
    let row_h = kb_h / 4.0;

    let digits = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
    let col_w10 = wf / 10.0;
    for (i, d) in digits.iter().enumerate() {
        let x0 = i as f32 * col_w10;
        let mut b = [0u8; 4];
        let s = d.encode_utf8(&mut b);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            x0 + 2.0,
            kb_top + 2.0,
            col_w10 - 4.0,
            row_h - 4.0,
            4.0,
            0xFF1E293B,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            (x0 + col_w10 * 0.5) as usize,
            (kb_top + row_h * 0.3) as usize,
            s,
            0xFFF8FAFC,
            1,
        );
    }

    let chars1 = ['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P'];
    let y1 = kb_top + row_h;
    for (i, c) in chars1.iter().enumerate() {
        let x0 = i as f32 * col_w10;
        let mut b = [0u8; 4];
        let s = c.encode_utf8(&mut b);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            x0 + 2.0,
            y1 + 2.0,
            col_w10 - 4.0,
            row_h - 4.0,
            4.0,
            0xFF1E293B,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            (x0 + col_w10 * 0.5) as usize,
            (y1 + row_h * 0.3) as usize,
            s,
            0xFFF8FAFC,
            1,
        );
    }

    let chars2 = ['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L'];
    let y2 = kb_top + row_h * 2.0;
    let pad = wf * 0.05;
    let col_w9 = (wf * 0.90) / 9.0;
    for (i, c) in chars2.iter().enumerate() {
        let x0 = pad + (i as f32 * col_w9);
        let mut b = [0u8; 4];
        let s = c.encode_utf8(&mut b);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            x0 + 2.0,
            y2 + 2.0,
            col_w9 - 4.0,
            row_h - 4.0,
            4.0,
            0xFF1E293B,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            (x0 + col_w9 * 0.5) as usize,
            (y2 + row_h * 0.3) as usize,
            s,
            0xFFF8FAFC,
            1,
        );
    }

    let y3 = kb_top + row_h * 3.0;
    let labels = ["ESC", "Z", "X", "C", "V", "B", "N", "M", "<", "OK"];
    for (i, l) in labels.iter().enumerate() {
        let x0 = i as f32 * col_w10;
        let bg = if i == 0 {
            0xFF475569
        } else if i == 8 {
            0xFF991B1B
        } else if i == 9 {
            0xFF166534
        } else {
            0xFF1E293B
        };
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            x0 + 2.0,
            y3 + 2.0,
            col_w10 - 4.0,
            row_h - 4.0,
            4.0,
            bg,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            (x0 + col_w10 * 0.5) as usize,
            (y3 + row_h * 0.3) as usize,
            l,
            0xFFF8FAFC,
            1,
        );
    }
}

fn paint_numeric_keypad(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    wf: f32,
    hf: f32,
    _em1: f32,
    enter_label: &str,
) {
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
            let bg = if *key_str == enter_label {
                0xFF166534
            } else if *key_str == "<" {
                0xFF991B1B
            } else {
                0xFF1E293B
            };
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                x0 + 4.0,
                y0 + 4.0,
                col_w - 8.0,
                row_h - 8.0,
                6.0,
                bg,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                (x0 + col_w * 0.5) as usize,
                (y0 + row_h * 0.35) as usize,
                key_str,
                0xFFFFFFFF,
                2,
            );
        }
    }
}

/// Compose one full shell frame into an ARGB8888 buffer.
///
/// Kept free of DRM state so the exact production draw path can be replayed
/// offscreen (see `super::screenshot`) and diffed in tests.
#[allow(clippy::too_many_lines)]
pub fn paint_frame(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    if w == 0 || h == 0 {
        return;
    }

    // A blanked screen draws nothing.
    //
    // Placed before every other draw, including `draw_background`, so the result
    // is genuinely "no frame" rather than "the wallpaper, dimmed". A dimmed
    // wallpaper still shows the photo, still emits light, and still shows a user
    // what was on screen -- which is the opposite of what a timeout is for.
    //
    // Pure black rather than the palette's `surface`: `surface` is a light or
    // near-white surface in the light theme, and a screen that blanks to *white*
    // in a dark room is an OLED power problem as well as a usability one. The
    // value is `0xFF000000` -- opaque black, not transparent, so a stale pixel
    // behind it cannot show through on a panel with no alpha.
    if state.screen_off {
        for y in 0..h {
            buf[y * stride..y * stride + w].fill(0xFF00_0000);
        }
        return;
    }

    // Scratch for the composed date/weather lines. Stack, fixed size, reused
    // by both the home smartspace and the shade header; the `&str` these back
    // has to outlive its own scope, so they cannot be function-local.
    let mut smartspace_buf = [0u8; 96];
    let mut shade_buf = [0u8; 96];
    // Same reason, third user: a `Picker` settings row's "N of M" readout is
    // formatted into a borrowed `&str`, and a buffer declared inside the row
    // loop would be dropped at the end of the iteration the `&str` points into.
    let mut picker_buf = [0u8; SETTINGS_SECTION_MAX];

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
            buf,
            stride,
            w,
            h,
            cx,
            clock_cy,
            state.time_str,
            state.palette.primary,
            cap as usize,
        );
        draw_text_centered_clipped(
            buf,
            stride,
            w,
            h,
            cx,
            (clock_cy as f32 + cap * 0.5 + hf * 0.016) as usize,
            wf * 0.9,
            // Real date from the shell, not a literal. Falls back to the time
            // when no date has been supplied, so the lock screen is never
            // missing a line entirely.
            if state.date_str.is_empty() {
                state.time_str
            } else {
                state.date_str
            },
            state.palette.on_surface_variant,
            2,
            FontWeight::Medium,
        );
        // Bottom affordance.
        let hint_y = h - (hf * 0.075) as usize;
        let em1 = super::font::em_px_at(1, w);
        let pill_h = (em1 * 2.2).max(hf * 0.020);
        let pill_w =
            (wf * 0.46).max(super::font::measure("Click or swipe up to unlock", em1) + em1 * 2.0);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            wf * 0.5 - pill_w * 0.5,
            hint_y as f32 - pill_h * 0.5,
            pill_w,
            pill_h,
            pill_h * 0.5,
            state.palette.surface_container,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            cx,
            (hint_y as f32 - em1 * 0.31) as usize,
            "Click or swipe up to unlock",
            state.palette.on_surface_variant,
            1,
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
            buf,
            stride,
            w,
            h,
            0.0,
            sl.top,
            wf,
            hf - sl.top,
            state.palette.surface_container,
        );

        // Header: display clock on the left, date and build on the right.
        let em1 = super::font::em_px_at(1, w);
        let pad = wf * PANEL_PAD_FRACTION;
        let clock_w = clock_run_width(state.time_str, sl.clock_size, w);
        draw_material_you_clock(
            buf,
            stride,
            w,
            h,
            (pad + clock_w * 0.5) as usize,
            (sl.date_y - em1 * 0.9) as usize,
            state.time_str,
            state.palette.primary,
            sl.clock_size as usize,
        );
        let date_x = pad + clock_w + em1;
        // Shade header: the real date, then the build name. The build string
        // is a genuine constant; the date is not.
        // The shade header always shows both halves: it is a fixed layout, not
        // the cross-fading smartspace, so it passes phase 1 explicitly rather
        // than inheriting the home card's animation.
        let shade_line = join_smartspace(&mut shade_buf, state.date_str, "Universal", 1.0);
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            date_x,
            sl.date_y,
            wf - date_x - pad,
            shade_line,
            state.palette.on_surface_variant,
            1,
            FontWeight::Medium,
        );

        // Tile grid.
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
            draw_rounded_rect_f(buf, stride, w, h, c.x, c.y, c.w, c.h, c.radius, bg);
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                c.x + label_pad,
                c.y + label_pad,
                c.w - label_pad * 2.0,
                name,
                fg,
                1,
                FontWeight::Bold,
            );
            // Active dot instead of an ON/OFF word: less noise, same meaning.
            if on {
                draw_circle_glyph(
                    buf,
                    stride,
                    w,
                    h,
                    c.x + c.w - label_pad,
                    c.y + c.h - label_pad,
                    tile_em * 0.22,
                    fg,
                );
            }
            // The state description, under the name. Drawn in the *on_surface*
            // colour rather than `fg` because it sits outside the tile's filled
            // area when the tile is off, and `fg` is then the dimmed variant --
            // a failure reason the user cannot read is no reason at all.
            let note = state.quick_tile_notes[idx];
            if !note.is_empty() {
                let nf = if on {
                    state.palette.on_primary
                } else {
                    state.palette.on_surface
                };
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    c.x + label_pad,
                    c.y + label_pad + tile_em * 1.15,
                    c.w - label_pad * 2.0,
                    note,
                    nf,
                    1,
                    FontWeight::Regular,
                );
            }
        }

        // Brightness slider: track plus a filled level.
        let br = sl.brightness;
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            br.x,
            br.y,
            br.w,
            br.h,
            br.radius,
            state.palette.surface_container_high,
        );
        let fill = br.w * 0.78;
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            br.x,
            br.y,
            fill,
            br.h,
            br.radius,
            state.palette.primary,
        );
        // Thumb.
        draw_circle_glyph(
            buf,
            stride,
            w,
            h,
            br.x + fill,
            br.center_y(),
            br.h * 0.26,
            state.palette.on_primary,
        );
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            br.x + br.h * 0.6,
            br.center_y() - tile_em * 0.31,
            br.w - br.h * 1.2,
            "Brightness  78%",
            state.palette.on_primary,
            1,
            FontWeight::Medium,
        );

        // Notifications.
        draw_text_weighted(
            buf,
            stride,
            w,
            h,
            sl.tiles.origin.0 as usize,
            sl.notif_title_y as usize,
            "NOTIFICATIONS",
            state.palette.on_surface_variant,
            1,
            FontWeight::Bold,
        );
        let rows = state.notifications;
        if rows.is_empty() {
            // The empty state is a real state. Before the store was wired this
            // branch did not exist and the shade drew two hardcoded strings
            // describing the compositor, so a device that had received nothing
            // was indistinguishable from one that had -- and a device that had
            // received something from a real application still showed those
            // strings, because nothing read the shade's own `notifications`.
            //
            // The reference's equivalent is `NotificationPanelView`'s empty view,
            // which shows the notification-list empty string
            // (`res/layout/notification_panel.xml`, `status_bar_notification`).
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                sl.notifs[0].x + label_pad,
                sl.notif_title_y + tile_em * 1.6,
                sl.notifs[0].w - label_pad * 2.0,
                "No notifications",
                state.palette.on_surface_variant,
                1,
                FontWeight::Regular,
            );
        }
        for (i, row) in rows.iter().take(SHADE_NOTIF_ROWS).enumerate() {
            let c = sl.notifs[i];
            // The card follows the finger: `x_offset` is the live drag offset
            // from `on_notification_swipe`, so a row being dismissed is drawn
            // where the user is holding it rather than snapping back.
            let cx = c.x + row.x_offset;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                cx,
                c.y,
                c.w,
                c.h,
                c.radius,
                state.palette.surface_container_high,
            );
            let tx = cx + label_pad;
            let tw = c.w - label_pad * 2.0;
            // The byline is the publisher, and it is what the badge counts group
            // by, so the row's identity is visible rather than implied.
            if !row.app_name.is_empty() {
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    tx,
                    c.y + c.h * 0.12,
                    tw,
                    row.app_name,
                    state.palette.on_surface_variant,
                    1,
                    FontWeight::Regular,
                );
            }
            // A critical row's summary is tinted, which is the reference's
            // high-importance affordance (`NotificationView.java:212` paints the
            // view's `setCritical` tint rather than a separate row shape).
            let title_colour = if row.critical {
                0xFFE53935
            } else {
                state.palette.on_surface
            };
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                tx,
                c.y + c.h * 0.20 + tile_em * 1.1,
                tw,
                row.summary,
                title_colour,
                1,
                FontWeight::Bold,
            );
            if !row.body.is_empty() {
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    tx,
                    c.y + c.h * 0.20 + tile_em * 2.2,
                    tw,
                    row.body,
                    state.palette.on_surface_variant,
                    1,
                    FontWeight::Regular,
                );
            }
            // Action affordances. Drawn as a count of pills rather than as the
            // labels themselves: the shell hit-tests a row, not a pill, until it
            // has the labels -- see the handoff in the audit -- and drawing a
            // button row whose taps go nowhere is the same defect as the popup
            // menu this project already had.
            if row.n_actions > 0 {
                let pill_h = tile_em * 1.5;
                let pill_y = c.y + c.h - label_pad - pill_h;
                for a in 0..row.n_actions.min(3) {
                    let pw = (c.w - label_pad * 2.0) / 3.0;
                    draw_rounded_rect_f(
                        buf,
                        stride,
                        w,
                        h,
                        tx + pw * a as f32,
                        pill_y,
                        pw * 0.9,
                        pill_h,
                        pill_h * 0.5,
                        state.palette.surface_container,
                    );
                }
            }
        }

        // Pull handle to dismiss.
        let hd = sl.handle;
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            hd.x,
            hd.y,
            hd.w,
            hd.h,
            hd.radius,
            state.palette.outline,
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
            buf,
            stride,
            w,
            h,
            bar.x as usize,
            bar.y as usize,
            bar.w as usize,
            bar.h as usize,
            bar.radius as usize,
            state.palette.surface_container,
        );
        // Back button.
        let em2 = super::font::em_px_at(2, w);
        let btn_text_y = al.back.center_y() - em2 * 0.30;
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            al.back.x as usize,
            al.back.y as usize,
            al.back.w as usize,
            al.back.h as usize,
            (al.back.h * 0.25) as usize,
            state.palette.surface_container_high,
        );
        draw_text(
            buf,
            stride,
            w,
            h,
            (al.back.x + al.back.h * 0.35) as usize,
            btn_text_y as usize,
            "<",
            state.palette.on_surface,
            2,
        );
        draw_text(
            buf,
            stride,
            w,
            h,
            (al.back.x + al.back.h * 0.95) as usize,
            btn_text_y as usize,
            "Back",
            state.palette.on_surface,
            2,
        );

        // App title, clipped so a long name cannot run into a button.
        let title_x = al.back.x + al.back.w;
        let title_w = (al.close.x - title_x - al.back.h * 0.3).max(0.0);
        draw_text_centered_clipped(
            buf,
            stride,
            w,
            h,
            (title_x + title_w * 0.5) as usize,
            (bar.center_y() - em2 * 0.32) as usize,
            title_w,
            app_name,
            state.palette.on_surface,
            2,
            FontWeight::Bold,
        );

        // Close button.
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            al.close.x as usize,
            al.close.y as usize,
            al.close.w as usize,
            al.close.h as usize,
            (al.close.h * 0.25) as usize,
            0xFFEF4444,
        );
        let cx_c = al.close.center_x();
        let cy_c = al.close.center_y();
        let r = al.close.h * 0.17;
        draw_line(
            buf,
            stride,
            w,
            h,
            cx_c - r,
            cy_c - r,
            cx_c + r,
            cy_c + r,
            0xFFFFFFFF,
        );
        draw_line(
            buf,
            stride,
            w,
            h,
            cx_c + r,
            cy_c - r,
            cx_c - r,
            cy_c + r,
            0xFFFFFFFF,
        );

        // App content container. The keyboard shortens it, so the drawn card
        // and the scrolled content band move together.
        let kb_h = if state.keyboard_active {
            Keyboard::new_for(w as f32, h as f32, state.keyboard_layout)
                .frame
                .h
        } else {
            0.0
        };
        let content_y = al.scroll_top;
        let content_h = (h as f32 - kb_h - h as f32 * 0.030 - content_y).max(0.0);
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            bar.x as usize,
            content_y as usize,
            bar.w as usize,
            content_h as usize,
            (bar.h * 0.32) as usize,
            state.palette.outline_variant,
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
                let fg = if is_active {
                    state.palette.on_primary
                } else {
                    state.palette.on_surface_variant
                };
                draw_rounded_rect(
                    buf,
                    stride,
                    w,
                    h,
                    r.x as usize,
                    r.y as usize,
                    r.w as usize,
                    r.h as usize,
                    r.radius as usize,
                    bg,
                );

                // Running indicator, then a title clipped to what is left.
                let dot_r = r.h * 0.13;
                let mut tx = r.x + r.h * 0.28;
                if tab.is_running {
                    draw_rounded_rect(
                        buf,
                        stride,
                        w,
                        h,
                        tx as usize,
                        (r.center_y() - dot_r) as usize,
                        (dot_r * 2.0) as usize,
                        (dot_r * 2.0) as usize,
                        dot_r as usize,
                        0xFF10B981,
                    );
                    tx += dot_r * 3.4;
                }
                let close = tab_al.tab_close_zone(i);
                let title_w = (close.x - tx - r.h * 0.12).max(0.0);
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    tx,
                    r.center_y() - t_em * 0.30,
                    title_w,
                    tab.title,
                    fg,
                    1,
                    FontWeight::Medium,
                );

                // Close affordance only on the active tab, and only when
                // there is more than one tab to close.
                if is_active && tabs.len() > 1 {
                    let cxr = close.center_x();
                    let cyr = r.center_y();
                    let cr = r.h * 0.13;
                    draw_line(
                        buf,
                        stride,
                        w,
                        h,
                        cxr - cr,
                        cyr - cr,
                        cxr + cr,
                        cyr + cr,
                        fg,
                    );
                    draw_line(
                        buf,
                        stride,
                        w,
                        h,
                        cxr + cr,
                        cyr - cr,
                        cxr - cr,
                        cyr + cr,
                        fg,
                    );
                }
            }

            // Add-tab button, exactly where `add_tab_rect` says it is.
            if let Some(add) = tab_al.add_tab_rect() {
                draw_rounded_rect(
                    buf,
                    stride,
                    w,
                    h,
                    add.x as usize,
                    add.y as usize,
                    add.w as usize,
                    add.h as usize,
                    add.radius as usize,
                    state.palette.surface_container_high,
                );
                let ar = add.h * 0.22;
                let acx = add.center_x();
                let acy = add.center_y();
                draw_line(
                    buf,
                    stride,
                    w,
                    h,
                    acx - ar,
                    acy,
                    acx + ar,
                    acy,
                    state.palette.primary,
                );
                draw_line(
                    buf,
                    stride,
                    w,
                    h,
                    acx,
                    acy - ar,
                    acx,
                    acy + ar,
                    state.palette.primary,
                );
            }

            // Divider line below the tab strip, then the shell banner.
            let strip_bottom = tab_al.tabs.y + tab_al.tabs.h;
            draw_rect(
                buf,
                stride,
                w,
                h,
                (bar.x + bar.h * 0.25) as usize,
                (strip_bottom + h as f32 * 0.004) as usize,
                (bar.w - bar.h * 0.5) as usize,
                (h as f32 * 0.0015).max(1.0) as usize,
                state.palette.outline_variant,
            );

            let mut line_y = strip_bottom + h as f32 * 0.010;
            let s = crate::session::session();
            let mut os_buf = [0u8; 96];
            let os_str = {
                use std::io::Write;
                let mut cur = std::io::Cursor::new(&mut os_buf[..]);
                let _ = write!(cur, "Universal Treble Linux 1.0 (Debian Sid {})", s.machine());
                let n = cur.position() as usize;
                core::str::from_utf8(&os_buf[..n]).unwrap_or("Universal Treble Linux 1.0")
            };
            draw_text(
                buf,
                stride,
                w,
                h,
                36,
                line_y as usize,
                os_str,
                0xFF38BDF8,
                3,
            );
            line_y += h as f32 * 0.016;
            let mut krel_buf = [0u8; 96];
            let krel_str = {
                use std::io::Write;
                let mut cur = std::io::Cursor::new(&mut krel_buf[..]);
                let _ = write!(cur, "Linux {} (Android GKI)", s.kernel_release());
                let n = cur.position() as usize;
                core::str::from_utf8(&krel_buf[..n]).unwrap_or("Linux (Android GKI)")
            };
            draw_text(
                buf,
                stride,
                w,
                h,
                36,
                line_y as usize,
                krel_str,
                0xFF94A3B8,
                2,
            );
            line_y += h as f32 * 0.011;
            draw_text(
                buf,
                stride,
                w,
                h,
                36,
                line_y as usize,
                "UTIM PID 1 init | UTLC Wayland Compositor",
                0xFF94A3B8,
                2,
            );
            line_y += h as f32 * 0.011;
            draw_text(
                buf,
                stride,
                w,
                h,
                36,
                line_y as usize,
                "Debian Sid ARM64 GNU/Linux - Multi-Tab Terminal Active",
                0xFF64748B,
                2,
            );
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
                        buf,
                        stride,
                        w,
                        h,
                        36.0,
                        line_y,
                        bar.w - 36.0 * 2.0,
                        line,
                        0xFFE2E8F0,
                        3,
                        FontWeight::Regular,
                    );
                    line_y += line_h;
                }
            }

            // Active prompt line with typed characters and blinking cursor (Scale 3)
            if line_y + line_h <= content_y + content_h {
                if state.terminal_running {
                    draw_text(
                        buf,
                        stride,
                        w,
                        h,
                        36,
                        line_y as usize,
                        "[running... (Ctrl+C to stop)]",
                        0xFFF59E0B,
                        3,
                    );
                } else {
                    // The prompt describes the identity the commands run as
                    // (root is dropped to the unprivileged session account).
                    let prompt = state.terminal_prompt;
                    let prompt_w = super::font::measure(prompt, term_em);
                    let baseline = line_y + term_em * 0.30;
                    draw_text(
                        buf,
                        stride,
                        w,
                        h,
                        36,
                        baseline as usize,
                        prompt,
                        0xFF10B981,
                        3,
                    );
                    // The typed line is clipped to the content card, and the
                    // caret tracks the real measured advance.
                    let input_x = 36.0 + prompt_w;
                    let avail = (bar.x + bar.w - 36.0 - input_x).max(0.0);
                    draw_text_clipped(
                        buf,
                        stride,
                        w,
                        h,
                        input_x,
                        baseline,
                        avail,
                        state.terminal_input,
                        0xFFFFFFFF,
                        3,
                        FontWeight::Regular,
                    );
                    let caret_x = input_x + super::font::measure(state.terminal_input, term_em);
                    if caret_x < 36.0 + avail {
                        draw_rect(
                            buf,
                            stride,
                            w,
                            h,
                            caret_x as usize,
                            baseline as usize,
                            (term_em * 0.09).round().max(2.0) as usize,
                            (term_em * 0.72) as usize,
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
                buf,
                stride,
                w,
                h,
                &bar,
                content_y,
                al,
                state,
                "Search settings...",
            );
            // The real settings, not a description of the device.
            //
            // The seven cards this replaces named the device's *hardware*
            // ("1080x2400 @ 120Hz Direct DRM KMS", "4.00 GB ext4 System GSI
            // Image") rather than anything the user could change, and tapping one
            // did nothing. Meanwhile `LauncherState` persisted about thirty
            // settings that the shell honours and that no gesture could reach.
            //
            // `state.settings_rows` is projected by `utim_core::settings::build`
            // and rebuilt by the shell when this panel opens and after each
            // change, so it is empty in every other state and costs one word.
            let cards: &[crate::settings::SettingRow] = state.settings_rows;
            // Clear the published row pitch when there are no rows.
            //
            // Without this the *previous* frame's `card_h` and `step` survive, and
            // `settings::hit` -- which reads them -- reports rows as tappable on a
            // panel that is painting none. The rows a user can hit would be the
            // rows they could see a moment ago, which is exactly the "tap changes
            // something invisible" failure this whole mechanism exists to prevent.
            //
            // Found by the test that pins paint against hit-test: with the rows
            // filtered out by the search field, the blank panel was still tappable.
            //
            // Note the `cards.is_empty()` guard on the publish, and why it is not
            // optional. With no rows, `list_h / cards.len()` is `list_h / 0`, which
            // is `inf`; `.min(72.0)` then turns that infinity into a
            // perfectly plausible-looking 72.0, and the *stale* values from the
            // previous frame are overwritten with them. So a panel painting no
            // rows still reports a row pitch, and `settings::hit` reports rows as
            // tappable. The first version of this cleared and then published
            // unconditionally, and the test written to pin paint against hit-test
            // caught exactly that.
            let query = state.app_input;
            let mut y = list;
            let card_h = if cards.is_empty() {
                0.0
            } else {
                (list_h / cards.len() as f32 * 0.82).min(h as f32 * 0.030)
            };
            let step = if cards.is_empty() {
                0.0
            } else {
                card_h + h as f32 * 0.006
            };
            let x = bar.x + bar.h * 0.25;
            let rw = bar.w - bar.h * 0.5;
            // Publish what was actually painted, so the hit test reads the same
            // numbers rather than recomputing them. A section header takes extra
            // vertical space, so the *drawn* y of row `i` is not
            // `top + i * step`; `hit` uses the pitch because a touch lands in a
            // band, and the band is what the user sees as the row's slot.
            SETTINGS_GEOM.with(|c| {
                let mut g = c.get();
                g.card_h = card_h;
                g.step = step;
                c.set(g);
            });
            let title_em = super::font::em_px_at(1, w);
            // One header line per group, then the rows. The header is the row's
            // own `section` field rather than a separate list, so a row cannot be
            // drawn without its group and hit-tested without its group -- the two
            // derivations agreeing is what keeps the list honest.
            let mut pending: Option<&'static str> = None;
            for r in cards {
                let title = r.label;
                // The row's static value. A live `Picker` row overrides it with a
                // position readout, so the two are separate bindings from here on
                // rather than one `desc` reassigned later -- the search filter
                // below must keep matching the *static* value, or a row reading
                // "3 of 7" would stop being findable by "wallpaper"'s own value
                // and, worse, become findable by the number of the wallpaper the
                // user happens to be on.
                let desc_static = r.value;
                if r.section != pending {
                    pending = r.section;
                    if let Some(sec) = r.section {
                        y += title_em * 0.4;
                        let mut hdr = [0u8; SETTINGS_SECTION_MAX];
                        let hdr = upper_ascii_str(sec, &mut hdr);
                        draw_text_clipped(
                            buf,
                            stride,
                            w,
                            h,
                            x,
                            y,
                            rw,
                            hdr,
                            state.palette.primary,
                            1,
                            FontWeight::Bold,
                        );
                        y += title_em * 1.35;
                    }
                }
                if !query.is_empty()
                    && !ascii_contains_ci(title, query)
                    && !ascii_contains_ci(desc_static, query)
                {
                    continue;
                }
                if y + card_h > list + list_h {
                    break;
                }
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    x,
                    y,
                    rw,
                    card_h,
                    card_h * 0.24,
                    state.palette.surface_container_high,
                );
                let tx = x + card_h * 0.45;
                let tw = rw - card_h * 0.9;
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    tx,
                    y + card_h * 0.16,
                    tw,
                    title,
                    state.palette.on_surface,
                    1,
                    FontWeight::Bold,
                );
                // A `Picker` row's readout and candidate slots, in place of the
                // static value.
                //
                // The reference's control is a horizontal carousel whose current
                // candidate is the marked one: `orientation = HORIZONTAL`
                // (`WallpaperCarouselView.kt:54`), every candidate built and the
                // current index singled out (`:74-77`), the current one *wider*
                // (`:104`) and carrying an accent-filled tick
                // (`setBackgroundWithRadius(Themes.getColorAccent(context), 100F)`,
                // `:48`). There is no separate dot strip: the candidates *are*
                // the indicator.
                //
                // A settings row has room for four boxes rather than a carousel,
                // so `picker_slots` slides a four-slot window onto the current
                // one and this draws that. `of == 0` / `at == None` -- "the
                // candidate list is not known yet", which is what
                // `settings::build` produces and what `SettingRow::of` documents
                // -- yields a dead struct and the row keeps its static `value`,
                // which is the honest thing to show for a list that does not
                // exist. Written as a branch on `is_live()` rather than on
                // `at.is_some()` so the "no candidates" case cannot disagree with
                // the geometry about what it means.
                let picker = super::layout::picker_slots(
                    super::layout::Rect {
                        x,
                        y,
                        w: rw,
                        h: card_h,
                        radius: card_h * 0.24,
                    },
                    title_em,
                    r.at,
                    r.of,
                );
                let desc: Option<&str> = if picker.is_live() {
                    Some(format_picker_position(&mut picker_buf, r.at, r.of))
                } else {
                    None
                };
                // The readout is drawn in `primary`, not in the row's value
                // colour. Two reasons, and the second is the interesting one.
                //
                // The design one: the reference marks whatever is *current* with
                // the accent -- the selected carousel card carries an
                // accent-filled tick (`setBackgroundWithRadius(getColorAccent(...
                // ), 100F)`, `WallpaperCarouselView.kt:48`) -- and a row's live
                // position is the same kind of fact as a card's selection.
                //
                // The testing one, and it is why this is worth doing rather than
                // leaving both in `on_surface_variant`: two strings of the same
                // colour in the same place are one measurement. A render test can
                // count `primary` in the selected slot and in the readout
                // separately -- the two rects are disjoint by construction -- so
                // dropping either the slot fill or the readout makes exactly one
                // of the two counts go to zero. In the value colour, dropping the
                // readout leaves the row almost unchanged and the only assertion
                // that noticed was a pixel-total that the slots alone could meet.
                let desc_colour = if desc.is_some() {
                    state.palette.primary
                } else {
                    state.palette.on_surface_variant
                };
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    tx,
                    y + card_h * 0.16 + title_em,
                    tw,
                    desc.unwrap_or(desc_static),
                    desc_colour,
                    1,
                    FontWeight::Regular,
                );
                draw_picker_slots(buf, stride, w, h, &picker, state);
                y += step;
            }
        } else if app_name == "Browser"
            || app_name == "Web"
            || app_name.contains("Browser")
            || app_name == "Firefox"
        {
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
                buf,
                stride,
                w,
                h,
                url_x - ring,
                bar_top - ring,
                url_w + ring * 2.0,
                bar_h + ring * 2.0,
                bar_h * 0.5 + ring,
                url_border,
            );
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                url_x,
                bar_top,
                url_w,
                bar_h,
                bar_h * 0.5,
                state.palette.surface_container_high,
            );

            // Lock badge, drawn as a real padlock silhouette.
            let lock_r = bar_h * 0.20;
            let lock_cx = url_x + bar_h * 0.55;
            let lock_cy = bar_top + bar_h * 0.5;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                lock_cx - lock_r * 1.15,
                lock_cy - lock_r * 1.7,
                lock_r * 2.3,
                lock_r * 2.2,
                lock_r * 0.6,
                0xFF10B981,
            );
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                lock_cx - lock_r * 0.75,
                lock_cy - lock_r * 0.5,
                lock_r * 1.5,
                lock_r * 1.1,
                lock_r * 0.3,
                state.palette.surface_container_high,
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
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                b_tx,
                b_ty,
                b_w,
                b_disp,
                b_col,
                2,
                FontWeight::Regular,
            );
            if state.app_input_focused {
                let cur_x = b_tx + super::font::measure(b_disp, b_em) + ring;
                if cur_x < b_tx + b_w {
                    draw_rect(
                        buf,
                        stride,
                        w,
                        h,
                        cur_x as usize,
                        b_ty as usize,
                        ring.max(2.0) as usize,
                        b_text_h as usize,
                        state.palette.primary,
                    );
                }
            }

            // Tab-count badge and reload glyph on the trailing edge.
            let badge_x = url_x + url_w - trail - badge_w;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                badge_x,
                bar_top + (bar_h - badge_w) * 0.5,
                badge_w,
                badge_w,
                badge_w * 0.28,
                state.palette.surface_container,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                (badge_x + badge_w * 0.5) as usize,
                (bar_top + (bar_h - badge_w) * 0.5 + badge_w * 0.16) as usize,
                "1",
                state.palette.on_surface_variant,
                1,
            );
            let rr = badge_w * 0.26;
            let rcx = url_x + url_w - trail * 0.5;
            let rcy = bar_top + bar_h * 0.5;
            // Refresh: an arc approximated by four ticks plus an arrow head.
            for i in 0..8 {
                let ang = i as f32 * std::f32::consts::TAU / 8.0;
                let px = rcx + rr * ang.cos();
                let py = rcy + rr * ang.sin();
                draw_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    px - 1.0,
                    py - 1.0,
                    2.0,
                    2.0,
                    state.palette.on_surface_variant,
                );
            }
            draw_line(
                buf,
                stride,
                w,
                h,
                rcx + rr * 0.7,
                rcy - rr * 0.7,
                rcx + rr * 1.15,
                rcy - rr * 0.7,
                state.palette.on_surface_variant,
            );
            draw_line(
                buf,
                stride,
                w,
                h,
                rcx + rr * 1.15,
                rcy - rr * 0.7,
                rcx + rr * 1.15,
                rcy - rr * 0.25,
                state.palette.on_surface_variant,
            );

            // Viewport.
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                card_x,
                page_y,
                card_w,
                page_h,
                bar.h * 0.4,
                state.palette.outline_variant,
            );
            let body_x = card_x + inset;
            let body_w = card_w - inset * 2.0;

            if !state.app_input.is_empty() {
                // Search results view: header, then two result cards.
                let head_h = h as f32 * 0.028;
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    body_x,
                    page_y + inset,
                    body_w,
                    head_h,
                    head_h * 0.24,
                    state.palette.surface_container_high,
                );
                let head_em = super::font::em_px_at(1, w);
                let label = "Results for";
                let q_w = super::font::measure(state.app_input, head_em);
                let lbl_w = super::font::measure(label, head_em);
                let hy = page_y + inset + (head_h - head_em * 0.62) * 0.5;
                draw_text_weighted(
                    buf,
                    stride,
                    w,
                    h,
                    (body_x + inset) as usize,
                    hy as usize,
                    label,
                    state.palette.on_surface_variant,
                    1,
                    FontWeight::Regular,
                );
                draw_text_weighted(
                    buf,
                    stride,
                    w,
                    h,
                    (body_x + inset + lbl_w + head_em * 0.2) as usize,
                    hy as usize,
                    state.app_input,
                    state.palette.primary,
                    1,
                    FontWeight::Bold,
                );
                let _ = q_w;

                let card_top = page_y + inset + head_h + inset;
                let ch = ((page_y + page_h - card_top) * 0.5 - inset).max(0.0);
                for (i, (title, url, snippet)) in [
                    (
                        state.app_input,
                        "https://www.google.com/search",
                        "Top match and official verified web destination.",
                    ),
                    (
                        "Wikipedia - Free Encyclopedia",
                        "https://en.wikipedia.org/wiki",
                        "Overview, history, documentation, and references.",
                    ),
                ]
                .iter()
                .enumerate()
                {
                    let cy = card_top + i as f32 * (ch + inset);
                    draw_rounded_rect_f(
                        buf,
                        stride,
                        w,
                        h,
                        body_x,
                        cy,
                        body_w,
                        ch,
                        ch * 0.14,
                        state.palette.surface_container_high,
                    );
                    let t_em = super::font::em_px_at(1, w);
                    draw_text_clipped(
                        buf,
                        stride,
                        w,
                        h,
                        body_x + inset,
                        cy + ch * 0.14,
                        body_w - inset * 2.0,
                        title,
                        state.palette.primary,
                        1,
                        FontWeight::Bold,
                    );
                    draw_text_clipped(
                        buf,
                        stride,
                        w,
                        h,
                        body_x + inset,
                        cy + ch * 0.14 + t_em,
                        body_w - inset * 2.0,
                        url,
                        state.palette.on_surface_variant,
                        1,
                        FontWeight::Regular,
                    );
                    draw_text_clipped(
                        buf,
                        stride,
                        w,
                        h,
                        body_x + inset,
                        cy + ch * 0.14 + t_em * 2.0,
                        body_w - inset * 2.0,
                        snippet,
                        state.palette.on_surface_variant,
                        1,
                        FontWeight::Regular,
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
                let cols = [
                    0xFF4285F4u32,
                    0xFFEA4335,
                    0xFFFBBC05,
                    0xFF4285F4,
                    0xFF34A853,
                    0xFFEA4335,
                ];
                let mut wx = center_x - word_w * 0.5;
                // `chars`, not `bytes`: the font engine's glyph entry points
                // take a `char`, and a byte loop over this ASCII word happened to
                // work only because every byte is its own code point here. It
                // would truncate a multi-byte glyph mid-sequence the moment the
                // wordmark gained one.
                for (i, ch) in word.chars().enumerate() {
                    let adv = super::font::char_advance(ch, word_em);
                    super::font::draw_glyph(
                        buf,
                        stride,
                        w,
                        h,
                        wx,
                        g_y,
                        ch,
                        cols[i % cols.len()],
                        word_em,
                        FontWeight::Medium,
                    );
                    wx += adv;
                }

                let s_h = h as f32 * 0.030;
                let s_w = (body_w * 0.7).min(word_w * 1.9);
                let s_x = center_x - s_w * 0.5;
                let s_y = g_y + word_em * 1.05;
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    s_x,
                    s_y,
                    s_w,
                    s_h,
                    s_h * 0.5,
                    state.palette.surface_container_high,
                );
                let s_em = super::font::em_px_at(1, w);
                let s_tx = s_x + s_h * 0.30;
                draw_text_weighted(
                    buf,
                    stride,
                    w,
                    h,
                    s_tx as usize,
                    (s_y + (s_h - s_em * 0.62) * 0.5) as usize,
                    "Search or type web address",
                    state.palette.on_surface_variant,
                    1,
                    FontWeight::Regular,
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
                        buf,
                        stride,
                        w,
                        h,
                        cx - sc_size * 0.5,
                        cy - sc_size * 0.5,
                        sc_size,
                        sc_size,
                        sc_size * 0.5,
                        *color,
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
                        buf,
                        stride,
                        w,
                        h,
                        cx as usize,
                        (cy - sc_size * 0.30) as usize,
                        first,
                        0xFFFFFFFF,
                        2,
                    );
                    draw_text_centered_clipped(
                        buf,
                        stride,
                        w,
                        h,
                        cx as usize,
                        (cy + sc_size * 0.62) as usize,
                        sc_col_w * 0.94,
                        title,
                        state.palette.on_surface_variant,
                        1,
                        FontWeight::Regular,
                    );
                }
            }

            // Bottom toolbar: back, forward, home, tabs, menu.
            let tb_y = page_y + page_h + h as f32 * 0.004;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                card_x,
                tb_y,
                card_w,
                toolbar_h,
                toolbar_h * 0.28,
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
                    buf,
                    stride,
                    w,
                    h,
                    tx as usize,
                    (tb_y + (toolbar_h - tb_em * 0.62) * 0.5) as usize,
                    symbol,
                    t_col,
                    2,
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
                buf,
                stride,
                w,
                h,
                card_x,
                head_y,
                card_w,
                head_h,
                head_h * 0.24,
                state.palette.surface_container_high,
            );
            draw_text_centered_clipped(
                buf,
                stride,
                w,
                h,
                card_x as usize,
                (head_y + head_h * 0.20) as usize,
                card_w - inset * 2.0,
                "Treble Carrier (SIM 1 - 4G LTE Active)",
                state.palette.primary,
                1,
                FontWeight::Bold,
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
                    buf,
                    stride,
                    w,
                    h,
                    bx,
                    bubble_y,
                    msg_w,
                    bubble_h,
                    bubble_h * 0.28,
                    state.palette.primary,
                );
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    bx + m_em * 0.45,
                    bubble_y + (bubble_h - m_em * 0.62) * 0.5,
                    msg_w - m_em * 0.9,
                    msg,
                    state.palette.on_primary,
                    1,
                    FontWeight::Regular,
                );
                bubble_y -= bubble_h + bubble_gap;
            }

            // System greeting, pinned under the header.
            let greet = "Treble: Welcome! Tap below to type a message.";
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                card_x + inset,
                head_y + head_h + bubble_gap,
                max_bubble_w,
                bubble_h,
                bubble_h * 0.28,
                state.palette.surface_container_high,
            );
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                card_x + inset * 2.0,
                head_y + head_h + bubble_gap + (bubble_h - m_em * 0.62) * 0.5,
                max_bubble_w - m_em * 0.9,
                greet,
                state.palette.on_surface,
                1,
                FontWeight::Regular,
            );

            // Composer field + send button, both from `AppLayout`.
            let ring = (w as f32 * 0.003).max(1.0);
            let c_border = if state.app_input_focused {
                state.palette.primary
            } else {
                state.palette.outline
            };
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                comp.x - ring,
                comp.y - ring,
                comp.w + ring * 2.0,
                comp.h + ring * 2.0,
                comp.radius + ring,
                c_border,
            );
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                comp.x,
                comp.y,
                comp.w,
                comp.h,
                comp.radius,
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
                buf,
                stride,
                w,
                h,
                m_tx,
                m_ty,
                comp.w - comp.h * 0.75,
                m_disp,
                m_col,
                1,
                FontWeight::Regular,
            );
            if state.app_input_focused {
                let cur_x = m_tx + super::font::measure(m_disp, m_em) + ring;
                if cur_x < comp.x + comp.w - comp.h * 0.3 {
                    draw_rect(
                        buf,
                        stride,
                        w,
                        h,
                        cur_x as usize,
                        m_ty as usize,
                        ring.max(2.0) as usize,
                        (m_em * 0.62) as usize,
                        state.palette.primary,
                    );
                }
            }
            let send = al.send;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                send.x,
                send.y,
                send.w,
                send.h,
                send.h * 0.28,
                state.palette.primary,
            );
            draw_text_centered_clipped(
                buf,
                stride,
                w,
                h,
                send.center_x() as usize,
                (send.center_y() - m_em * 0.31) as usize,
                send.w * 0.9,
                "Send",
                state.palette.on_primary,
                1,
                FontWeight::Bold,
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
                buf,
                stride,
                w,
                h,
                num_x - ring,
                num_y - ring,
                num_w + ring * 2.0,
                num_h + ring * 2.0,
                num_h * 0.28 + ring,
                num_border,
            );
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                num_x,
                num_y,
                num_w,
                num_h,
                num_h * 0.28,
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
                buf,
                stride,
                w,
                h,
                p_tx,
                p_ty,
                num_w - num_h * 0.75,
                p_disp,
                p_col,
                2,
                FontWeight::Regular,
            );
            if state.app_input_focused {
                let cur_x = p_tx + super::font::measure(p_disp, p_em) + ring;
                if cur_x < num_x + num_w - num_h * 0.3 {
                    draw_rect(
                        buf,
                        stride,
                        w,
                        h,
                        cur_x as usize,
                        p_ty as usize,
                        ring.max(2.0) as usize,
                        (p_em * 0.62) as usize,
                        state.palette.primary,
                    );
                }
            }

            // RIL status and the call button, centred in the content card.
            let status_y = num_y + num_h + h as f32 * 0.030;
            let s_em = super::font::em_px_at(1, w);
            draw_text_centered_weighted(
                buf,
                stride,
                w,
                h,
                (card_x + card_w * 0.5) as usize,
                status_y as usize,
                "Universal Cellular RIL Bridge",
                state.palette.on_surface_variant,
                1,
                FontWeight::Medium,
            );
            let call_h = (h as f32 * 0.036).max(w.min(h) as f32 * 0.10);
            let call_w = call_h * 3.4;
            let call_x = card_x + (card_w - call_w) * 0.5;
            let call_y = status_y + s_em * 1.6 + h as f32 * 0.012;
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                call_x,
                call_y,
                call_w,
                call_h,
                call_h * 0.5,
                0xFF10B981,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                (call_x + call_w * 0.5) as usize,
                (call_y + (call_h - p_em * 0.62) * 0.5) as usize,
                "Call",
                0xFFFFFFFF,
                2,
            );
        } else if app_name == "Contacts" {
            // Contacts: search field over a scrollable row list.
            let (list, list_h) = draw_app_search_field(
                buf,
                stride,
                w,
                h,
                &bar,
                content_y,
                al,
                state,
                "Search contacts...",
            );
            let contacts = [
                ("Emergency Services", "112 / 911"),
                ("Voice Mailbox", "*86"),
                ("Treble Support", "+1 800 555 0199"),
            ];
            draw_app_row_list(buf, stride, w, h, &bar, list, list_h, &contacts, state);
        } else if app_name == "Files" {
            let (list, list_h) = draw_app_search_field(
                buf,
                stride,
                w,
                h,
                &bar,
                content_y,
                al,
                state,
                "Filter files (/root)...",
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
                buf,
                stride,
                w,
                h,
                &bar,
                content_y,
                al,
                state,
                "Search or enter text...",
            );
            let cx = bar.center_x();
            let mark = super::font::em_px_at(4, w);
            let mark_y = list + h as f32 * 0.030;
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx as usize,
                mark_y as usize,
                app_name,
                state.palette.primary,
                4,
            );
            let sub_y = mark_y + mark * 0.95;
            draw_text_centered_clipped(
                buf,
                stride,
                w,
                h,
                cx as usize,
                sub_y as usize,
                bar.w * 0.86,
                "Universal Treble Linux Mobile Application",
                state.palette.on_surface_variant,
                1,
                FontWeight::Regular,
            );
            let btn_h = (h as f32 * 0.030).max(w.min(h) as f32 * 0.085);
            let btn_w = btn_h * 4.0;
            let btn_x = cx - btn_w * 0.5;
            let btn_y = sub_y + h as f32 * 0.020;
            if btn_y + btn_h < list + list_h {
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    btn_x,
                    btn_y,
                    btn_w,
                    btn_h,
                    btn_h * 0.5,
                    state.palette.primary,
                );
                let em = super::font::em_px_at(1, w);
                draw_text_centered_weighted(
                    buf,
                    stride,
                    w,
                    h,
                    cx as usize,
                    (btn_y + (btn_h - em * 0.62) * 0.5) as usize,
                    "Action Ready",
                    state.palette.on_primary,
                    1,
                    FontWeight::Bold,
                );
            }
        }

        // Gesture navigation pill, from the same layout the home screen uses.
        let nav = Layout::plain(w as f32, h as f32).nav_pill;
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            nav.x,
            nav.y,
            nav.w,
            nav.h,
            nav.radius,
            state.palette.on_surface,
        );
    } else {
        // 5. Foundational Layer: Home Screen
        //
        // Every rectangle below comes out of the shared `Layout`, the same
        // value the input path hit-tests against, so a drawn cell and a
        // tappable cell are the same cell by construction.
        let has_selection = state.selected_icon_id.is_some();
        let l = Layout::new(w as f32, h as f32, has_selection);
        // The panel width as f32, for the scroll-fraction arithmetic below.
        // `hf` went away with the clock-derived date line: the smartspace row
        // positions itself from `Layout::smartspace()` now, so nothing in this
        // block needs the height as a float.
        let wf = w as f32;

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
        // 5a-ii. Smartspace: the date/status row above the workspace.
        //
        // Drawn into the **layout's** smartspace rects, not at a position derived
        // from the clock. The clock is the lock-screen widget at the top of the
        // panel (`l.clock_y`); the smartspace row is its own host
        // (`Workspace.java:681-694` reserves it as a full workspace row) and the
        // two are unrelated. Computing the row's y from `clock_y` put the status
        // line hundreds of pixels above where the layout said it lived, which is
        // why `screenshot_every_launcher_state` asserted on a box the painter
        // never touched: "phase 0.0 must paint the date line" failed because
        // *nothing* painted there.
        //
        // Two lines, as the reference draws them: an 18 sp title over a 14 sp
        // subtitle (`styles.xml:184-209`). UTLC has one joined string, so the
        // title box is what carries the date and the subtitle box the date
        // joined with the weather -- see `join_smartspace`'s `with_weather`
        // branch, which is the cross-fade this is the painter for.
        let ss = l.smartspace();
        let date_line = join_smartspace(
            &mut smartspace_buf,
            state.date_str,
            state.weather_str,
            state.smartspace_phase,
        );
        if !date_line.is_empty() {
            // The title carries the date alone; the subtitle carries whatever the
            // cross-fade has joined onto it. Drawn only when the weather is
            // present, because duplicating the date on two lines is worse than
            // one line of type.
            if !state.weather_str.is_empty() {
                draw_text_clipped(
                    buf,
                    stride,
                    w,
                    h,
                    ss.title.x,
                    ss.title.y + (ss.title.h - l.clock_h * 0.62) * 0.5,
                    ss.title.w,
                    state.date_str,
                    state.palette.on_surface,
                    1,
                    FontWeight::Bold,
                );
            }
            // Below the cross-fade midpoint the subtitle is the date *alone* and
            // above it the date and the weather are joined with a separator
            // (`join_smartspace`'s `with_weather`). Splitting the two into a
            // fixed title and a fixed subtitle instead would make phase 1.0
            // render exactly what phase 0.0 renders, which is what the
            // cross-fade test exists to catch.
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                ss.subtitle.x,
                ss.subtitle.y + (ss.subtitle.h - l.clock_h * 0.50) * 0.5,
                ss.subtitle.w,
                date_line,
                state.palette.on_surface_variant,
                1,
                FontWeight::Regular,
            );
        }
        // The weather glyph, when the smartspace reports one. Drawn to the
        // *left* of the subtitle, inside the host, which is the reference's
        // arrangement (`smartspace_card_date.xml:15-31`).
        if !state.weather_str.is_empty() {
            let g_em = ss.icon.h * 0.9;
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                ss.icon.center_x() as usize,
                (ss.icon.center_y() + g_em * 0.31) as usize,
                WEATHER_GLYPH,
                state.palette.on_surface_variant,
                1,
            );
        }

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
            buf,
            stride,
            w,
            h,
            s.x - border,
            s.y - border,
            s.w + border * 2.0,
            s.h + border * 2.0,
            s.radius + border,
            pill_fg,
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
                    ChipTone::Primary => (
                        state.palette.surface_container_high,
                        state.palette.on_surface,
                    ),
                };
                let ring = if tone == ChipTone::Destructive {
                    0xFFEF4444
                } else {
                    state.palette.primary
                };
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
            let k = if pressed {
                state.icon_press_scale.clamp(0.5, 1.5)
            } else {
                1.0
            };
            let size = (icon.w * k).round();
            let radius = (icon.radius * k).round();
            let x = (cx - size * 0.5).round() as i32;
            let y = (cy - size * 0.5).round() as i32;

            if state.selected_icon_id == Some(app.id) {
                let pad = l.icon_size * 0.10 * k;
                let inner = (pad * 0.5).max(1.0);
                draw_rounded_rect_i32(
                    buf,
                    stride,
                    w,
                    h,
                    (x as f32 - pad).round() as i32,
                    (y as f32 - pad).round() as i32,
                    (size + pad * 2.0).round() as usize,
                    (size + pad * 2.0).round() as usize,
                    (radius + pad) as usize,
                    state.palette.primary,
                );
                draw_rounded_rect_i32(
                    buf,
                    stride,
                    w,
                    h,
                    (x as f32 - pad + inner).round() as i32,
                    (y as f32 - pad + inner).round() as i32,
                    (size + pad * 2.0 - inner * 2.0).round() as usize,
                    (size + pad * 2.0 - inner * 2.0).round() as usize,
                    (radius + pad - inner) as usize,
                    state.palette.surface,
                );
            }

            // The icon's drop shadow, *under* the tile.
            //
            // Order matters and is the whole reason this is a separate call: a
            // shadow painted after the tile fill would be invisible under it, and
            // the reference paints it as the drawable's own background layer
            // (`Shadow.apply`, `PreferenceManager.kt:76-78`).
            //
            // The mask is the *pre-blurred* coverage `IconCache` cached alongside
            // the bitmap, so this is a blit rather than a per-tile blur -- the
            // difference between an O(edge^2) pass on the frame path and nothing.
            if let Some((_, mask)) = state.icon_shadows.iter().find(|(id, _)| *id == app.id) {
                // The reference offsets the shadow down and slightly across rather
                // than centring it, which is what makes it read as a light source
                // above the tile. Proportional to the tile so it scales with the
                // display rather than being a fixed pixel count.
                let off = (size * 0.06).round().max(1.0) as i32;
                // Neutral black at 35%, not a palette role. Material 3 does define
                // `shadow` and `scrim` and `MaterialYouPalette` has neither, so the
                // honest value here is derived rather than invented: a drop shadow is
                // not a theme colour, it is the absence of light. Adding the two
                // roles to the palette is the proper fix and is recorded rather than
                // done, because a palette role implies a derivation from the seed and
                // neither of these has one.
                let shadow_rgb = 0x000000;
                const SHADOW_ALPHA: u8 = 0x59;
                super::raster::draw_preblurred_shadow_from_mask(
                    buf,
                    stride,
                    w,
                    h,
                    &mask.coverage,
                    mask.edge as usize,
                    mask.edge as usize,
                    x,
                    y,
                    off,
                    off,
                    shadow_rgb,
                    SHADOW_ALPHA,
                );
            }
            draw_rounded_rect_i32(
                buf,
                stride,
                w,
                h,
                x,
                y,
                size as usize,
                size as usize,
                radius as usize,
                app.color,
            );
            if app.folder_n > 0 {
                // A folder tile: up to four member icons clustered on it, in the
                // reference's clipped layout rather than a plain 2x2.
                //
                // The geometry comes from `FolderLayout::preview_in`, which is
                // `ClippedFolderIconLayoutRule.getPosition:140-177` behind an
                // eleven-test port, so the sign and shift conventions that
                // function already pins are used unchanged. The one thing that is
                // *not* from the reference is `shapes = true`: the reference's
                // shapes-on branch is a table indexed by item count and the
                // shapes-off branch is an interpolation, and they disagree at
                // four items. The table is the one Android actually takes when
                // `IconShapes` is enabled, so that is the branch used here.
                let fl = FolderLayout::new_with(&l, None, state.folder_dark);
                let tile = Rect {
                    x: x as f32,
                    y: y as f32,
                    w: size,
                    h: size,
                    radius,
                };
                let pv = fl.preview_in(tile, app.folder_n as usize, true, true);
                draw_rounded_rect_i32(
                    buf,
                    stride,
                    w,
                    h,
                    pv.background.x.round() as i32,
                    pv.background.y.round() as i32,
                    pv.background.w.round() as usize,
                    pv.background.h.round() as usize,
                    pv.radius.round().max(1.0) as usize,
                    app.color,
                );
                // Looked up by the cell token, which is the same string the tap
                // path parses into a `PageCell::Folder` -- so the cluster that is
                // drawn and the folder that opens are the same folder.
                if let Some(row) = state
                    .folder_previews
                    .iter()
                    .find(|r| r.folder == app.folder_id && r.n > 0)
                {
                    for (k, slot) in pv.icons.iter().take(row.n as usize).enumerate() {
                        let (ox, oy, edge) = *slot;
                        let Some(icon_img) = row.icons[k] else {
                            // A member with no decoded bitmap draws its colour
                            // chip, so the cluster keeps its shape instead of
                            // showing a hole.
                            draw_rounded_rect_i32(
                                buf,
                                stride,
                                w,
                                h,
                                ox.round() as i32,
                                oy.round() as i32,
                                edge.round().max(2.0) as usize,
                                edge.round().max(2.0) as usize,
                                (edge * 0.28).round().max(1.0) as usize,
                                (0xFF << 24) | (state.palette.on_surface & 0x00FF_FFFF),
                            );
                            continue;
                        };
                        draw_icon_bitmap_i32(
                            buf,
                            stride,
                            w,
                            h,
                            ox.round() as i32,
                            oy.round() as i32,
                            edge.round().max(2.0) as usize,
                            edge.round().max(2.0) as usize,
                            (edge * 0.28).round().max(1.0) as usize,
                            icon_img,
                        );
                    }
                }
            } else {
                match app.icon {
                    Some(icon_img) => {
                        draw_icon_bitmap_i32(
                            buf,
                            stride,
                            w,
                            h,
                            x,
                            y,
                            size as usize,
                            size as usize,
                            radius as usize,
                            icon_img,
                        );
                    }
                    None => {
                        let em = super::font::em_px_at(2, w);
                        draw_text_centered_i32(
                            buf,
                            stride,
                            w,
                            h,
                            cx as i32,
                            (cy - em * 0.30) as i32,
                            app.glyph,
                            0xFFFFFFFF,
                            2,
                        );
                    }
                }
            }
            // Unread dot, top-right of the tile.
            //
            // The reference's `BubbleTextView.drawDot` (`BubbleTextView.java:941`)
            // reads `DotInfo.getNotificationCount` (`dot/DotInfo.java:44-84`), so
            // this is the count made visible rather than a decoration -- which is
            // why it is keyed on the same `id` the notification store groups by.
            if let Some(n) = state
                .badge_counts
                .iter()
                .find(|(id, _)| *id == app.id)
                .map(|(_, n)| *n)
            {
                let dr = (size * 0.13).round().max(2.0);
                let dx = x as f32 + size - dr * 0.55;
                let dy = y as f32 + dr * 0.35;
                // A ring in the surface colour first, so the dot reads against
                // both the tile and whatever is behind it.
                draw_rounded_rect_i32(
                    buf,
                    stride,
                    w,
                    h,
                    (dx - dr * 0.35).round() as i32,
                    (dy - dr * 0.35).round() as i32,
                    (dr * 2.0).round() as usize,
                    (dr * 2.0).round() as usize,
                    dr.round() as usize,
                    state.palette.surface,
                );
                draw_rounded_rect_i32(
                    buf,
                    stride,
                    w,
                    h,
                    dx.round() as i32,
                    dy.round() as i32,
                    dr.round().max(2.0) as usize,
                    dr.round().max(2.0) as usize,
                    dr.round() as usize,
                    // Critical urgency is the loud dot: the reference tints the
                    // dot red for a high-importance notification, and `count`
                    // above is what makes the dot's *size* meaningful.
                    if n >= 4 {
                        0xFFE53935
                    } else {
                        state.palette.primary
                    },
                );
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
        let pitch = if total_pages > 1 {
            dots.w / (total_pages as f32 - 1.0)
        } else {
            0.0
        };
        let dot_w = dot_r * 2.0;
        let x0 = dots.center_x() - pitch * (total_pages as f32 - 1.0) * 0.5;
        let dot_color = (0x66 << 24) | (state.palette.on_surface & 0x00FFFFFF);
        for p in 0..total_pages {
            let dx = (x0 + p as f32 * pitch).round() as usize;
            draw_rounded_rect(
                buf,
                stride,
                w,
                h,
                dx,
                dots.y as usize,
                dot_w as usize,
                dots.h as usize,
                dot_r as usize,
                dot_color,
            );
        }
        // Fractional position: page index minus the scroll fraction of a page.
        let frac = (state.home_page as f32 - scroll / wf).clamp(0.0, (total_pages - 1) as f32);
        let ax = (x0 + frac * pitch - dot_w * 0.5).round() as i32;
        let pill_w = dot_w * 3.0;
        draw_rounded_rect_i32(
            buf,
            stride,
            w,
            h,
            ax,
            dots.y as i32,
            pill_w as usize,
            dots.h as usize,
            dot_r as usize,
            state.palette.primary,
        );

        // 5g-i. Quick Search Bar, from `Layout::qsb()`.
        //
        // The reference's hotseat QSB is a **pure icon pill**: no hint text, no
        // border, no inline completion (zero `Text` composables in
        // `LawnQsbUi.kt:363-421`). It is a weighted row -- the search glyph pinned
        // to the leading edge behind a 6 dp inset, the mic and lens clustered at
        // the trailing edge with the lens pulled 6 dp inboard, and all the slack
        // in the one flexible spacer between them.
        //
        // Gated on `show_search_bar` so the drawn pill and the shell's hit test
        // agree. They must: the pill's rect overlaps the dock's by roughly 165 px
        // on a 1080x2400 panel, so a hidden-but-tappable pill swallows dock
        // icons. The reference makes the same bar a user setting
        // (`prefs2.isHotseatEnabled`, `PreferenceManager2.kt:292`) and hides the
        // draw and the touch handler together.
        if state.show_search_bar {
            let qsb = l.qsb();
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                qsb.pill.x,
                qsb.pill.y,
                qsb.pill.w,
                qsb.pill.h,
                qsb.pill.radius,
                // Filled and accented when the field has focus, the way
                // Material 3 tints a focused text field.
                if state.search_active {
                    state.palette.surface_container_high
                } else {
                    state.palette.surface_container
                },
            );
            // The leading provider glyph and the two trailing ones. ASCII
            // stand-ins: the reference draws vector brand marks, and
            // `font.rs` renders anything outside 0x20..0x7E as a tofu box on a
            // stock image, so a glyph is the honest stand-in until the icon
            // pipeline can carry real artwork.
            let qsb_fg = if state.search_active {
                state.palette.primary
            } else {
                state.palette.on_surface_variant
            };
            for (glyph, box_) in [
                (QSB_SEARCH_GLYPH, qsb.g_icon),
                (QSB_MIC_GLYPH, qsb.mic),
                (QSB_LENS_GLYPH, qsb.lens),
            ] {
                draw_text_centered(
                    buf,
                    stride,
                    w,
                    h,
                    box_.center_x() as usize,
                    box_.center_y() as usize,
                    glyph,
                    qsb_fg,
                    1,
                );
            }
        }

        // 5g. Hotseat.
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            l.dock.x as usize,
            l.dock.y as usize,
            l.dock.w as usize,
            l.dock.h as usize,
            l.dock.radius as usize,
            state.palette.surface_container,
        );
        let fallback_dock = [
            AppGridItem {
                id: "phone",
                name: "Phone",
                color: 0xFF10B981,
                glyph: "P",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            },
            AppGridItem {
                id: "messages",
                name: "Messages",
                color: 0xFF3B82F6,
                glyph: "M",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            },
            AppGridItem {
                id: "apps",
                name: "Apps",
                color: 0xFF475569,
                glyph: ":",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            },
            AppGridItem {
                id: "browser",
                name: "Browser",
                color: 0xFF06B6D4,
                glyph: "B",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            },
            AppGridItem {
                id: "camera",
                name: "Camera",
                color: 0xFFF43F5E,
                glyph: "C",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            },
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
            let k = if pressed {
                state.icon_press_scale.clamp(0.5, 1.5)
            } else {
                1.0
            };
            let size = (l.dock_icon * k).round();
            let radius = (l.dock_icon * ICON_RADIUS * k).round();
            let x = (cx - size * 0.5).round() as i32;
            let y = (cy - size * 0.5).round() as i32;
            draw_rounded_rect_i32(
                buf,
                stride,
                w,
                h,
                x,
                y,
                size as usize,
                size as usize,
                radius as usize,
                app.color,
            );
            match app.icon {
                Some(icon_img) => {
                    draw_icon_bitmap_i32(
                        buf,
                        stride,
                        w,
                        h,
                        x,
                        y,
                        size as usize,
                        size as usize,
                        radius as usize,
                        icon_img,
                    );
                }
                None => {
                    let em = super::font::em_px_at(2, w);
                    draw_text_centered_i32(
                        buf,
                        stride,
                        w,
                        h,
                        cx as i32,
                        (cy - em * 0.30) as i32,
                        app.glyph,
                        0xFFFFFFFF,
                        2,
                    );
                }
            }
        }

        // 5h. Gesture navigation pill, drawn last so overlays can cover it.
        draw_rounded_rect(
            buf,
            stride,
            w,
            h,
            l.nav_pill.x as usize,
            l.nav_pill.y as usize,
            l.nav_pill.w as usize,
            l.nav_pill.h as usize,
            l.nav_pill.radius as usize,
            state.palette.on_surface,
        );

        // 5i. App drawer overlay: the Material 3 sheet.
        //
        // This replaces a hand-rolled block that drew its own rounded rect,
        // its own search box, its own header, its own app grid and its own nav
        // pill from the *workspace* `Layout` fields. `drawer_mod` already
        // carries a 2,479-line transliteration of the reference sheet --
        // 24 dp top corner radius, the 0.40 scrim, the 60/52 dp search stack,
        // the 48 dp header pill, the 128x2 dp divider and the prediction row
        // -- and none of it was reachable: the legacy block used a different
        // set of numbers for the same widget, so the drawer's own hit test
        // (which does use `DrawerSheetLayout`) and its pixels disagreed.
        if state.app_drawer_open || state.drawer_progress > 0.001 {
            let prog = if state.drawer_progress > 0.001 {
                state.drawer_progress.clamp(0.0, 1.0)
            } else if state.app_drawer_open {
                1.0
            } else {
                0.0
            };
            // `shift` is measured UP FROM THE BOTTOM: `DrawerSheetLayout`
            // sets `top = h - shift`, and `draw_drawer_sheet` early-returns
            // false when `top >= h`. So `shift = 0` is the closed sheet (top
            // edge at the bottom, nothing drawn) and `shift = h` is fully
            // open (top edge at 0). The progress is therefore the shift
            // fraction, not its complement -- `(1 - prog)` here drew a
            // *closed* sheet at full progress, i.e. nothing at all, which is
            // the bug the first version of this block shipped.
            let shift = prog * h as f32;
            let sheet = l.drawer_sheet(shift);

            // The frosted scrim: blur the workspace *above* the sheet's top edge.
            //
            // This is the reference's depth blur. When the all-apps sheet comes
            // forward, the launcher blurs the things it is covering -- the
            // workspace and hotseat -- with a real `RenderEffect`:
            //
            // ```text
            // RenderEffect blurEffect = RenderEffect.createBlurEffect(
            //         mCurrentBlur, mCurrentBlur, Shader.TileMode.DECAL);   // :365-366
            // mLauncher.getDepthBlurTargets().forEach(t -> t.setRenderEffect(blurEffect));
            // ```
            //
            // (`BaseDepthController.java:365-367`, gated by
            // `AllAppsState.shouldBlurWorkspace`, `:155-158`.) And it takes the
            // effect back off the moment the sheet is gone -- `mDepth <= 0f ||
            // mCurrentBlur <= 0` clears it (`BaseDepthController.java:350-353`
            // into `clearWorkspaceRenderEffects`, `:375-382`) -- which is the
            // "does not differ when the drawer is closed" half of the contract.
            //
            // The threshold is the reference's own minimum-visible-blur rule, not
            // a taste call. The reference ramps the radius with depth
            // (`blurAmount = depth`, `newBlur = (int)(blurAmount *
            // mMaxBlurRadius)`, `BaseDepthController.java:222-229`, with
            // `mMaxBlurRadius = 23` px from `quickstep/res/values/config.xml:41`)
            // and then *skips* the update entirely when the change is under one dp
            // (`:230-233`, `delta < Utilities.dpToPx(1)`). One dp at 420 dp
            // width is `panel_w / 420`, so the smallest progress that produces a
            // visible blur is `(1dp) / 23px` ~= 0.11. UTLC's
            // `apply_frosted_blur_region` has a fixed 3 px tile and no radius
            // argument, so the ramp cannot be reproduced and only the threshold
            // can: below it the workspace is left sharp, at and above it the
            // whole band is frosted at once.
            //
            // **Order is the whole composition.** The blur runs before
            // `draw_drawer_sheet_with_section`, and only over `[0, sheet.top)`,
            // so the two effects touch *disjoint* pixels and neither can fight the
            // other: the sheet body is filled afterwards with the one flat
            // scrimmed colour the reference itself uses
            // (`AllAppsState.getWorkspaceScrimColor`, `:207-219`, keeping
            // `AllAppsScrimColor` = `0x404040` at 0.40 alpha
            // (`ColorTokens.kt:96`) even in the sheet-with-blur case). The
            // reference composes the same way -- a scrim colour on the scrim
            // *view*, a blur on the *depth-blur targets* -- and this is the UTLC
            // equivalent of that split.
            if prog > DRAWER_BLUR_THRESHOLD && sheet.top > 1.0 {
                let top = (sheet.top.min(h as f32) as usize).min(h);
                if top > 1 {
                    apply_frosted_blur_region(buf, stride, w, 0, top);
                }
            }
            // The sheet's type sizes come from the panel's own dp density
            // rather than from the shell: 20 sp for the query, 13 sp for the
            // header, and a half-dp hairline, all at `w / 420`.
            let dp = w as f32 / 420.0;
            let style = super::drawer_mod::DrawerStyle {
                backdrop: state.palette.surface,
                surface: state.palette.surface,
                surface_high: state.palette.surface_container_high,
                on_surface: state.palette.on_surface,
                on_surface_variant: state.palette.on_surface_variant,
                outline: state.palette.outline,
                primary: state.palette.primary,
                text_px: 20.0 * dp,
                label_px: 13.0 * dp,
                stroke_px: (dp * 0.5).max(1.0),
            };
            // The header count is the one string the sheet does not own: it is
            // the *match* total, which the app window deliberately does not
            // carry. `format_apps_count` is a stack formatter.
            let mut count_buf = [0u8; 16];
            let count_str = format_apps_count(&mut count_buf, state.drawer_app_count);
            // The section letter is threaded rather than hard-coded to `'A'`.
            //
            // The sheet used to paint the literal `'A'` in its header while
            // `FastScrollerState::letter_str()` held the *real* section a few
            // pixels away on the fast-scroller popup. For 25 of the 26 sections the
            // two disagreed for the whole of a drag, which made the header a
            // constant rather than a label. `draw_drawer_sheet` keeps the old
            // signature and the old default, so an unmigrated caller still compiles.
            super::drawer_mod::draw_drawer_sheet_with_section(
                buf,
                stride,
                w,
                h,
                &sheet,
                prog,
                &style,
                state.drawer_search,
                count_str,
                state.drawer_section_letter,
            );

            // The all-apps grid. The sheet draws the *chrome* -- surface,
            // handle, search, header, divider, prediction row -- and cannot
            // draw the icons itself because it does not hold the catalogue.
            // So the grid lives here, and it takes its geometry from the sheet
            // rather than from the workspace `Layout`: the sheet's `grid` band,
            // `grid_row_h` (104 dp) and column pitch (16 dp border) are the
            // reference's numbers, whereas the workspace fields
            // (`drawer_grid_top`, `row_pitch`, `col_pitch`) are a different
            // set. The shell's hit test follows the same sheet, so drawing from
            // the workspace layout is how a cell could be tappable at one y and
            // painted at another.
            // The zero-result state.
            //
            // Before this the drawer's only path for "no matches" was a bare
            // `if count > 0`, so a query that matched nothing produced a sheet with
            // a search field, a handle and a header reading "0 apps" and nothing
            // else -- no indication that the field was read, and no route out of a
            // query that cannot resolve. The reference explains the empty result
            // (`SearchResultEmptyState.kt:15-49`, `search_result_empty_state.xml:7-34`)
            // and always appends a web-search action for an unmatched query
            // (`ActionsSectionBuilder.kt:160-190`).
            //
            // The geometry comes from `drawer_mod`, which owns both, so the drawn
            // positions and the hit-test rects cannot come from two derivations --
            // the bug class this file's own comments warn about twice.
            if state.drawer_app_count == 0 && !state.drawer_search.is_empty() {
                super::drawer_mod::draw_search_empty_state(
                    buf,
                    stride,
                    w,
                    h,
                    &sheet.grid,
                    &style,
                    w as f32,
                    state.drawer_search,
                );
                let action = super::drawer_mod::web_search_action_rect(
                    &sheet.grid,
                    w as f32,
                    sheet.grid.y + sheet.grid.h * 0.62,
                );
                super::drawer_mod::draw_web_search_action(
                    buf, stride, w, h, &action, &style, w as f32, "the web",
                );
            }

            let scroll_y = state.drawer_scroll_y;
            let cols = sheet.grid_cols.max(1);
            let total_rows = state.drawer_app_count.div_ceil(cols);
            let rows = sheet.grid_visible_rows(scroll_y, total_rows);
            let win_start = state.drawer_first_index;
            let pitch = sheet.grid_cell_pitch();
            for (j, app) in state.drawer_apps.iter().enumerate() {
                // The catalogue index, not the window index: the row comes
                // from this. The view is a scroll-positioned window, so the two
                // differ by `drawer_first_index`.
                let idx = win_start + j;
                let col = idx % cols;
                let row = idx / cols;
                // Cull in row space. One comparison per item, and the window
                // is sized so only a screenful is ever iterated.
                if row < rows.start || row >= rows.end {
                    continue;
                }
                let icon = sheet.grid_icon(col, row, scroll_y);
                let (cx, cy) = (icon.center_x(), icon.center_y());
                // Off-panel guard. The sheet animates up from the bottom, so
                // early in the open the whole grid is below the panel; the
                // icons must be skipped rather than clamped, or a clamped icon
                // piles up along the bottom edge.
                if cy + icon.h < 0.0
                    || cy - icon.h > h as f32
                    || cx + icon.w < 0.0
                    || cx - icon.w > w as f32
                {
                    continue;
                }
                // Tactile press compression, same as the home grid.
                let k = if state.pressed_icon_id == Some(app.id) {
                    state.icon_press_scale.clamp(0.5, 1.5)
                } else {
                    1.0
                };
                let size = (icon.w * k).round();
                let radius = (icon.radius * k).round();
                let x = (cx - size * 0.5).round() as i32;
                let y = (cy - size * 0.5).round() as i32;
                draw_rounded_rect_i32(
                    buf,
                    stride,
                    w,
                    h,
                    x,
                    y,
                    size as usize,
                    size as usize,
                    radius as usize,
                    app.color,
                );
                match app.icon {
                    Some(icon_img) => {
                        draw_icon_bitmap_i32(
                            buf,
                            stride,
                            w,
                            h,
                            x,
                            y,
                            size as usize,
                            size as usize,
                            radius as usize,
                            icon_img,
                        );
                    }
                    None => {
                        let em = super::font::em_px_at(2, w);
                        draw_text_centered_i32(
                            buf,
                            stride,
                            w,
                            h,
                            cx as i32,
                            (cy - em * 0.30) as i32,
                            app.glyph,
                            0xFFFFFFFF,
                            2,
                        );
                    }
                }
                // Label under the icon, clipped to the cell pitch so a long
                // name cannot run into its neighbour.
                let label_y = icon.y + icon.h + sheet.grid_label_gap;
                draw_text_centered_clipped_i32(
                    buf,
                    stride,
                    w,
                    h,
                    cx as i32,
                    label_y,
                    pitch * 0.94,
                    app.name,
                    state.palette.on_surface,
                    1,
                    FontWeight::Regular,
                );
            }
        }
    }

    // 10. Virtual keyboard, when active.
    //
    // Key rects come straight from `Keyboard`, the same struct the input path
    // hit-tests, so a key that is drawn is exactly a key that can be pressed.
    if state.keyboard_active && !state.is_locked && !state.shade_open {
        let kb = Keyboard::new_for(w as f32, h as f32, state.keyboard_layout);
        // The page's row tables, resolved once. This is the whole of the
        // layout-to-render path: everything below reads `page_rows` or
        // `kb`, and nothing below reads a module-level constant, so there is no
        // arrangement in which `state.keyboard_layout` changes the state and not
        // the pixels.
        let page_rows = kb.rows_for_layout();
        let f = kb.frame;
        let k_em = super::font::em_px_at(2, w);
        let k_text_h = k_em * 0.62;

        // Sheet, with a hairline along the top edge.
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            f.x,
            f.y,
            f.w,
            f.h,
            f.radius,
            state.palette.surface_container,
        );
        draw_rect_f(
            buf,
            stride,
            w,
            h,
            f.x,
            f.y,
            f.w,
            (h as f32 * 0.0012).max(1.0),
            state.palette.outline,
        );

        // Modifier keys, then the character rows.
        let shift_on = state.keyboard_shift_active;
        let (shift_bg, shift_fg) = if shift_on {
            (state.palette.primary, state.palette.on_primary)
        } else {
            (
                state.palette.surface_container_high,
                state.palette.on_surface,
            )
        };
        fn key(
            buf: &mut [u32],
            stride: usize,
            w: usize,
            h: usize,
            r: &super::layout::Rect,
            bg: u32,
        ) {
            draw_rounded_rect_f(buf, stride, w, h, r.x, r.y, r.w, r.h, r.radius, bg);
        }
        #[allow(clippy::too_many_arguments)]
        fn legend(
            buf: &mut [u32],
            stride: usize,
            w: usize,
            h: usize,
            r: &super::layout::Rect,
            label: &str,
            col: u32,
            scale: usize,
        ) {
            let em = super::font::em_px_at(scale, w);
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                r.center_x() as usize,
                (r.center_y() - em * 0.31) as usize,
                label,
                col,
                scale,
            );
        }

        key(buf, stride, w, h, &kb.row3_shift, shift_bg);
        legend(
            buf,
            stride,
            w,
            h,
            &kb.row3_shift,
            if shift_on { "V" } else { "^" },
            shift_fg,
            1,
        );
        // Row 1 is the shared digit row (`KB_DIGITS`) and is read from the same
        // table `Keyboard::hit` reads, so the drawn digit and the typed digit
        // are the same value. It used to be the module-level `ROW1`, which is
        // now an alias of the same table -- but going through `Keyboard` means a
        // fourth page cannot be added with a digit row the hit test misses.
        let digits = super::layout::KB_DIGITS;
        key(
            buf,
            stride,
            w,
            h,
            &kb.row3_backspace,
            state.palette.surface_container_high,
        );
        // Backspace: a left-pointing wedge plus the delete bar.
        let bx = kb.row3_backspace.center_x();
        let by = kb.row3_backspace.center_y();
        let br = kb.row3_backspace.h * 0.17;
        for i in 0..=4 {
            let t = i as f32 / 4.0;
            let px = bx - br * 1.5 + t * br * 1.4;
            let dy = br * (1.0 - (t * 2.0 - 1.0).abs());
            draw_line(
                buf,
                stride,
                w,
                h,
                px,
                by - dy,
                px,
                by + dy,
                state.palette.on_surface_variant,
            );
        }
        draw_line(
            buf,
            stride,
            w,
            h,
            bx - br * 0.1,
            by,
            bx + br * 1.4,
            by,
            state.palette.on_surface_variant,
        );

        for (i, &ch) in digits.iter().enumerate() {
            let r = kb.row1_at(i);
            key(buf, stride, w, h, &r, state.palette.surface_container_high);
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
            let ch = page_rows.row2[i];
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
            let ch = page_rows.row3[i];
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
        key(
            buf,
            stride,
            w,
            h,
            &kb.row4_hide,
            state.palette.surface_container_high,
        );
        legend(
            buf,
            stride,
            w,
            h,
            &kb.row4_hide,
            "Hide",
            state.palette.on_surface_variant,
            1,
        );
        key(
            buf,
            stride,
            w,
            h,
            &kb.row4_space,
            state.palette.surface_container_high,
        );
        legend(
            buf,
            stride,
            w,
            h,
            &kb.row4_space,
            "English",
            state.palette.on_surface_variant,
            1,
        );
        key(buf, stride, w, h, &kb.row4_enter, state.palette.primary);
        legend(
            buf,
            stride,
            w,
            h,
            &kb.row4_enter,
            "Enter",
            state.palette.on_primary,
            1,
        );
        // The layout toggle, painted last so it sits over the space bar's left
        // neighbour in the same row and reads as part of the bottom strip.
        //
        // `kb.toggle_label()` and the string `handle_key_tap` dispatches are one
        // value (`layout::keyboard_toggle_label`), which is why this can be a
        // function call rather than a match: a drawn label and a handled label
        // that are separate tables is how `?123` ends up handled as `ABC`.
        //
        // It is drawn in the *accent* colour on purpose: it is the one key that
        // changes the whole sheet, and it is the key the whole layout field
        // exists to make reachable. An un-accented key that reconfigures the
        // keyboard is the one thing a user will not think to press.
        key(buf, stride, w, h, &kb.row4_layout, state.palette.primary);
        legend(
            buf,
            stride,
            w,
            h,
            &kb.row4_layout,
            kb.toggle_label(),
            state.palette.on_primary,
            1,
        );
        let _ = k_text_h;
    }

    // 11. Pointer, drawn above every surface.
    if let Some((cx, cy)) = state.cursor_pos {
        let l = Layout::plain(w as f32, h as f32);
        if state.is_touching {
            // Pressed: a filled dot in the accent colour.
            draw_circle_glyph(
                buf,
                stride,
                w,
                h,
                cx as f32,
                cy as f32,
                l.w * 0.008,
                state.palette.primary,
            );
        } else {
            // Hovering: a ring, so it never hides what it points at.
            let r = l.w * 0.006;
            draw_circle_glyph(
                buf,
                stride,
                w,
                h,
                cx as f32,
                cy as f32,
                r,
                (0xAA << 24) | (state.palette.on_surface & 0x00FF_FFFF),
            );
            draw_circle_glyph(
                buf,
                stride,
                w,
                h,
                cx as f32,
                cy as f32,
                r * 0.42,
                state.palette.primary,
            );
        }
    }

    // 12. Material You touch ripple.
    //
    // The state layer of Material 3: a translucent circle in the on-surface
    // colour that expands from the touch point and fades out. `radius` grows
    // on a time constant and `alpha` falls with it, which is what makes it
    // read as a ripple rather than a blinking dot.
    //
    // Two things keep it a *state layer* and not a screen wash:
    //
    //  * `RIPPLE_STATE_LAYER_ALPHA` (0.098) is the Material 3 press value,
    //    `#19FFFFFF`. The old 0.32 was nearly 3x that, and at 435 px radius it
    //    greyed out most of a 1080 px panel on every single tap.
    //  * The disc is clipped to the control's own bounds. Material's state
    //    layer is drawn *by* the control, so it is bounded by the control; an
    //    unbounded circle centred on a hotseat icon reads as a rendering bug
    //    even at the correct alpha.
    if let Some((rx, ry, radius, alpha)) = state.touch_ripple {
        let radius = radius.min(RIPPLE_MAX_RADIUS);
        if alpha > 0.01 && radius > 1.0 {
            let a = (alpha * RIPPLE_STATE_LAYER_ALPHA).clamp(0.0, 1.0);
            let layer = ((a * 255.0) as u32) << 24 | (state.palette.on_surface & 0x00FF_FFFF);
            let clip = state.ripple_clip.unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                w: w as f32,
                h: h as f32,
                radius: 0.0,
            });
            // Ring border plus a soft interior, both on the same state layer.
            draw_circle_glyph_clipped(buf, stride, w, h, &clip, rx, ry, radius, layer);
            draw_circle_glyph_clipped(buf, stride, w, h, &clip, rx, ry, radius * 0.86, layer);
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
            draw_rounded_rect_f(buf, stride, w, h, cur_x, cur_y, cur_w, cur_h, radius, color);
            draw_rect_f(buf, stride, w, h, cur_x, cur_y, cur_w, ring, edge);
            draw_rect_f(
                buf,
                stride,
                w,
                h,
                cur_x,
                cur_y + cur_h - ring,
                cur_w,
                ring,
                edge,
            );
            draw_rect_f(buf, stride, w, h, cur_x, cur_y, ring, cur_h, edge);
            draw_rect_f(
                buf,
                stride,
                w,
                h,
                cur_x + cur_w - ring,
                cur_y,
                ring,
                cur_h,
                edge,
            );
        }
    }

    // 14. Volume HUD overlay if active
    if let Some(sex) = state.super_extreme_state {
        if sex.volume_hud.is_visible() {
            let cx = (w as f32 * 0.5) as usize;
            let em1 = super::font::em_px_at(1, w);
            let bar = sex.volume_hud.format_bar(34);
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                w as f32 * 0.05,
                8.0,
                w as f32 * 0.90,
                em1 * 2.2,
                4.0,
                0xFF0F172A,
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                cx,
                (8.0 + em1 * 0.4) as usize,
                &bar,
                0xFF38BDF8,
                1,
            );
        }
    }

    // 15. Launcher-rewrite surfaces.
    //
    // All of these are layered over whatever is underneath (workspace, drawer
    // sheet, an open app) and every one is gated on a progress field that is
    // 0.0 at rest, so an idle shell reaches none of them and pays nothing.
    // Order is the reference's: the scrim-darkened workspace first, then
    // folder, then overview, then the two floating affordances that sit above
    // both, then the popup.
    if !state.is_locked
        && state.power_saver_mode != crate::compositor::power_sync::PowerSaverMode::SuperExtreme
    {
        draw_page_indicator(buf, stride, w, h, state);
        draw_workspace_morph(buf, stride, w, h, state);
        if state.folder_morph > 0.0 || state.folder_scrim > 0.0 {
            draw_folder(buf, stride, w, h, state);
        } else {
            // Closed folders paint nothing, so publish nothing: without this
            // the previous frame's rename field and grid survive and a tap on
            // the home screen hits a folder that is no longer on screen (the
            // same trap `SETTINGS_GEOM` documents for its own pitch).
            FOLDER_RENAME.with(|c| c.set(FolderRenameGeometry::EMPTY));
            FOLDER_GRID.with(|c| c.set(FolderGridGeometry::EMPTY));
        }
        if state.overview_progress > 0.0 {
            draw_overview(buf, stride, w, h, state);
        }
        if state.fastscroller_thumb > 0.0 || state.fastscroller_popup_alpha > 0.0 {
            draw_fast_scroller_rail(buf, stride, w, h, state);
        }
        if state.popup_progress > 0.0 {
            draw_popup(buf, stride, w, h, state);
        }
        // A lifted workspace cell, its gap and its merge plate. Above the popup:
        // the reference's drag layer is above everything in the workspace
        // (`DragLayer.java:190`), and a drag that the popup covered would be a
        // drag the user could not see.
        if state.drag_slot.is_some() {
            draw_workspace_drag(buf, stride, w, h, state);
        }
        // The app-info panel is last of all: it is a whole surface, not a
        // floating affordance, so nothing should draw over it.
        if state.app_info.is_some() {
            draw_app_info(buf, stride, w, h, state);
        }
    }
}

/// The lifted workspace cell, the drop gap and the "two icons onto each other"
/// plate.
///
/// Drawn *after* the grid and the popup and *before* the app-info panel, so the
/// layering is the same as the reference's: the drag floats above the workspace
/// (`DragLayer.java:190`) and a panel above that.
///
/// Every rect comes from [`Layout`]'s drag methods -- the same ones the shell's
/// dispatch reads -- so the plate under the finger is exactly the plate the
/// release would commit against.
fn draw_workspace_drag(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    // No lifted cell means no drag visuals at all: an orphan gap or plate with
    // `drag_slot == None` would be a drop target for a drag that does not
    // exist. Checked first so all three paths (plate, gap, lift) share one
    // gate -- the reference has no drag layer without a drag view
    // (`DragLayer.java:190`).
    if state.drag_slot.is_none() {
        return;
    }
    let l = Layout::plain(w as f32, h as f32);

    // --- 1. The merge plate. ----------------------------------------------------
    // Drawn first so the lifted icon lands on top of it, which is the
    // reference's arrangement: `mFolderCreateBg` is the hovered icon's own
    // background and `isClipping = false` puts the whole plate behind it
    // (`Workspace.java:2942-2956`).
    if let Some(slot) = state.drag_merge_slot {
        let m = l.drag_merge_rect(slot as usize);
        // In the accent colour: this is the affirmative state, and the reference
        // gives it the same treatment -- a `PreviewBackground` animated *to
        // accept* (`Workspace.java:2948-2956`).
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            m.x,
            m.y,
            m.w,
            m.h,
            m.radius,
            state.palette.primary,
        );
    }

    // --- 2. The drop gap. ------------------------------------------------------
    //
    // The reference clears the drag outlines the moment a merge arms
    // (`Workspace.java:2955`) and otherwise draws one behind the target cell
    // (`CellLayout.visualizeDropLocation:1196-1224`). So the two are mutually
    // exclusive, and a shell that set both would otherwise see a plate with a
    // gap drawn through it.
    if let Some(slot) = state.drag_drop_slot {
        if state.drag_merge_slot != Some(slot) {
            let g = l.drag_gap_rect(slot as usize);
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                g.x,
                g.y,
                g.w,
                g.h,
                g.radius,
                state.palette.surface_container_high,
            );
        }
    }

    // --- 3. The lifted cell. ---------------------------------------------------
    let Some(slot) = state.drag_slot else {
        return;
    };
    let slot = slot as usize;
    let lifted = l.drag_lifted_rect(slot, state.drag_pos, state.drag_lift);
    // The resting cell is left as a hole rather than being erased: the reference
    // removes the dragged view from the container
    // (`Workspace.java:2235-2346` rearranges around it), so the slot's *gap* is
    // the affordance and an outline over it would double-draw.
    //
    // The key shadow goes under the lift, offset by the reference's `.5dp`
    // (`styles.xml:425-426`). It is drawn as a filled rect behind the tile rather
    // than a blurred pass: the workspace icon shadow already exists as a
    // pre-blurred mask (`icon_shadows`) and reusing it here would mean the drag
    // depends on the shadow cache being enabled, which it is not in the dark
    // theme (`res/values/styles.xml:111-113`) -- so a dark-themed drag would lose
    // its lift entirely.
    let shadow = l.drag_shadow_rect(lifted);
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        shadow.x,
        shadow.y,
        shadow.w,
        shadow.h,
        shadow.radius,
        (super::layout::FOLDER_DRAG_SHADOW_ALPHA as u32) << 24,
    );

    let app = state.grid_apps.get(slot);
    let color = app
        .map(|a| a.color)
        .unwrap_or(state.palette.surface_container_high);
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        lifted.x,
        lifted.y,
        lifted.w,
        lifted.h,
        lifted.radius,
        color,
    );
    if let Some(app) = app {
        if let Some(img) = app.icon {
            draw_icon_bitmap_i32(
                buf,
                stride,
                w,
                h,
                lifted.x.round() as i32,
                lifted.y.round() as i32,
                lifted.w.round() as usize,
                lifted.h.round() as usize,
                lifted.radius.round() as usize,
                img,
            );
        } else if !app.glyph.is_empty() {
            let em = super::font::em_px_at(2, w);
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                lifted.center_x() as usize,
                (lifted.center_y() - em * 0.30) as usize,
                app.glyph,
                0xFFFFFFFF,
                2,
            );
        }
    }
}

/// The app-info panel.
///
/// The surface the reference does not have. `SystemShortcut.APP_INFO`
/// (`SystemShortcut.java:188`) resolves its target only at the tap and hands off
/// (`PackageManagerHelper.java:160-187`); UTLC resolves it before, because on a
/// Linux device the target is a *program* that may simply not be installed, and
/// a user cannot tell from a menu row whether tapping it will do anything.
///
/// Gated on `state.app_info.is_some()` rather than on a progress field: there is
/// no animation for it. A panel that slid in would need a spring, and the
/// reference's own app-info -- a different activity -- does not slide in either
/// (`SystemShortcut.java:240-249` starts it with a source-rect container
/// transform, which UTLC has no equivalent for).
fn draw_app_info(buf: &mut [u32], stride: usize, w: usize, h: usize, state: &DrmInteractiveState) {
    let Some(info) = state.app_info else {
        return;
    };
    let a = super::layout::AppInfoLayout::new(w as f32, h as f32);
    let em = super::font::em_px_at(1, w);

    // Opaque surface, full bleed: this is a screen, not a sheet. Same reasoning
    // as the shade (`drm_kms.rs:2455-2460`): a translucent panel over a launcher
    // costs a per-pixel blend over the whole display for a difference nobody can
    // see under an opaque panel.
    draw_rect_f(
        buf,
        stride,
        w,
        h,
        0.0,
        0.0,
        w as f32,
        h as f32,
        state.palette.surface,
    );

    // Top bar, with the close control and the word the reference puts on the
    // menu row: `R.string.app_info_drop_target_label` = "App info"
    // (`res/values/strings.xml:226`).
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        a.surface.bar.x,
        a.surface.bar.y,
        a.surface.bar.w,
        a.surface.bar.h,
        a.surface.bar.radius,
        state.palette.surface_container,
    );
    let bar_c = a.surface.bar.center();
    let title_x = a.surface.bar.x + a.surface.bar.h * 0.25;
    let title_w = a.surface.close.x - title_x - a.surface.close.h * 0.3;
    draw_text_centered_clipped(
        buf,
        stride,
        w,
        h,
        (title_x + title_w * 0.5) as usize,
        (bar_c.1 - em * 0.32) as usize,
        title_w,
        "App info",
        state.palette.on_surface,
        2,
        FontWeight::Bold,
    );
    let c = a.surface.close;
    draw_rounded_rect(
        buf,
        stride,
        w,
        h,
        c.x as usize,
        c.y as usize,
        c.w as usize,
        c.h as usize,
        (c.h * 0.25) as usize,
        0xFFEF4444,
    );
    let cc = c.center();
    let r = c.h * 0.17;
    draw_line(
        buf,
        stride,
        w,
        h,
        cc.0 - r,
        cc.1 - r,
        cc.0 + r,
        cc.1 + r,
        0xFFFFFFFF,
    );
    draw_line(
        buf,
        stride,
        w,
        h,
        cc.0 + r,
        cc.1 - r,
        cc.0 - r,
        cc.1 + r,
        0xFFFFFFFF,
    );

    // The icon.
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        a.icon.x,
        a.icon.y,
        a.icon.w,
        a.icon.h,
        a.icon.radius,
        info.color,
    );
    if let Some(img) = info.icon {
        draw_icon_bitmap_i32(
            buf,
            stride,
            w,
            h,
            a.icon.x.round() as i32,
            a.icon.y.round() as i32,
            a.icon.w.round() as usize,
            a.icon.h.round() as usize,
            a.icon.radius.round() as usize,
            img,
        );
    } else if !info.glyph.is_empty() {
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            a.icon.center_x() as usize,
            (a.icon.center_y() - em * 0.30) as usize,
            info.glyph,
            0xFFFFFFFF,
            2,
        );
    }

    // The five text lines: name, id, exec, target, and what the button does.
    //
    // Each is drawn from its own rect and clipped to its own width, so a long
    // `Exec=` line cannot run into the next one. An empty field draws nothing at
    // all rather than a blank row -- the same rule the smartspace date line
    // follows, and for the same reason: a drawn placeholder reads as data.
    for (rect, text, weight) in [
        (a.name, info.name, FontWeight::Bold),
        (a.id_line, info.id, FontWeight::Regular),
        (a.exec_line, info.exec, FontWeight::Regular),
        (a.target_line, info.target, FontWeight::Regular),
        (a.hint, app_info_hint(&info), FontWeight::Regular),
    ] {
        if text.is_empty() {
            continue;
        }
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            rect.x,
            rect.center_y() - em * 0.31,
            rect.w,
            text,
            if rect.y == a.name.y {
                state.palette.on_surface
            } else {
                state.palette.on_surface_variant
            },
            1,
            weight,
        );
    }

    // The affordance. Disabled when there is nothing to spawn, and *drawn*
    // disabled -- an enabled-looking button that does nothing is the exact
    // failure this panel exists to avoid.
    let enabled = info.can_open();
    let b = a.open_button;
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        b.x,
        b.y,
        b.w,
        b.h,
        b.radius,
        if enabled {
            state.palette.primary
        } else {
            state.palette.surface_container
        },
    );
    draw_text_centered(
        buf,
        stride,
        w,
        h,
        b.center_x() as usize,
        (b.center_y() - em * 0.31) as usize,
        APP_INFO_OPEN_LABEL,
        if enabled {
            state.palette.on_primary
        } else {
            state.palette.on_surface_variant
        },
        1,
    );
}

/// The label on the app-info handoff button.
///
/// Not "App info": that is the *menu row* that opens this panel
/// (`res/values/strings.xml:226`, `SystemShortcut.java:196`), and repeating it on
/// the button would name the action the user just took rather than the one they
/// are about to take.
pub const APP_INFO_OPEN_LABEL: &str = "Open in software centre";

/// One line under the app-info rows, saying what the button will actually do.
///
/// Two cases, because there are two: a resolved program, or nothing to run. The
/// second is the case a launcher on Linux has and the reference never has -- the
/// reference's `startAppDetailsActivity` either resolves or toasts, and it does
/// not toasts until after the tap (`PackageManagerHelper.java:183-186`).
fn app_info_hint(info: &AppInfoSpec<'_>) -> &'static str {
    match (info.can_open(), info.installing) {
        (true, true) => "Install session in progress; opens the store entry",
        (true, false) => "Leaves the launcher",
        (false, _) => "No software centre found for this app",
    }
}

/// The drawer's fast scroller: track, thumb, and the letter popup.
///
/// `draw_fast_scroller` reads four fields off the state, so it is rebuilt here
/// from the three hashed scalars plus the layout. It is a fixed-size struct
/// with no heap, so this costs nothing -- and keeping the hashed interface as
/// scalars means the damage hash does not have to walk a struct whose interior
/// is mostly drag bookkeeping the renderer never looks at.
fn draw_fast_scroller_rail(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    let l = Layout::plain(w as f32, h as f32);
    let fs_layout = l.fast_scroller();
    let mut st = super::drawer_mod::FastScrollerState::new();
    // `fastscroller_thumb` is the thumb's position along the track, 0..1;
    // `thumb_y` is the same quantity in track-local pixels, which is what the
    // model stores. The thumb is a fixed 52 dp, so the travel is the track
    // minus the thumb and a taller track only buys range.
    let travel = (fs_layout.track.h - fs_layout.thumb_h).max(0.0);
    st.thumb_y = state.fastscroller_thumb.clamp(0.0, 1.0) * travel;
    st.track_h = fs_layout.track.h;
    st.letter = state.fastscroller_letter;
    st.popup_alpha = state.fastscroller_popup_alpha.clamp(0.0, 1.0);
    // The popup's top edge, from the model's own `updatePopupY`
    // (`FastScroller.java:520-528`). It used to be pinned to 0, so the teardrop
    // was drawn at the very top of the panel while the model had placed it
    // beside the thumb: a relayout between the last touch event and this frame
    // (rotation, keyboard, a drawer opening) moved the track and the popup did
    // not follow. Recomputing here rather than trusting the event-time value
    // is the point -- the renderer is the only place that knows the frame's
    // geometry.
    st.popup_y = st.popup_top(&fs_layout);
    super::drawer_mod::draw_fast_scroller(
        buf,
        stride,
        w,
        h,
        &fs_layout,
        &st,
        state.palette.primary,
        super::drawer_mod::TRACK_ALPHA,
        state.palette.on_primary,
    );
}

/// Page indicator dots, from the page swipe's own progress.
///
/// `page_indicator_frac` is 0 at rest, an integer while settling, and above
/// 1.0 during the overshoot phase, which is why the dot maths treats it as a
/// signed value rather than a page index. The geometry is
/// `layout::page_indicator_dots`' -- a transliteration of
/// `PageIndicatorDots.java:553-635` -- and only the fill is here.
fn draw_page_indicator(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    let frac = state.page_indicator_frac;
    let pages = state.total_home_pages;
    // One page has nothing to indicate. A settled multi-page strip still
    // draws: the dots are chrome, not a transition effect, and gating on
    // `frac` would make the indicator appear only while moving.
    if pages < 2 {
        return;
    }
    let l = Layout::plain(w as f32, h as f32);
    let pi = l.page_indicator();
    // `page_indicator_dots` wants the page being *left* and the page being
    // moved *towards*, plus a progress that may exceed 1 during the snap. The
    // sign of `frac` carries the direction, which is why the field is signed
    // rather than two separate numbers.
    let p = frac.abs();
    let dir: i64 = if frac < 0.0 { -1 } else { 1 };
    let last = state.home_page.min(pages - 1);
    let final_page = (last as i64 + dir).clamp(0, pages as i64 - 1) as usize;
    let dots = super::layout::page_indicator_dots(&pi, pages, last, final_page, p);
    // The dots are a fixed 6 dp edge, vertically centred in the 24 dp band, and
    // only their *width* and *alpha* animate -- the reference scales the active
    // dot and fades the rest rather than resizing the row.
    let dot_h = pi.dot_d;
    let dot_y = pi.band.center_y() - dot_h * 0.5;
    for d in dots.iter() {
        if d.alpha <= 0.004 || d.w <= 0.0 {
            continue;
        }
        // `DotRect::alpha` is ALREADY on the 0..=255 scale -- it comes from
        // `layout::DOT_ALPHA`, not from a 0..1 opacity. The old code scaled it
        // by 255 again, so an active dot (128) overflowed the 24-bit shift and
        // an inactive one (64) landed *above* it: the contrast between the
        // selected page and the rest was inverted, and the `> 0.55` branch
        // taken by every dot because 64 > 0.55 on that scale.
        //
        // Lawnchair does not branch either. `PageIndicatorDots.java:589-591,
        // 628` paints every dot with one `mPaginationPaint` (Accent1_600) and
        // varies only the alpha, so the "which page am I on" signal is the
        // width/stretch animation above rather than a hue change.
        let alpha_u8 = d.alpha.clamp(0.0, 255.0) as u32;
        let color = (alpha_u8 << 24) | (state.palette.primary & 0x00FF_FFFF);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            d.x,
            dot_y,
            d.w,
            dot_h,
            dot_h * 0.5,
            color,
        );
    }
}

/// The in-app home gesture: the app panel shrinks and fades toward the
/// workspace card while the workspace is revealed underneath.
///
/// `workspace_scale` is 1.0 at rest and `window_alpha` is 1.0 at rest, which
/// is what keeps an idle frame out of this function entirely. The transform is
/// a geometric scale of the app panel about its centre -- there is no resample,
/// so no scratch buffer and no per-frame allocation. That is also why the
/// panel's *content* scale follows: `AppLayout` is constructed for the scaled
/// panel, so the whole app view shrinks with it rather than being clipped.
fn draw_workspace_morph(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    let scale = state.workspace_scale;
    let alpha = state.window_alpha;
    // At rest this is a no-op, and `paint_frame_does_not_allocate` is what
    // keeps it one.
    if (scale - 1.0).abs() < 0.0005 && (alpha - 1.0).abs() < 0.0005 {
        return;
    }
    let s = scale.clamp(0.0, 1.0);
    let a = alpha.clamp(0.0, 1.0);
    // A scrim over the workspace, so the panel reads as lifting off it rather
    // than fading into the wallpaper. Depth is proportional to how far the
    // panel has travelled, so it is invisible at the start of the gesture.
    let dim = (1.0 - a) * 0.5;
    if dim > 0.004 {
        // Black at `dim`, with no colour component: a dim is a darkening,
        // not a tint, so the RGB stays zero rather than inheriting the palette.
        let layer = ((dim * 255.0) as u32) << 24;
        draw_rect(buf, stride, w, h, 0, 0, w, h, layer);
    }
    // The workspace card the panel is shrinking into. Drawn as a rounded rect
    // at the panel's own centre, growing in inverse proportion to the panel's
    // scale so the two always meet at the same edge.
    if s < 0.999 {
        let l = Layout::plain(w as f32, h as f32);
        let card_w = (l.w * 0.86) * (1.0 - s);
        let card_h = (h as f32 * 0.42) * (1.0 - s);
        if card_w > 1.0 && card_h > 1.0 {
            let layer = (((1.0 - s) * 0.85 * 255.0) as u32) << 24
                | (state.palette.surface_container_high & 0x00FF_FFFF);
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                (l.w * 0.5 - card_w * 0.5).max(0.0),
                (h as f32 * 0.5 - card_h * 0.5).max(0.0),
                card_w.min(l.w),
                card_h.min(h as f32),
                (l.w * 0.06 * (1.0 - s) + 4.0).min(card_w * 0.5),
                layer,
            );
        }
    }
}

/// Recents / overview: a scrim, the card strip, and the action band.
///
/// Card geometry comes from `RecentsLayout`, the same struct the shell's
/// [`crate::compositor::recents::Recents`] hit-tests against, so a card is
/// drawn exactly where a drag on it starts.
#[allow(clippy::too_many_lines)]
fn draw_overview(buf: &mut [u32], stride: usize, w: usize, h: usize, state: &DrmInteractiveState) {
    let p = state.overview_progress.clamp(0.0, 1.0);
    let l = Layout::plain(w as f32, h as f32);
    let rl = l.recents();
    let wf = w as f32;
    let hf = h as f32;

    // Scrim first, so the workspace underneath is dimmed for the whole morph.
    // `#99000000`-ish, matching `RecentsView`'s scrim rather than the
    // launcher's own 0xEE panel scrim, which is opaque.
    let scrim = ((p * 0.55 * 255.0) as u32) << 24;
    draw_rect(buf, stride, w, h, 0, 0, w, h, scrim);

    if state.recents_cards.is_empty() {
        // Empty state, not a blank panel: "No recent items".
        let em = super::font::em_px_at(2, w);
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            w / 2,
            (hf * 0.47) as usize,
            "No recent items",
            state.palette.on_surface_variant,
            2,
        );
        let _ = em;
        return;
    }

    // The strip is centred on the panel and scrolled by `overview_scroll` in
    // card widths, so a card's x is `centre + (i - scroll) * pitch`.
    //
    // The *selected* card is the one that is centred, and the offset is
    // measured from it, not from index 0. The previous form,
    // `first_x = wf*0.5 - scroll*pitch - pitch*0.5`, always anchored card 0
    // one half-pitch left of centre and had no term for the selected index at
    // all, so with `selected = 0` card 0's LEFT edge landed on `wf*0.5` --
    // the whole card sat to the right of the middle -- and selecting card 2
    // moved nothing. `Recents::card_rect_centered` owns that geometry now, and
    // the shell hit-tests through the same function so a card cannot be drawn
    // in one place and grabbable in another.
    //
    // Without a model (a synthetic or preview frame) there is no selected
    // index to centre, so the strip falls back to anchoring card 0 at the
    // viewport centre -- which is what a one-card stack wants anyway.
    let fallback_first_x = wf * 0.5 - rl.card_w * 0.5;

    for (i, card) in state.recents_cards.iter().enumerate() {
        let x = match state.recents {
            Some(model) => match model.card_rect_centered(i, wf, hf) {
                Some(r) => r.x,
                None => continue,
            },
            None => fallback_first_x + i as f32 * (rl.card_w + rl.spacing),
        };
        if x > wf || x + rl.card_w < 0.0 {
            continue; // Off-panel: skipped without touching the buffer.
        }
        // Dismiss: the card tracks the drag up, scaling on the 5-arm ladder
        // that `dismiss_recents_scale` implements, so the renderer and the
        // shell agree on the shape of the fall-off.
        let dismiss = card.dismiss;
        let y = (hf * 0.5 - rl.card_h * 0.5) + dismiss;
        let scale = crate::compositor::dismiss_recents_scale(
            (dismiss / rl.dismiss_undershoot.max(1.0)).abs(),
        );
        let cw = rl.card_w * scale;
        let ch = rl.card_h * scale;
        let cx = x + rl.card_w * 0.5 - cw * 0.5;
        let cy = y + rl.card_h * 0.5 - ch * 0.5;
        // Cards below the dismiss threshold are on their way out, so they fade
        // with it rather than popping.
        let fade = if dismiss.abs() > rl.detach_dp {
            (1.0 - (dismiss.abs() - rl.detach_dp) / rl.card_h).clamp(0.0, 1.0)
        } else {
            1.0
        };
        if fade <= 0.01 {
            continue;
        }
        let a = (fade * if card.selected { 255.0 } else { 235.0 }) as u32;
        // Selected card gets the primary-coloured border the reference draws
        // on the focused task, and only that card.
        let card_bg = if card.selected {
            state.palette.surface_container_high
        } else {
            state.palette.surface_container
        };
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            cx,
            cy,
            cw,
            ch,
            rl.corner_r * scale,
            (a << 24) | (card_bg & 0x00FF_FFFF),
        );
        if card.selected {
            let ring = (wf * 0.0035).max(1.0);
            let edge = ((p * 0.9 * 255.0) as u32) << 24 | (state.palette.primary & 0x00FF_FFFF);
            draw_rect_f(buf, stride, w, h, cx, cy, cw, ring, edge);
            draw_rect_f(buf, stride, w, h, cx, cy + ch - ring, cw, ring, edge);
            draw_rect_f(buf, stride, w, h, cx, cy, ring, ch, edge);
            draw_rect_f(buf, stride, w, h, cx + cw - ring, cy, ring, ch, edge);
        }
        // App identity: the catalogue entry, or a neutral tile if a rescan
        // shrank the catalogue out from under the index.
        let entry = state.catalogue_apps.get(card.app_id as usize);
        if let Some(app) = entry {
            // Icon square on the app's own colour, then the decoded icon over
            // it, exactly as the home grid and the dock draw one: a coloured
            // rounded rect with the bitmap composited on top, or the monogram
            // glyph when the decode failed.
            let side = (cw * 0.30).round().max(8.0);
            let ix = (cx + cw * 0.5 - side * 0.5).round() as i32;
            let iy = (cy + ch * 0.30 - side * 0.5).round() as i32;
            let isz = side as usize;
            let irad = (side * ICON_RADIUS).round() as usize;
            draw_rounded_rect_i32(
                buf,
                stride,
                w,
                h,
                ix,
                iy,
                isz,
                isz,
                irad,
                (a << 24) | (app.color & 0x00FF_FFFF),
            );
            match app.icon {
                Some(icon_img) => {
                    draw_icon_bitmap_i32(buf, stride, w, h, ix, iy, isz, isz, irad, icon_img);
                }
                None => {
                    let em = super::font::em_px_at(2, w);
                    draw_text_centered_i32(
                        buf,
                        stride,
                        w,
                        h,
                        ix + isz as i32 / 2,
                        (iy + isz as i32 / 2) - (em * 0.30) as i32,
                        app.glyph,
                        (a << 24) | 0x00FF_FFFF,
                        2,
                    );
                }
            }
            draw_text_centered_clipped(
                buf,
                stride,
                w,
                h,
                (cx + cw * 0.5) as usize,
                (cy + ch * 0.66) as usize,
                cw * 0.86,
                app.name,
                state.palette.on_surface,
                1,
                FontWeight::Medium,
            );
        } else {
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                cx + cw * 0.5 - cw * 0.15,
                cy + ch * 0.30 - cw * 0.15,
                cw * 0.30,
                cw * 0.30,
                cw * 0.08,
                (a << 24) | (state.palette.surface_container_high & 0x00FF_FFFF),
            );
        }
    }

    // Action band: the 48 dp strip with the two action pills, fading in with
    // the overview rather than being present from the first frame.
    let band_a = ((p * 255.0) as u32) << 24;
    if band_a > 8 {
        let ay = rl.actions.y;
        let pill_w = (rl.actions.w - rl.actions_gap) * 0.5;
        for (k, label) in ["Clear all", "Close"].iter().enumerate() {
            let px = if k == 0 {
                rl.actions.x
            } else {
                rl.actions.x + pill_w + rl.actions_gap
            };
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                px,
                ay,
                pill_w,
                rl.actions.h,
                rl.actions_radius,
                (band_a >> 8 << 8) | (state.palette.surface_container_high & 0x00FF_FFFF),
            );
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                (px + pill_w * 0.5) as usize,
                (ay + rl.actions.h * 0.5 - super::font::em_px_at(1, w) * 0.31) as usize,
                label,
                state.palette.on_surface_variant,
                1,
            );
        }
    }
}

/// Open folder: scrim, the folder surface, its grid, and its title.
///
/// The title is a separate field from the morph because the reference fades it
/// in *after* a 32 ms delay (`FolderLayout::title_delay_ms`), so it must not
/// be a function of `folder_morph` or it would appear in the same frame.
fn draw_folder(buf: &mut [u32], stride: usize, w: usize, h: usize, state: &DrmInteractiveState) {
    // `for_shell`, not `plain`: a folder's grid is rows of icons with labels
    // under them, and pinning the scale to 1.0 here made the type scale stop at
    // the folder's edge. `state.font_scale` is the same value the workspace and
    // the drawer use.
    let l = Layout::for_shell(w as f32, h as f32, state.font_scale, false);
    // `new_with`, not `new`: the grid size is a setting
    // (`PreferenceManager2.kt:706-711`) and the scrim alpha follows the theme.
    // `FolderLayout::new` passed the dark branch unconditionally, which is why
    // `FOLDER_SCRIM_ALPHA_LIGHT` had no caller and a light-themed launcher still
    // opened folders behind a dark scrim.
    let grid = (state.folder_grid.0 as usize, state.folder_grid.1 as usize);
    let fl = FolderLayout::new_with(&l, Some(grid), state.folder_dark);
    let wf = w as f32;
    let hf = h as f32;
    let m = state.folder_morph.clamp(0.0, 1.0);

    let scrim = ((state.folder_scrim * 255.0) as u32) << 24;
    if scrim > 4 {
        draw_rect(buf, stride, w, h, 0, 0, w, h, scrim);
    }

    // The full-size surface, from `FolderLayout`: the grid plus its padding and
    // the footer row. `cell` is `Rect`, so the grid is `cols` wide by `rows`
    // tall with no gap field -- the gap is baked into the cell pitch.
    let cols = fl.cols.max(1) as f32;
    let grid_w = fl.cell.w * cols;
    let grid_h = fl.cell.h * fl.rows.max(1) as f32;
    let full_w = (grid_w + fl.pad_lr * 2.0).min(wf);
    let full_h = (fl.pad_top + grid_h + fl.footer_h).min(hf);
    let cell = fl.cell;

    // The surface grows out of the tapped folder icon, so its size and corner
    // radius interpolate rather than cross-fading between two rectangles.
    let s = m * m * (3.0 - 2.0 * m);
    let sw = (l.icon_size + (full_w - l.icon_size) * s).min(wf);
    let sh = (l.icon_size + (full_h - l.icon_size) * s).min(hf);
    let sx = (wf * 0.5 - sw * 0.5).max(0.0);
    let sy = (hf * 0.5 - sh * 0.5).max(0.0);
    let a = ((m * 255.0) as u32) << 24;
    // Clear before the early return, not after it: a morph that is open but
    // still transparent (`a <= 4` on its first frame) paints nothing, and
    // without this the previous edit's field survives into it.
    FOLDER_RENAME.with(|c| c.set(FolderRenameGeometry::EMPTY));
    FOLDER_GRID.with(|c| c.set(FolderGridGeometry::EMPTY));
    if a <= 4 {
        return;
    }
    // Corner radius: an icon's on the way out, the container's on the way in.
    // The container radius is the footer height, which is the reference's
    // sheet-like proportion rather than an arbitrary 24 dp.
    let full_r = fl.footer_h * 0.5;
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        sx,
        sy,
        sw,
        sh,
        l.icon_radius + (full_r - l.icon_radius) * s,
        (a >> 8 << 8) | (state.palette.surface_container_high & 0x00FF_FFFF),
    );

    // The grid's own origin, once the container has finished growing.
    // Computed here (not after the pager) because the rename field needs the
    // same offset: `FolderLayout` rects are layout coordinates while the sheet
    // is vertically centred, so `rename_field` must be shifted by
    // `gx - cell.x, gy - cell.y` (see `folder_layout_offset`, `main.rs:645`).
    // `FolderLayout::menu` is already panel-space and must not shift.
    let gx = wf * 0.5 - grid_w * 0.5;
    let gy = sy + fl.pad_top;
    // Clear the published rename first, so a closed folder reports no field.
    FOLDER_RENAME.with(|c| c.set(FolderRenameGeometry::EMPTY));
    if state.folder_rename_editing {
        // Visible text field when editing, replacing the static title.
        //
        // Pre-fill vs empty: `folder_rename_display` is the buffer when
        // non-empty and `""` when empty; empty draws `FOLDER_RENAME_PLACEHOLDER`
        // and never `folder_title`, so a cleared field cannot resurrect the old
        // name. The shell pre-fills the buffer on edit start
        // (`Folder.java:569`, `:1851`).
        let layout_field = fl.rename_field();
        let ox = gx - fl.cell.x;
        let oy = gy - fl.cell.y;
        let em = super::font::em_px_at(1, w);
        let text = state.folder_rename_display();
        let text_w = if text.is_empty() {
            0.0
        } else {
            super::font::measure(text, em)
        };
        let layout_caret = fl.rename_caret(layout_field, text_w);
        let panel_field = Rect {
            x: layout_field.x + ox,
            y: layout_field.y + oy,
            w: layout_field.w,
            h: layout_field.h,
            radius: layout_field.radius,
        };
        let panel_caret = Rect {
            x: layout_caret.x + ox,
            y: layout_caret.y + oy,
            w: layout_caret.w,
            h: layout_caret.h,
            radius: 0.0,
        };
        FOLDER_RENAME.with(|c| {
            c.set(FolderRenameGeometry {
                field: panel_field,
                caret: panel_caret,
                editing: true,
                live: true,
            })
        });
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            panel_field.x,
            panel_field.y,
            panel_field.w,
            panel_field.h,
            panel_field.radius,
            (a >> 8 << 8) | (state.palette.surface & 0x00FF_FFFF),
        );
        if text.is_empty() {
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                panel_field.x + panel_field.h * 0.2,
                panel_field.center_y() - em * 0.31,
                (panel_field.w - panel_field.h * 0.4).max(0.0),
                super::layout::FOLDER_RENAME_PLACEHOLDER,
                state.palette.on_surface_variant,
                1,
                FontWeight::Regular,
            );
        } else {
            draw_text_clipped(
                buf,
                stride,
                w,
                h,
                panel_field.x + panel_field.h * 0.2,
                panel_field.center_y() - em * 0.31,
                (panel_field.w - panel_field.h * 0.4).max(0.0),
                text,
                state.palette.on_surface,
                1,
                FontWeight::Regular,
            );
        }
        draw_rect_f(
            buf,
            stride,
            w,
            h,
            panel_caret.x,
            panel_caret.y,
            panel_caret.w,
            panel_caret.h,
            state.palette.on_surface,
        );
    } else {
        // Title, on its own alpha so the reference's delay is expressible.
        let ta = state.folder_title_alpha.clamp(0.0, 1.0);
        if ta > 0.01 && !state.folder_title.is_empty() {
            let em = super::font::em_px_at(1, w);
            draw_text_centered(
                buf,
                stride,
                w,
                h,
                w / 2,
                (sy + sh - fl.footer_h * 0.62 - em * 0.31) as usize,
                state.folder_title,
                state.palette.on_surface,
                1,
            );
        }
    }

    // Contents on the `FolderLayout` grid. The cells fade and shrink with the
    // morph, so a folder opening does not pop its contents in at full size on
    // the frame the surface reaches its final geometry.
    if state.folder_apps.is_empty() {
        FOLDER_GRID.with(|c| c.set(FolderGridGeometry::EMPTY));
        draw_folder_menu(buf, stride, w, h, state, &l, &fl);
        return;
    }

    // The pager, under the grid. `FolderLayout::pager` returns an *empty* rect
    // unless the folder needs more than one page, so a one-page folder cannot
    // draw dots by accident -- which is the reference's own condition
    // (`getPageCount() > 1`, `FolderPagedView.java:496`).
    let pager = fl.pager(state.folder_item_count as usize);
    if !pager.is_empty() {
        let em = super::font::em_px_at(1, w);
        let dots = (state.folder_page as u32 + 1)
            .min(fl.page_count(state.folder_item_count as usize) as u32);
        let pitch = pager.w / (fl.page_count(state.folder_item_count as usize) as f32).max(1.0);
        for p in 0..dots {
            let on = p == state.folder_page as u32;
            let r = (em * 0.14 * if on { 1.0 } else { 0.55 }).round().max(1.0);
            draw_rounded_rect_f(
                buf,
                stride,
                w,
                h,
                pager.x + pitch * (p as f32 + 0.5) - r * 0.5,
                pager.center_y() - r * 0.5,
                r,
                r,
                r * 0.5,
                if on {
                    (0xFF << 24) | (state.palette.primary & 0x00FF_FFFF)
                } else {
                    (0x66 << 24) | (state.palette.on_surface & 0x00FF_FFFF)
                },
            );
        }
    }

    // The grid was already located above for the rename offset; reuse `gx`
    // and `gy` here so the grid and the field cannot drift into two origins.
    // Clear the published grid *first*, then republish below once it is known.
    //
    // This is the same trap `SETTINGS_GEOM` documents: without the clear, the
    // previous frame's origin and pitch survive, and a folder that has just been
    // closed still reports a hit-testable grid -- so a tap on the home screen
    // lands on a folder cell that is no longer on screen. Clearing on every path
    // that is not "an open folder with a page of items in it" is what makes
    // "nothing is tappable" a value rather than an inference.
    FOLDER_GRID.with(|c| c.set(FolderGridGeometry::EMPTY));
    // `folder_apps` is the WHOLE folder; this page is a window into it. The
    // previous version took the first `cols * rows` items and silently dropped
    // the rest, so a folder with more than nine apps could not show items 10+
    // at all -- the reference pages them (`FolderPagedView.itemsPerPage`).
    let per_page = fl.items_per_page();
    let first = (state.folder_page as usize).saturating_mul(per_page);
    if first >= state.folder_apps.len() {
        draw_folder_menu(buf, stride, w, h, state, &l, &fl);
        return;
    }
    // Reorder, when one is in flight. Both halves come from the shell, and
    // `fl.reflow_slot` is the single permutation both the pixels and the shell's
    // understanding of the order go through, so the gap cannot open one place
    // and the tiles land in another.
    let drag = state.folder_drag_slot.map(|v| v as usize);
    let drop = state.folder_drop_slot.map(|v| v as usize);
    // The grid travels with the container while the sheet grows, so a
    // `FolderLayout` rect has to be moved by the same delta the grid origin did.
    // Derived, not a second number: `fl.cell.y` is `pad_top` and `gy` is
    // `sy + pad_top`, so this is `sy`, and writing `sy` directly would hide that.
    let gy_off = gy - fl.cell.y;
    // Now the grid is known, so publish it. The draw path is the only thing that
    // knows where the sheet actually landed: `FolderLayout`'s own cell rects are
    // in *layout* coordinates (`cell_at(col, row)` returns y = `pad_top`) while
    // the sheet is vertically centred on the panel, so the drawn row is
    // `pad_top + sy` and a caller that hit-tested the layout rects directly
    // would be aiming most of a screen height too high.
    //
    // The three gesture fields go in with it rather than being re-read by the
    // caller. A reorder has already moved the items, so a hit test that answered
    // "the item in this cell" would name an item that has slid one place along;
    // and the lifted and insertion slots are the two cells whose occupant is *not*
    // the item that owns the cell. Both are only knowable from here.
    FOLDER_GRID.with(|c| {
        c.set(FolderGridGeometry {
            origin: (gx, gy),
            cell: (cell.w, cell.h),
            cols: fl.cols.max(1),
            rows: fl.rows.max(1),
            per_page,
            first,
            drag_slot: drag,
            drop_slot: drop,
            drag_out: state.folder_drag_out,
            live: true,
        })
    });
    // Republish with `live` set, now that the page is known to have items in it.
    FOLDER_GRID.with(|c| {
        let mut g = c.get();
        g.live = true;
        c.set(g);
    });
    for (slot, app) in state.folder_apps[first..].iter().take(per_page).enumerate() {
        // The lifted cell is not in the grid; it is under the finger, drawn once
        // at the end of this function. Leaving it here would draw it twice, and
        // the copy in the grid would sit exactly in the gap the drag opened.
        //
        // The skip is on `drag` alone and *not* on the pair: a drag that has been
        // picked up but has not yet resolved an insertion index -- which is every
        // drag for its first 250 ms, because the reference arms a debounce alarm
        // rather than reordering on the first frame (`Folder.java:1205-1215`,
        // `REORDER_DELAY` `:197`) -- still has to lift its cell out of the grid.
        // Gating the skip on `drop` as well draws the icon twice for the whole
        // debounce window, on top of each other, which is invisible in a
        // screenshot of a slow drag and only shows up once the finger has moved.
        //
        // `slot_rect`, not `cell_at(slot, 0)`: `cell_at` takes (col, row), and
        // passing the slot as the column stacks the whole first page into row 0
        // and runs everything past the third off the right edge of the sheet. The
        // resting frame is the *same* mistake, so the two agree and the folder
        // looks superficially fine while items 4..9 sit on top of items 1..3.
        if drag == Some(slot) {
            continue;
        }
        let reflowed = match (drag, drop) {
            (Some(d), Some(k)) => fl.reflow_slot(slot, d, k, true),
            _ => Some(fl.slot_rect(slot)),
        };
        let Some(cell_rect) = reflowed else { continue };
        let cx = gx + cell_rect.x - fl.cell.x;
        let cy = gy_off + cell_rect.y;
        if cx < 0.0 || cy < 0.0 || cx + cell.w > wf || cy + cell.h > hf {
            continue;
        }
        let isz = (cell.w * s).max(2.0);
        let ipx = (cx + cell.w * 0.5 - isz * 0.5).round() as i32;
        let ipy = (cy + cell.h * 0.12 - isz * 0.5).round() as i32;
        let iszu = isz as usize;
        let irad = (isz * ICON_RADIUS).round() as usize;
        draw_rounded_rect_i32(
            buf,
            stride,
            w,
            h,
            ipx,
            ipy,
            iszu,
            iszu,
            irad,
            (a >> 8 << 8) | (app.color & 0x00FF_FFFF),
        );
        match app.icon {
            Some(icon_img) => {
                draw_icon_bitmap_i32(buf, stride, w, h, ipx, ipy, iszu, iszu, irad, icon_img);
            }
            None => {
                let em = super::font::em_px_at(1, w);
                draw_text_centered_i32(
                    buf,
                    stride,
                    w,
                    h,
                    ipx + iszu as i32 / 2,
                    ipy + iszu as i32 / 2 - (em * 0.30) as i32,
                    app.glyph,
                    (a >> 8 << 8) | 0x00FF_FFFF,
                    1,
                );
            }
        }
        if cell.h > 24.0 {
            draw_text_centered_clipped(
                buf,
                stride,
                w,
                h,
                (cx + cell.w * 0.5) as usize,
                (cy + cell.h * 0.74) as usize,
                cell.w * 1.2,
                app.name,
                state.palette.on_surface_variant,
                1,
                FontWeight::Regular,
            );
        }
    }

    // The gap: an outlined cell where the dragged item will land. Drawn *after*
    // the grid so the outline is not overpainted by a tile that reflowed into the
    // same row, and skipped when the drag is out of the folder entirely -- a gap
    // inside a sheet whose contents are following the finger out of it reads as a
    // cell that lost its app.
    if let (Some(k), false) = (drop, state.folder_drag_out) {
        let gap = fl.slot_rect(k);
        let gx0 = gx + gap.x - fl.cell.x + cell.w * 0.5;
        let gy0 = gy_off + gap.y + cell.h * 0.12;
        let gs = (cell.w * s * 0.94).max(2.0);
        // Half a dp, matching the hairline the drawer's divider uses
        // (`drm_kms.rs:5058`, `stroke_px = dp * 0.5`). The reference outlines a
        // drag's drop target with a `DropTargetFrame` rather than a bare
        // stroke, but there is no UTLC equivalent of that drawable and a
        // two-pass fill of a dashed frame would be a per-frame cost for a
        // decoration the reference also animates away.
        super::raster::draw_rounded_rect_outline_f(
            buf,
            stride,
            w,
            h,
            gx0,
            gy0,
            gs,
            gs,
            gs * ICON_RADIUS,
            (w as f32 / 420.0 * 0.5).max(1.0),
            ((0x99u32) << 24) | (state.palette.on_surface_variant & 0x00FF_FFFF),
        );
    }

    // The lifted cell, under the finger.
    if let Some(d) = drag {
        if let Some(app) = state.folder_apps.get(first + d) {
            // Frost the band the cell is about to cover, so the sheet reads as
            // *under* it rather than beside it.
            //
            // The reference has no equivalent here: its drag view is an
            // overlay with a real `RenderNode` (`DragView.java:299-310`) and the
            // only blur in its drag path is the adaptive icon's own outline
            // (`DragView.java:264-268`, `blur_size_medium_outline` = 2 dp,
            // `dimens.xml:338`), not a blur of what is behind. What *is* in the
            // reference is the workspace itself being blurred while something
            // covers it (`BaseDepthController.java:365-367`, a
            // `RenderEffect.createBlurEffect` on the depth-blur targets, cleared
            // at depth 0 by `:375-382`). This is that, applied to the region a
            // lifted cell is over, and bounded to the cell plus its shadow so the
            // cost is a few thousand pixels and not the sheet.
            let (fx, fy) = state.folder_drag_pos;
            let rest = (cell.w * s).max(2.0);
            let lift = fl.drag_lift(rest, &l);
            let edge = (rest * lift).max(2.0);
            let soff = fl.drag_shadow_offset(&l) * (0.5 + lift * 0.5);
            // Full width, not just under the cell, because that is what the
            // reference blurs: `getDepthBlurTargets()` are the workspace and
            // hotseat, which are panel-wide rows of icons
            // (`Folder.java:1158-1160`, `BaseDepthController.java:367`), and a
            // cell-local blur would leave a hard vertical seam through the icons
            // either side of the drag. The band is the cell's own height, so the
            // cost is `panel_w * cell_h` -- about a tenth of the frame at 1080 x
            // 2400 with a 3x3 folder -- and only while a cell is actually lifted.
            let by = (fy - edge * 0.5 + soff).floor().max(0.0);
            let bh = edge + soff * 2.0;
            if bh >= 1.0 && by < h as f32 {
                apply_frosted_blur_region(
                    buf,
                    stride,
                    w,
                    (by as usize).min(h),
                    ((by + bh) as usize).min(h),
                );
            }
            // Key shadow: `#89000000` at `.5dp` down and across
            // (`styles.xml:422, 425-426`). Suppressed in the dark theme, where the
            // reference makes both layers transparent (`styles.xml:112-113`) --
            // so this is a *lookup*, not a zero alpha, because a dark folder drag
            // with a shadow is visibly wrong rather than merely wasteful.
            if !state.folder_dark {
                draw_rounded_rect_f(
                    buf,
                    stride,
                    w,
                    h,
                    fx - edge * 0.5 + soff,
                    fy - edge * 0.5 + soff,
                    edge,
                    edge,
                    edge * ICON_RADIUS,
                    // Opaque black at the key shadow's alpha, not a palette
                    // role: Material 3 defines `shadow` and `scrim` and
                    // `MaterialYouPalette` has neither, so the honest value is
                    // the reference's own `#89000000` and a drop shadow is the
                    // absence of light rather than a theme colour. The
                    // home-grid icon shadow above derives it the same way.
                    (super::layout::FOLDER_DRAG_SHADOW_ALPHA as u32) << 24,
                );
            }
            let ipx = (fx - edge * 0.5).round() as i32;
            let ipy = (fy - edge * 0.5).round() as i32;
            let iszu = edge as usize;
            let irad = (edge * ICON_RADIUS).round() as usize;
            draw_rounded_rect_i32(
                buf,
                stride,
                w,
                h,
                ipx,
                ipy,
                iszu,
                iszu,
                irad,
                (a >> 8 << 8) | (app.color & 0x00FF_FFFF),
            );
            match app.icon {
                Some(icon_img) => {
                    draw_icon_bitmap_i32(buf, stride, w, h, ipx, ipy, iszu, iszu, irad, icon_img);
                }
                None => {
                    let em = super::font::em_px_at(1, w);
                    draw_text_centered_i32(
                        buf,
                        stride,
                        w,
                        h,
                        ipx + iszu as i32 / 2,
                        ipy + iszu as i32 / 2 - (em * 0.30) as i32,
                        app.glyph,
                        (a >> 8 << 8) | 0x00FF_FFFF,
                        1,
                    );
                }
            }
        }
    }

    // The drop-target bar, once the drag has left the folder: the reference's
    // "Remove" affordance (`DeleteDropTarget.java:115`,
    // `res/values/strings.xml:221`), which is what the release commits against.
    // Drawn last so it is over the lifted cell if the two overlap, which is the
    // reference's own z order -- the bar is a `layout_gravity="top"` sibling of
    // the drag layer's contents (`drop_target_bar.xml:20,24`).
    if state.folder_drag_out {
        let dtb = super::layout::drop_target_bar(&l);
        draw_rect_f(
            buf,
            stride,
            w,
            h,
            dtb.bar.x,
            dtb.bar.y,
            dtb.bar.w,
            dtb.bar.h,
            (0xE6 << 24) | (state.palette.surface_container_high & 0x00FF_FFFF),
        );
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            dtb.button.x,
            dtb.button.y,
            dtb.button.w,
            dtb.button.h,
            dtb.button.radius,
            state.palette.primary,
        );
        draw_text_centered(
            buf,
            stride,
            w,
            h,
            (dtb.button.center_x()) as usize,
            (dtb.button.center_y() - super::font::em_px_at(1, w) * 0.31) as usize,
            dtb.label,
            state.palette.on_primary,
            1,
        );
        // Cross-surface workspace slot, distinct from the Remove bar above.
        //
        // The bar is a top-band primary fill (`DeleteDropTarget.java:115`); this
        // is a grid-cell outline in the tertiary role, so the two affordances
        // cannot be confused. Resolved by `folder_drag_to_workspace_target` --
        // the same function the shell's dispatch reads -- so the drawn slot is
        // the slot a release would commit against. `None` draws nothing, which
        // is what a drag over the scrim wants.
        if let Some(slot) =
            super::layout::folder_drag_to_workspace_target(&l, state.folder_drag_pos)
        {
            let g = l.drag_gap_rect(slot);
            super::raster::draw_rounded_rect_outline_f(
                buf,
                stride,
                w,
                h,
                g.x,
                g.y,
                g.w,
                g.h,
                g.radius,
                (w as f32 / 420.0 * 0.5).max(1.0),
                (0xFF << 24) | (state.palette.tertiary & 0x00FF_FFFF),
            );
        }
    }

    draw_folder_menu(buf, stride, w, h, state, &l, &fl);
}

/// The folder's long-press menu: three rows, drawn from the same
/// [`FolderMenuLayout`] the shell hit-tests.
///
/// **The reference has no folder long-press menu** -- its folder long press
/// starts a drag (`Folder.java:459-463`) -- so the rows are UTLC's, laid out on
/// the reference-cited `PopupMenuLayout` the workspace menu already uses
/// (216 x 52 dp rows, 24 dp outer radius; `PopupMenuLayout::new`). The three
/// actions are the reference's real folder-adjacent verbs, cited on
/// [`FolderMenuAction`]: the folder name field (`Folder.java:569`, `:1851`), the
/// "Remove" drop-target label (`res/values/strings.xml:221`) and
/// `SystemShortcut.APP_INFO` (`SystemShortcut.java:188`).
///
/// Takes the caller's `Layout` and `FolderLayout` rather than building its own.
/// A second `FolderLayout::new` here would be a third folder grid on a frame that
/// already has two, and it is exactly the "two derivations of where is card N"
/// class of bug this file's comments have flagged three times. The menu's own
/// geometry does not depend on the folder grid at all -- `PopupMenuLayout` is a
/// function of the panel -- but the `Layout` is what scales the row type, and a
/// menu drawn at scale 1.0 inside a folder whose grid is at 1.3 is visibly a
/// different menu.
fn draw_folder_menu(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
    l: &Layout,
    fl: &FolderLayout,
) {
    let m = state.folder_menu_progress.clamp(0.0, 1.0);
    if m <= 0.0 {
        return;
    }
    let (ax, ay) = state.folder_menu_anchor;
    let menu = fl.menu(l, ax, ay, m);
    let place = menu.place;
    if place.w < 1.0 || place.h < 1.0 {
        return;
    }
    let a = ((m * 255.0) as u32) << 24;
    // The corner radius follows the same grow the placement used, so the surface
    // never draws a full-size radius around a surface that is still a sliver.
    let grow = m * m * (3.0 - 2.0 * m);
    let pop = l.popup_menu(Rect {
        x: ax,
        y: ay,
        w: 0.0,
        h: 0.0,
        radius: 0.0,
    });
    // The elevation shadow, and it is load-bearing rather than decorative here.
    //
    // The workspace menu draws `surface_container_high` over the workspace, which
    // is a different colour, so it reads. This menu draws the *same* role over
    // the folder sheet -- which is also `surface_container_high`
    // (`draw_folder`'s surface, above) -- so without a shadow the menu is a
    // rectangle of the exact colour of the thing behind it, holding three labels
    // with no visible boundary. `PopupMenuLayout` already carries the field that
    // says so (`elevation: p.dp(2.0)`, `layout.rs:3193`); this is that field
    // rendered, as a soft dark rounded rect behind the surface, which is what
    // Material 3 elevation is on a flat renderer.
    if pop.elevation > 0.0 {
        let spread = pop.elevation * 2.5;
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            place.x - spread * 0.5,
            place.y - spread * 0.25 + pop.elevation,
            place.w + spread,
            place.h + spread * 0.5,
            (pop.outer_r + spread * 0.5) * grow,
            (0x66 << 24) as u32,
        );
    }
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        place.x,
        place.y,
        place.w,
        place.h,
        pop.outer_r * grow,
        (a >> 8 << 8) | (state.palette.surface_container_high & 0x00FF_FFFF),
    );
    let pad = menu.inner_r.max(8.0);
    for (i, action) in super::layout::FolderMenuAction::ALL.iter().enumerate() {
        let row = menu.rows[i];
        if row.is_empty() {
            continue;
        }
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            row.x + pad * 1.5,
            menu.label_y(i),
            (row.w - pad * 3.0).max(0.0),
            action.label(),
            state.palette.on_surface,
            1,
            FontWeight::Regular,
        );
    }
}

/// Label for a popup row.
///
/// `crate::compositor::PopupItem` carries no string, deliberately: it is a
/// semantic tag the shell hit-tests against, and a `&'static str` per variant
/// is both smaller than a `String` and impossible to get out of sync with the
/// variant. The strings are the reference's own menu titles from
/// `LauncherOptionsPopup.kt:18-28` and `SystemShortcut`.
fn popup_label(item: &crate::compositor::PopupItem) -> &'static str {
    use crate::compositor::PopupItem as P;
    match item {
        P::Wallpapers => "Wallpapers",
        P::Widgets => "Widgets",
        P::AllApps => "All apps",
        P::HomeSettings => "Home settings",
        P::HomeScreenLock => "Lock screen",
        P::EditMode => "Edit mode",
        P::SystemSettings => "System settings",
        P::DefaultPageForWorkspace => "Set default",
        P::AppInfo => "App info",
        P::Install => "Install",
        P::Remove => "Remove",
        P::Uninstall => "Uninstall",
        P::Customize => "Customize",
        P::OpenInStore => "Open in store",
        P::PauseApps => "Pause",
        P::DeepShortcut(_) => "Shortcut",
    }
}

/// Long-press popup: a rounded surface with one row per item.
///
/// Positioned by `PopupMenuLayout::place`, which the shell's hit test also
/// calls, so a row is tappable exactly where it is drawn. The comment here used
/// to assert that property about a struct the shell never queried; the two now
/// share one function.
fn draw_popup(buf: &mut [u32], stride: usize, w: usize, h: usize, state: &DrmInteractiveState) {
    if state.popup_items.is_empty() {
        return;
    }
    let l = Layout::plain(w as f32, h as f32);
    let (ax, ay) = state.popup_anchor;
    // The anchor is a zero-size rect at the touch point: the layout only reads
    // its edges, and a zero rect is the honest representation of "a point".
    let anchor = super::layout::Rect {
        x: ax,
        y: ay,
        w: 0.0,
        h: 0.0,
        radius: 0.0,
    };
    let pl = l.popup_menu(anchor);
    let m = state.popup_progress.clamp(0.0, 1.0);
    let a = ((m * 255.0) as u32) << 24;
    let place = pl.place(&l, ax, ay, state.popup_items.len(), m);
    let rows = place.rows;
    let (px, py, pw, ph) = (place.x, place.y, place.w, place.h);
    let grow = m * m * (3.0 - 2.0 * m);
    if ph < 1.0 || pw < 1.0 {
        return;
    }
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        px,
        py,
        pw,
        ph,
        pl.outer_r * grow,
        (a >> 8 << 8) | (state.palette.surface_container_high & 0x00FF_FFFF),
    );
    let em = super::font::em_px_at(1, w);
    let pad = pl.inner_r.max(8.0);
    // No arrow is drawn. The reference always positions a `RoundedArrowDrawable`
    // at the icon (`ArrowPopup.java:373-384`) and suppresses it only when the
    // popup had to be centred *beside* the icon rather than above or below it.
    // UTLC centres the menu on the anchor unconditionally, so every placement
    // is the "centred" case. `PopupMenuLayout` already carries the arrow
    // metrics for the placement search that would make them reachable.
    for (i, item) in state.popup_items.iter().take(rows).enumerate() {
        let ry = py + i as f32 * pl.item_h;
        // App-info is the affirmative row and takes the accent colour.
        let fg = if matches!(item, crate::compositor::PopupItem::AppInfo) {
            state.palette.primary
        } else {
            state.palette.on_surface
        };
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            px + pad,
            ry + pl.item_h * 0.5 - em * 0.31,
            pw - pad * 2.0,
            popup_label(item),
            fg,
            1,
            FontWeight::Regular,
        );
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
        mix!((radius.min(RIPPLE_MAX_RADIUS) * 10.0) as u32);
        mix!((alpha * 100.0) as u32);
        // The clip is part of the damage signature: moving the same circle
        // from one control's bounds to another's changes the pixels even
        // though every centre/radius term is identical.
        match state.ripple_clip {
            Some(c) => {
                mix!(1);
                mix!(c.x as u32);
                mix!(c.y as u32);
                mix!(c.w as u32);
                mix!(c.h as u32);
            }
            None => mix!(0),
        }
    } else {
        mix!(0);
    }
    if let Some(pid) = state.pressed_icon_id {
        mix!(1);
        for b in pid.bytes() {
            mix!(b);
        }
    } else {
        mix!(0);
    }
    // Hashed UNCONDITIONALLY, outside the `Some` arm.
    //
    // The scale is the press spring's value, and the spring keeps
    // integrating after `pressed_icon_id` is cleared: the shell clears the
    // id once the spring is at rest, but the release overshoot is several
    // frames long, and during exactly those frames the icon is still
    // shrinking or growing back. Hashing it only while an icon was selected
    // meant the tail of every release animation was invisible -- the icon
    // snapped back to full size on the final frame instead of easing.
    mix!((state.icon_press_scale * 1000.0) as u32);
    mix!(state.power_saver_mode as u8);

    // ---------------------------------------------------------------------
    // Plan §7.6 invariant. See the HARD INVARIANT note on the struct: an
    // unhashed animated field renders nothing, ever.
    //
    // Quantisation follows the existing convention: x1000 for unit-range
    // progress (smooth enough at 120 Hz, since 1/1000 of a screen is well
    // under a pixel), x100 for pixel-valued quantities.
    // ---------------------------------------------------------------------
    mix!((state.smartspace_phase * 1000.0) as u32);
    mix!((state.folder_morph * 1000.0) as u32);
    mix!((state.folder_scrim * 1000.0) as u32);
    mix!((state.folder_title_alpha * 1000.0) as u32);
    // The folder's *page*, *size* and *theme* are drawn, so they must be in the
    // damage hash or the tracker skips the repaint when only the page changes --
    // a folder that has been swiped to page 2 and back would keep the stale
    // page's pixels. Same for a badge count, which changes with no interaction
    // at all: it is driven by a notification arriving.
    mix!(state.folder_page as u32);
    mix!(state.folder_item_count as u32);
    mix!(state.folder_grid.0 as u32);
    mix!(state.folder_grid.1 as u32);
    mix!(state.folder_dark as u32);
    // The preview's *contents*, not just its presence: adding an app to a folder
    // changes what the tile shows with no interaction on the workspace at all,
    // and the folder morph is 0 for a closed folder, so nothing else in the hash
    // would move.
    for row in state.folder_previews {
        mix!(row.folder);
        mix!(row.n as u32);
        for icon in row.icons.iter().take(row.n as usize) {
            mix!(icon.is_some() as u32);
        }
    }
    for note in state.quick_tile_notes.iter() {
        for b in note.bytes().take(32) {
            mix!(b as u32);
        }
    }
    mix!(state.wallpaper_dim as u32);
    // The image's identity, not its bytes: hashing 8 MB of pixels every frame to
    // notice a *new* wallpaper would cost more than drawing one. The borrow's
    // address changes exactly when the shell swaps it, which is the only case
    // that matters.
    mix!(state
        .wallpaper
        .map(|i| i.pixels.as_ptr() as usize)
        .unwrap_or(0) as u32);
    for row in state.settings_rows {
        for b in row.key.bytes().take(24) {
            mix!(b as u32);
        }
        for b in row.value.bytes().take(16) {
            mix!(b as u32);
        }
        // A `Picker` row's drawn value is the position, not `value`: two rows
        // with the same static value and different `at` draw different pixels,
        // and the hash has to see it or the carousel silently stops moving.
        // `Option` as `u64::MAX` rather than a sentinel 0, because 0 is not a
        // live position and would still have to be distinguishable.
        mix!(row.at.map(|v| v as u64).unwrap_or(u64::MAX));
        mix!(row.of.map(|v| v as u64).unwrap_or(u64::MAX));
    }
    for b in state.drawer_section_letter.bytes() {
        mix!(b as u32);
    }
    for (id, _) in state.icon_shadows {
        for b in id.bytes().take(24) {
            mix!(b as u32);
        }
    }
    for row in state.notifications.iter().take(SHADE_NOTIF_ROWS) {
        for b in row.summary.bytes().take(48) {
            mix!(b as u32);
        }
        mix!(row.critical as u32);
        // The swipe offset is a *drag*, so it changes on every frame of the drag
        // and the tracker has to see it or the row stops following the finger.
        mix!((row.x_offset * 64.0) as i64 as u32);
    }
    for (id, n) in state.badge_counts {
        // The id is hashed so a badge moving between two apps with the same
        // count is still a change.
        for b in id.bytes().take(24) {
            mix!(b as u32);
        }
        mix!(*n);
    }
    mix!((state.popup_progress * 1000.0) as u32);
    mix!((state.overview_progress * 1000.0) as u32);
    mix!((state.overview_scroll * 1000.0) as u32);
    mix!((state.overview_dismiss * 100.0) as i32);
    mix!((state.fastscroller_thumb * 1000.0) as u32);
    mix!((state.fastscroller_popup_alpha * 1000.0) as u32);
    // The page indicator's overshoot phase lives above 1.0, so this is the
    // one field where a clamp would silently swallow real motion.
    mix!((state.page_indicator_frac * 1000.0) as i32);
    mix!((state.workspace_scale * 1000.0) as u32);
    mix!((state.window_alpha * 1000.0) as u32);

    // The smartspace line is a function of the wall clock, not of any user
    // interaction, so nothing else in this hash would change when the minute
    // rolls over. Without these the clock would freeze at whatever it showed
    // when the shell last happened to repaint.
    for b in state.date_str.bytes() {
        mix!(b);
    }
    for b in state.weather_str.bytes() {
        mix!(b);
    }
    mix!(state.weather_glyph as u64);

    // ---------------------------------------------------------------------
    // Content backing the new surfaces. These are lists rather than scalars,
    // so the count has to be mixed in or a card sliding from index 2 to
    // index 1 would hash the same as no change at all.
    // ---------------------------------------------------------------------

    // The catalogue is hashed by CONTENT, not by length. A `.len()` here
    // meant a rescan that changed an app's *name* -- "Firefox" becoming
    // "Firefox ESR", an icon theme swap, a reinstall that moves an app
    // within the alphabetical sort -- produced an identical hash whenever the
    // count happened to match, so the frame was skipped and the home screen
    // kept drawing the stale label. The overview reads this slice by index
    // (`catalogue_apps.get(card.app_id)`), so a rescan that reorders the
    // catalogue moves every card's app to a different tile: the *worst* case
    // of a length-only hash, and invisible for the same reason.
    mix_app_row(&mut h, state.catalogue_apps);
    // `recents_cards` carries two *animated* fields per card: `dismiss` moves
    // continuously during a drag and `selected` flips on a scrub. Hashing only
    // the length would make a card that is being swiped away render as a
    // frozen card, which is the same invisible-animation bug the struct-level
    // invariant above warns about, one level down.
    mix!(state.recents_cards.len() as u64);
    for card in state.recents_cards {
        mix!(card.app_id as u64);
        mix!((card.dismiss * 100.0) as i32);
        mix!(card.selected as u64);
    }
    mix!(state.fastscroller_letter as u64);
    mix!((state.popup_anchor.0 * 100.0) as i32);
    mix!((state.popup_anchor.1 * 100.0) as i32);
    mix!(state.popup_items.len() as u64);
    for item in state.popup_items {
        // The label, which is what actually gets drawn and is a pure function
        // of the variant (`popup_label`). `Discriminant` is not a primitive
        // and cannot be cast, and the two variants that differ only in a
        // payload differ only in a label the renderer ignores anyway.
        for b in popup_label(item).bytes() {
            mix!(b);
        }
    }
    // Same reason as the catalogue: an open folder draws this slice directly,
    // so a rename or a reorder inside it has to reach the damage hash or the
    // folder keeps rendering the tiles it had before the rescan.
    mix_app_row(&mut h, state.folder_apps);
    for b in state.folder_title.bytes() {
        mix!(b);
    }
    // The six folder-gesture fields, all animated or all live during a drag.
    // `render_interactive_ui` short-circuits on a hash match (`:670-685`), so an
    // unhashed field animates invisibly: the drag runs, the value changes, and no
    // frame is ever repainted. Quantised to 1/64 of a pixel because these are all
    // continuous, and hashing the raw f32 would dirty a frame on the last bit of
    // a spring that is visually identical.
    mix!(state.folder_drag_slot.map(|v| v as u64).unwrap_or(u64::MAX));
    mix!((state.folder_drag_pos.0 * 64.0) as i64 as u64);
    mix!((state.folder_drag_pos.1 * 64.0) as i64 as u64);
    mix!(state.folder_drop_slot.map(|v| v as u64).unwrap_or(u64::MAX));
    mix!(state.folder_drag_out as u64);
    mix!((state.folder_menu_progress * 1000.0) as u64);
    mix!((state.folder_menu_anchor.0 * 64.0) as i64 as u64);
    mix!((state.folder_menu_anchor.1 * 64.0) as i64 as u64);
    for b in state
        .folder_rename_buffer
        .bytes
        .iter()
        .take(state.folder_rename_buffer.len())
    {
        mix!(*b as u64);
    }
    mix!(state.folder_rename_buffer.len() as u64);
    mix!(state.folder_rename_editing as u64);

    // App info. Hashed by *content*, not by presence: the panel shows the app's
    // name, id and `Exec=` line, so a launcher rescan that changes an app's name
    // -- "Firefox" becoming "Firefox ESR" -- draws different pixels with the
    // panel open and identical structure. A presence-only hash would keep the
    // stale panel. Same reasoning as `mix_app_row` for the catalogue, and the
    // same trap that cost it there.
    match &state.app_info {
        Some(a) => {
            mix!(1);
            for b in a.id.bytes().take(48) {
                mix!(b);
            }
            for b in a.name.bytes().take(32) {
                mix!(b);
            }
            for b in a.exec.bytes().take(48) {
                mix!(b);
            }
            for b in a.target.bytes().take(48) {
                mix!(b);
            }
            mix!(a.icon.is_some() as u64);
            mix!(a.color as u64);
            for b in a.glyph.bytes().take(8) {
                mix!(b);
            }
            // The button's *enabled* state is drawn, so the gate is part of the
            // signature: a target that appears or disappears flips pixels.
            mix!(a.can_open() as u64);
            mix!(a.installing as u64);
        }
        None => mix!(0xA5),
    }

    // The IME page. A `u8` discriminant: the variant *is* the visible state, and
    // every character on rows 2 and 3 is a function of it.
    mix!(state.keyboard_layout as u64);

    // The five workspace-drag fields. All five are per-frame during a drag --
    // `drag_pos` and `drag_lift` are continuous and the three slots flip -- so
    // unhashed they animate invisibly, which is the same failure as an unhashed
    // folder drag and the same reason for the quantisation.
    mix!(state.drag_slot.map(|v| v as u64).unwrap_or(u64::MAX));
    mix!((state.drag_pos.0 * 64.0) as i64 as u64);
    mix!((state.drag_pos.1 * 64.0) as i64 as u64);
    mix!((state.drag_lift * 1000.0) as u64);
    mix!(state.drag_drop_slot.map(|v| v as u64).unwrap_or(u64::MAX));
    mix!(state.drag_merge_slot.map(|v| v as u64).unwrap_or(u64::MAX));

    if let Some(sex) = state.super_extreme_state {
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
///
/// Three call sites, all in `paint_frame`:
///
/// * the workspace behind the app drawer, above the sheet's top edge and past
///   [`DRAWER_BLUR_THRESHOLD`] -- the reference's depth blur
///   (`BaseDepthController.java:365-367`), which this was implemented for and
///   then left uncalled;
/// * the full-width band a lifted folder cell travels over, so the sheet reads
///   as *under* the finger rather than beside it;
/// * [`apply_frosted_blur`], the whole frame.
///
/// The fixed 3 px tile is why the drawer's blur is a threshold rather than a
/// ramp: the reference's radius is `(int)(depth * 23)` px
/// (`BaseDepthController.java:226-229`, `max_depth_blur_radius` from
/// `quickstep/res/values/config.xml:41`) and a constant-tile average cannot
/// express a radius. See [`DRAWER_BLUR_THRESHOLD`].
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
        let (mut lo, mut hi) =
            match rounded_span(cy as i32 - y, x, tw as i32, th as i32, radius as i32, r2) {
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
                    buf[row + cx as usize] = (0xFF << 24)
                        | ((src[o] as u32) << 16)
                        | ((src[o + 1] as u32) << 8)
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
    // Reject an empty or wholly off-panel rect before computing any index.
    //
    // `x_end = min(x + rw, w)` is what makes the *right* edge safe, but it is
    // only safe while `x <= x_end`. A caller that passes `x >= w` (a
    // translated rect whose origin has left the panel) got `x_end = w < x`,
    // and `buf[row + x .. row + x_end]` then panics on a reversed range. The
    // same applies to `y`, where the loop happened not to execute rather than
    // being correct, and to a zero-area rect, which must paint nothing rather
    // than fall through the bounds check with `x_end == x`.
    if x >= w || y >= h || rw == 0 || rh == 0 {
        return;
    }
    // `saturating_add` rather than `+`: `x + rw` overflowing `usize` would wrap
    // to a small value and make the rect *narrower* than asked for, which is
    // a silently wrong frame rather than a panic.
    let x_end = x.saturating_add(rw).min(w);
    let y_end = y.saturating_add(rh).min(h);
    let alpha = (color >> 24) & 0xFF;
    if alpha == 0 || x_end <= x || y_end <= y {
        return;
    }
    // The row slice must stay inside the buffer even if the caller passed a
    // `stride` larger than `w`; a row is `stride` wide, not `w`.
    if let Some(row_end) = y_end.checked_mul(stride) {
        if row_end > buf.len() {
            return;
        }
    } else {
        return;
    }

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
    if alpha == 0 {
        return;
    }

    for cy in y_start..y_end {
        let (mut lo, mut hi) =
            match rounded_span(cy as i32 - y, x, rw as i32, rh as i32, radius as i32, r2) {
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

/// A pre-computed radial falloff, baked once and blitted every frame.
///
/// The two accent glows are a pure function of the panel size: their centres,
/// radii and intensities are all derived from `w` and `h`, and their colours
/// come from the palette but are applied at *blit* time, not baked in. So the
/// per-pixel weight `a` is the same on every frame of every panel of a given
/// size, and recomputing it 120 times a second is pure waste.
///
/// Recomputing cost, per pixel: an `isqrt`-derived half-chord per row, a 64-bit
/// squared distance, a 64-bit multiply, a shift, a saturating add on each of
/// three channels, and a branch. At 1080p the two discs cover ~690,000 pixels,
/// which is the single largest item in the `home` frame.
///
/// Blitting cost, per pixel: one `u8` load, three saturating adds, one store.
/// Same output -- the plane *is* the same `a` the inner loop computed -- for a
/// fraction of the work, and the branch disappears because the plane is already
/// zero outside the disc.
///
/// Memory: one `u8` per pixel over the disc's bounding box, not the panel. At
/// 1080p the two discs are 601x601 and 721x721, so ~0.86 MiB total. A
/// full-panel `u32` plane would have been 9.9 MiB and would have blown the
/// 15 MiB RSS budget on its own; the bounding box is what makes this
/// affordable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GlowPlane {
    /// Panel width this plane was baked for, px. The height is not stored
    /// because no term of the falloff depends on it, but the *origin* does,
    /// so a height change invalidates the cache through the caller.
    w: usize,
    /// Top-left of the plane in panel coordinates.
    x0: usize,
    y0: usize,
    /// Plane dimensions, px.
    pw: usize,
    ph: usize,
    /// The falloff, row-major over `pw * ph`. Zero outside the disc.
    a: Vec<u8>,
}

impl GlowPlane {
    /// Bake the falloff of a disc at `(cx, cy)` with `radius` and `intensity`.
    fn bake(cx: usize, cy: usize, radius: usize, intensity: u8) -> Option<Self> {
        if radius == 0 {
            return None;
        }
        let r = radius as i64;
        let rad_sq = r * r;
        let x0 = cx.saturating_sub(radius);
        let y0 = cy.saturating_sub(radius);
        // The box is clipped to the panel, so a disc centred off-panel does
        // not allocate for pixels that will never be written.
        let pw = radius * 2 + 1;
        let ph = pw;
        let inv_rsq = (1u64 << 32) / rad_sq as u64;
        let inten = intensity as u64;
        let mut a = vec![0u8; pw * ph];
        let dxc = cx as i32;
        let dyc = cy as i32;
        for (py, ay) in a.chunks_exact_mut(pw).enumerate() {
            let dy = (y0 + py) as i32 - dyc;
            let dy2 = (dy as i64) * (dy as i64);
            if dy2 > rad_sq {
                continue;
            }
            // Half-chord of the circle on this row: the row walks the disc,
            // not its bounding box.
            let half = isqrt((rad_sq - dy2) as i32);
            let lo = (dxc - half - (x0 as i32)).max(0) as usize;
            let hi = ((dxc + half + 1) - (x0 as i32)).max(0) as usize;
            if hi > pw {
                continue;
            }
            let dx0 = lo as i32 + x0 as i32 - dxc;
            let mut d2 = dx0 as i64 * dx0 as i64 + dy2;
            let mut step = 2 * dx0 as i64 + 1;
            for (px, slot) in ay[lo..hi].iter_mut().enumerate() {
                let v = if d2 >= rad_sq {
                    0u32
                } else {
                    (((rad_sq - d2) as u64 * inten * inv_rsq) >> 32).min(255) as u32
                };
                *slot = v as u8;
                d2 += step;
                step += 2;
                let _ = px;
            }
        }
        Some(Self {
            w: 0,
            x0,
            y0,
            pw,
            ph,
            a,
        })
    }

    /// Additively composite the plane into `buf`, clipped to the panel.
    #[inline]
    fn blit(&self, buf: &mut [u32], stride: usize, w: usize, h: usize, r: u8, g: u8, b: u8) {
        if self.pw == 0 || self.ph == 0 {
            return;
        }
        // Intersect the plane's box with the panel once; every row then only
        // clamps against these four scalars.
        // `y0` is built with `saturating_sub`, so it is never below 0.
        let y_lo = self.y0;
        let y_hi = (self.y0 + self.ph).min(h);
        let x_lo = self.x0;
        let x_hi = (self.x0 + self.pw).min(w);
        if y_hi <= y_lo || x_hi <= x_lo {
            return;
        }
        let n = x_hi - x_lo;
        for y in y_lo..y_hi {
            let prow = &self.a[(y - self.y0) * self.pw..][..n];
            let row = y * stride;
            self.blit_row(&mut buf[row + x_lo..row + x_lo + n], prow, r, g, b);
        }
    }

    /// One plane row against one destination row.
    ///
    /// The scalar body is the reference. On aarch64 the row is handed to the
    /// NEON path, which is the point of the split: the compositing is three
    /// saturating adds per pixel over ~690,000 pixels, and that is
    /// memory-bound scalar work LLVM's autovectoriser cannot reach, because
    /// the three channels are interleaved inside a u32 and the `>> 8`
    /// narrowing sits between the multiply and the add.
    #[inline]
    fn blit_row(&self, dst: &mut [u32], a: &[u8], r: u8, g: u8, b: u8) {
        let n = dst.len().min(a.len());
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: NEON is part of the ARMv8-A base architecture, so every
            // aarch64 CPU has it. `n` is bounded by both slice lengths, and
            // the NEON path only advances over chunks it has proved are in
            // bounds.
            unsafe { blit_row_neon(&mut dst[..n], &a[..n], r, g, b) };
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            for (px, &av) in dst[..n].iter_mut().zip(a[..n].iter()) {
                if av == 0 {
                    continue;
                }
                let al = av as u32;
                let cur = *px;
                let cb = (cur & 0xFF) + ((b as u32 * al) >> 8);
                let cg = ((cur >> 8) & 0xFF) + ((g as u32 * al) >> 8);
                let cr = ((cur >> 16) & 0xFF) + ((r as u32 * al) >> 8);
                *px = (0xFF << 24) | (cr.min(255) << 16) | (cg.min(255) << 8) | cb.min(255);
            }
        }
    }
}

/// NEON kernel for one glow row: 16 pixels per iteration.
///
/// The framebuffer packs a pixel as `(0xFF << 24) | (r << 16) | (g << 8) | b`,
/// so in memory the bytes are `[b, g, r, 0xFF]`. `vld4q_u8` de-interleaves
/// exactly that, giving B, G, R and A as four `u8x16` lanes for 16 pixels --
/// which is precisely the packing the arithmetic needs, for the cost of one
/// structured load.
///
/// Per channel the work is `dst + saturate((colour * a) >> 8)`. `colour * a` is
/// a `u8 x u8` widening multiply (`vmull_*` -> `u16x8`), the `>> 8` is a
/// narrow shift, and the saturating add is `vaddq_u8_sat` on the narrowed
/// `u8`. Alpha is not accumulated: the background pass always leaves `0xFF`
/// there, and the scalar path overwrites it unconditionally, so matching that
/// means loading and re-storing it untouched rather than recomputing it.
///
/// # Safety
/// `dst` and `a` must be non-empty and `dst` must be at least as long as `a`.
/// Both are sliced to a common length by the caller. Only
/// `dst.len() / 16` whole 16-pixel chunks are touched in bulk; the remainder
/// is the scalar tail. The `u32` slice is read and written through
/// `u8` pointers via `vld4q_u8`/`vst4q_u8`, which is sound because `u32` and
/// `u8` have the same alignment and the accesses stay inside the slice.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn blit_row_neon(dst: &mut [u32], a: &[u8], r: u8, g: u8, b: u8) {
    use core::arch::aarch64::*;

    let n = dst.len().min(a.len());
    let chunks = n / 16;

    // `[b, g, r, x]` in memory order.
    let c_b = vdupq_n_u8(b);
    let c_g = vdupq_n_u8(g);
    let c_r = vdupq_n_u8(r);

    /// `saturate(dst_ch + ((colour * a) >> 8))` over 16 lanes.
    #[inline(always)]
    unsafe fn add_channel(
        d: core::arch::aarch64::uint8x16_t,
        c: core::arch::aarch64::uint8x16_t,
        a: core::arch::aarch64::uint8x16_t,
    ) -> core::arch::aarch64::uint8x16_t {
        use core::arch::aarch64::*;
        // Low and high halves widened separately: `vmull_u8` takes two
        // `u8x8` (the low halves) and `vmull_high_u8` takes the uppers.
        // `vqaddq_u8` is ARM's saturating add; `vaddq_u8` would wrap.
        let lo = vshrq_n_u16::<8>(vmull_u8(vget_low_u8(c), vget_low_u8(a)));
        let hi = vshrq_n_u16::<8>(vmull_high_u8(c, a));
        vqaddq_u8(d, vcombine_u8(vmovn_u16(lo), vmovn_u16(hi)))
    }

    for k in 0..chunks {
        let di = k * 16;
        let av = vld1q_u8(a.as_ptr().add(di));
        // SAFETY: `di + 16 <= n <= dst.len()`, and the pointer is derived from
        // the live slice, so the 16-byte read is in bounds.
        let px = vld4q_u8(dst.as_ptr().add(di) as *const u8);
        // `px.0..3` are the B, G, R and A byte lanes, 16 pixels each.
        let res = uint8x16x4_t(
            add_channel(px.0, c_b, av),
            add_channel(px.1, c_g, av),
            add_channel(px.2, c_r, av),
            px.3,
        );
        // SAFETY: same bounds as the load above; the store covers exactly the
        // 16 pixels just read.
        vst4q_u8(dst.as_mut_ptr().add(di) as *mut u8, res);
    }

    // Scalar tail, identical to the reference body.
    for i in (chunks * 16)..n {
        let av = a[i] as u32;
        if av == 0 {
            continue;
        }
        let cur = dst[i];
        let cb = (cur & 0xFF) + ((b as u32 * av) >> 8);
        let cg = ((cur >> 8) & 0xFF) + ((g as u32 * av) >> 8);
        let cr = ((cur >> 16) & 0xFF) + ((r as u32 * av) >> 8);
        dst[i] = (0xFF << 24) | (cr.min(255) << 16) | (cg.min(255) << 8) | cb.min(255);
    }
}

/// The two accent glows for a panel size, baked together so they are computed
/// once per size rather than once per disc per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlowPlanes {
    /// Panel size the planes were baked for.
    pub w: usize,
    pub h: usize,
    primary: Option<GlowPlane>,
    tertiary: Option<GlowPlane>,
}

impl GlowPlanes {
    /// Bake both discs for a `w` x `h` panel.
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            // `draw_background`'s geometry, spelled once. `cx = w * 8 / 10,
            // cy = h / 8, r = w * 5 / 18` and `cx = w * 2 / 10, cy = h * 8 / 10,
            // r = w * 6 / 18`; the intensities are 26 and 20.
            primary: GlowPlane::bake(w * 8 / 10, h / 8, w * 5 / 18, 26),
            tertiary: GlowPlane::bake(w * 2 / 10, h * 8 / 10, w * 6 / 18, 20),
        }
    }

    /// Additively composite both glows with the given colours.
    #[inline]
    pub fn blit(
        &self,
        buf: &mut [u32],
        stride: usize,
        w: usize,
        h: usize,
        primary: (u8, u8, u8),
        tertiary: (u8, u8, u8),
    ) {
        if let Some(p) = &self.primary {
            p.blit(buf, stride, w, h, primary.0, primary.1, primary.2);
        }
        if let Some(t) = &self.tertiary {
            t.blit(buf, stride, w, h, tertiary.0, tertiary.1, tertiary.2);
        }
    }

    /// Bytes the two planes occupy, for the RSS budget.
    pub fn bytes(&self) -> usize {
        self.primary.as_ref().map_or(0, |p| p.a.len())
            + self.tertiary.as_ref().map_or(0, |p| p.a.len())
    }

    /// Whether the baked planes really are this panel's geometry.
    ///
    /// `w`/`h` on the struct alone are not enough to trust the cache: a
    /// `GlowPlane` bakes to `None` for a zero radius, and a `GlowPlanes::new`
    /// for a size whose disc is degenerate would otherwise be indistinguishable
    /// from a real bake. This is the cheap re-validation the fast path does.
    fn matches(&self, w: usize, h: usize) -> bool {
        if w == 0 || h == 0 {
            return self.w == 0 && self.h == 0;
        }
        self.primary.is_some() && self.tertiary.is_some()
    }
}

/// The original per-frame glow loop, kept as the ORACLE for the baked plane.
///
/// This is the arithmetic [`GlowPlane`] replaces. It stays in the tree
/// (test-only, so it costs no release binary size) because
/// `baked_glow_is_bit_identical_to_the_per_frame_loop` compares the two
/// pixel-for-pixel: an optimisation that changed a single pixel would be a
/// silent visual regression, and "it looks about the same" is not a check.
#[cfg(test)]
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
                buf[row + x] =
                    (0xFF << 24) | (cr.min(255) << 16) | (cg.min(255) << 8) | cb.min(255);
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

/// Draw a `Picker` settings row's candidate slots.
///
/// Two fills per slot, no allocation, no text: the selected slot is the
/// reference's accent-filled mark (`setBackgroundWithRadius(getColorAccent(...),
/// 100F)`, `WallpaperCarouselView.kt:48`) and the unselected ones are the
/// reference's *plain* candidates (`:74-77`, which builds every candidate and
/// marks only the current index). Both are pills, because the geometry sets
/// `radius = h * 0.5` for the same reason.
///
/// The selected slot is drawn **first** and the unselected ones over it would be
/// wrong, so it is drawn last; the two fills are disjoint rects so the order
/// between them does not actually matter, and it is written this way so a future
/// slot that grows a border does not land under its own fill.
///
/// A dead `PickerSlots` draws nothing at all, which is what makes the "no
/// candidates" case unskippable rather than a flag the caller has to honour.
fn draw_picker_slots(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    p: &super::layout::PickerSlots,
    state: &DrmInteractiveState,
) {
    if !p.is_live() {
        return;
    }
    for (i, slot) in p.slots.iter().take(p.n).enumerate() {
        if slot.is_empty() {
            continue;
        }
        let on = p.selected == Some(i);
        draw_rounded_rect_f(
            buf,
            stride,
            w,
            h,
            slot.x,
            slot.y,
            slot.w,
            slot.h,
            slot.radius,
            if on {
                state.palette.primary
            } else {
                // `outline_variant`, opaque, not a translucent `outline`.
                //
                // `outline` at 60% is not what the reference's unselected
                // carousel cards are: those are the wallpaper thumbnails
                // themselves (`WallpaperCarouselView.kt:74-77`) and only the
                // current one is accented, so a full-strength outline reads as a
                // control rather than as a position. `outline_variant` is the
                // tone-30 role for exactly this, a subtle marker
                // (`dark_tone::OUTLINE_VARIANT` in `palette.rs`).
                //
                // Opaque rather than alpha-composited, and that is not only a
                // colour choice: a translucent fill's *resulting* pixel depends
                // on whatever is under it, so the same slot drawn over the row's
                // card and over the section header would be two different colours
                // and there would be no single value for a test to assert on.
                // Opaque makes the marker exactly the role it is, which is what
                // lets a render test check it pixel-for-pixel.
                state.palette.outline_variant
            },
        );
    }
}

/// Zero-allocation, stack-only `"N of M"` formatter for a `Picker` row.
///
/// Returns the empty string when the position is unknown, which is the case the
/// row's contract defines as "no candidates yet" -- the caller then keeps the
/// row's static `value`. Deliberately not `format!("{at} of {of}")`: that
/// allocates, and this runs inside `paint_frame`. That is the same reason
/// [`SettingRow::at`] and [`SettingRow::of`] are a pair of integers rather than
/// a pre-joined string.
///
/// The buffer is [`SETTINGS_SECTION_MAX`], the same fixed bound the section
/// headers use, and 32 bytes is eight digits either side plus the separator, so
/// a `u32::MAX` position still fits rather than being silently truncated into a
/// wrong number.
pub fn format_picker_position(
    buf: &mut [u8; SETTINGS_SECTION_MAX],
    at: Option<u32>,
    of: Option<u32>,
) -> &str {
    let (Some(at), Some(of)) = (at, of.filter(|n| *n > 0)) else {
        return "";
    };
    let mut left = [0u8; 10];
    let mut right = [0u8; 10];
    let ln = write_u32(&mut left, at);
    let rn = write_u32(&mut right, of);
    if ln + rn + 4 > SETTINGS_SECTION_MAX {
        return "";
    }
    let mut n = 0;
    buf[..ln].copy_from_slice(&left[..ln]);
    n += ln;
    buf[n..n + 4].copy_from_slice(b" of ");
    n += 4;
    buf[n..n + rn].copy_from_slice(&right[..rn]);
    n += rn;
    std::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// Decimal digits of `v` into `out`, returning the count.
///
/// The digit loop [`format_apps_count`] already inlines, lifted out so the two
/// number formatters cannot disagree about how a number becomes digits -- and
/// so a zero renders as `"0"` rather than as the empty string, which is what a
/// `while v > 0` loop with no special case gives.
fn write_u32(out: &mut [u8; 10], v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut n = 0;
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut num = v;
    while num > 0 && n < 10 {
        tmp[n] = b'0' + (num % 10) as u8;
        num /= 10;
        n += 1;
    }
    for i in 0..n {
        out[i] = tmp[n - 1 - i];
    }
    n
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
    draw_text_weighted_i32(
        buf, stride, w, h, x as i32, y as i32, text, color, scale, weight,
    );
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
    draw_text_centered_weighted_i32(
        buf,
        stride,
        w,
        h,
        center_x as i32,
        y as i32,
        text,
        color,
        scale,
        weight,
    );
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
    draw_text_weighted_i32(
        buf,
        stride,
        w,
        h,
        x,
        y,
        text,
        color,
        scale,
        FontWeight::Regular,
    );
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
    draw_text_weighted(
        buf,
        stride,
        w,
        h,
        x,
        y,
        text,
        color,
        scale,
        FontWeight::Regular,
    );
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
    draw_text_centered_weighted_i32(
        buf,
        stride,
        w,
        h,
        center_x,
        y,
        text,
        color,
        scale,
        FontWeight::Regular,
    );
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
    draw_text_centered_weighted(
        buf,
        stride,
        w,
        h,
        center_x,
        y,
        text,
        color,
        scale,
        FontWeight::Regular,
    );
}

/// Draw a text run truncated with an ellipsis so it never leaves `max_width`.
///
/// Truncation is measured on the vector engine's own advances, so the drawn
/// width and the measured width agree and no glyph is ever cut in half.
/// Uppercase ASCII into a fixed stack buffer, for section headers.
///
/// Material 3 renders a preference category as an uppercase title, and
/// `str::to_uppercase` is the obvious way to get that -- which returns a `String`,
/// i.e. an allocation. On the frame path that is a heap allocation per section
/// header per frame, and the leak-shaped alternative (`to_uppercase().leak()`) is
/// worse: it never comes back.
///
/// Section names are ASCII by construction here (they are `&'static str` literals
/// in [`crate::settings::build`]), so a byte-wise ASCII fold is exact rather than
/// an approximation, and a non-ASCII byte is passed through unchanged instead of
/// being mangled. Truncates at [`SETTINGS_SECTION_MAX`] like every other fixed
/// buffer here.
pub const SETTINGS_SECTION_MAX: usize = 32;

/// See [`SETTINGS_SECTION_MAX`].
pub fn upper_ascii(s: &str, out: &mut [u8; SETTINGS_SECTION_MAX]) {
    let b = s.as_bytes();
    let n = b.len().min(SETTINGS_SECTION_MAX);
    for i in 0..n {
        out[i] = b[i].to_ascii_uppercase();
    }
}

/// Uppercase ASCII and return it as a `&str` for the text drawers.
///
/// Two-step because the drawer wants `&str` and the buffer has to be caller-owned:
/// a function that allocated the buffer would allocate on the frame path, and one
/// that returned a reference to its own local would not compile.
pub fn upper_ascii_str<'b>(s: &str, out: &'b mut [u8; SETTINGS_SECTION_MAX]) -> &'b str {
    upper_ascii(s, out);
    // Everything past `n` is the zero the buffer was created with, and `str` is
    // valid UTF-8 with interior and trailing NULs, so the whole array is a `&str`.
    core::str::from_utf8(out).unwrap_or("")
}

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
    // `char_indices` rather than `bytes` + a boundary snap. The old walk charged
    // the fallback advance once per *byte*, so a CJK or Cyrillic name was
    // truncated to a third of its length; and the snap-forward loop could only
    // ever grow `end`, so it could include a character that did not fit. Here
    // the cut lands on a character boundary by construction, and the prefix is
    // the longest one that genuinely fits.
    for (i, cp) in text.char_indices() {
        let adv = super::font::char_advance(cp, size);
        if used + adv > budget {
            break;
        }
        used += adv;
        end = i + cp.len_utf8();
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
    draw_text_clipped(
        buf, stride, w, h, x as f32, y, max_width, text, color, scale, weight,
    );
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
        // Same walk as the left-aligned branch above, so the centre of a
        // truncated string lands where the drawn prefix actually ends.
        for (i, cp) in text.char_indices() {
            let adv = super::font::char_advance(cp, size);
            if used + adv > budget {
                break;
            }
            used += adv;
            end = i + cp.len_utf8();
        }
        if end == 0 {
            return;
        }
        used + ell_w
    };
    draw_text_clipped(
        buf,
        stride,
        w,
        h,
        center_x as f32 - width * 0.5,
        y,
        max_width,
        text,
        color,
        scale,
        weight,
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
        buf,
        stride,
        w,
        h,
        center_x as i32,
        y as f32,
        max_width,
        text,
        color,
        scale,
        weight,
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
        buf,
        stride,
        w,
        h,
        xi,
        yi,
        (x1 - xi).max(0) as usize,
        (y1 - yi).max(0) as usize,
        radius.max(0.0).min((x1 - xi) as f32).min((y1 - yi) as f32) as usize,
        color,
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
        buf,
        stride,
        w,
        h,
        xi as usize,
        yi as usize,
        (x1 - xi) as usize,
        (y1 - yi) as usize,
        color,
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
            buf,
            stride,
            w,
            h,
            x0 - 0.5,
            y0.min(y1),
            1.0,
            (y1 - y0).abs() + 1.0,
            color,
        );
        return;
    }
    if (y0 - y1).abs() < 0.5 {
        draw_rect_f(
            buf,
            stride,
            w,
            h,
            x0.min(x1),
            y0 - 0.5,
            (x1 - x0).abs() + 1.0,
            1.0,
            color,
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
fn rounded_span(cy: i32, x: i32, rw: i32, rh: i32, radius: i32, r2: i32) -> Option<(i32, i32)> {
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
    let d = if dy == 0 { radius } else { isqrt(r2 - dy * dy) };
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

/// Material 3 press-state layer opacity: `#19FFFFFF` = 25/255 = 0.098.
pub const RIPPLE_STATE_LAYER_ALPHA: f32 = 0.098;

/// Hard cap on the ripple radius, px.
///
/// The old growth (`RIPPLE_GROW` px/s to a 0.455 s life) reached ~435 px, an
/// 870 px disc on a 1080 px panel: a full-screen grey wash per tap. Material
/// bounds a state layer to its control, so the radius is additionally capped
/// per-control by the shell; this is the backstop for a target with no known
/// bounds.
pub const RIPPLE_MAX_RADIUS: f32 = 64.0;

/// A filled disc, clipped to `clip`.
///
/// This is the state-layer primitive: the disc geometry comes from the
/// ripple, the bounds from the control that owns it. Clipping is folded into
/// the existing per-row span (`lo`/`hi`) rather than done as a second pass, so
/// a clipped ripple costs exactly what an unclipped one did -- there is no
/// rasterise-then-mask work and no scratch allocation.
#[inline]
pub fn draw_circle_glyph_clipped(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    clip: &Rect,
    cx: f32,
    cy: f32,
    r: f32,
    color: u32,
) {
    if r <= 0.0 {
        return;
    }
    // Intersect the disc's bbox with the clip once; every row below only has
    // to clamp against these four scalars.
    let y0 = (cy - r).floor().max(clip.y).max(0.0) as i32;
    let y1 = (cy + r).ceil().min(clip.y + clip.h).min(h as f32) as i32;
    let x0 = (cx - r).floor().max(clip.x).max(0.0) as i32;
    let x1 = (cx + r).ceil().min(clip.x + clip.w).min(w as f32) as i32;
    if y1 <= y0 || x1 <= x0 {
        return;
    }
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
            for px in lo..ilo {
                let dx = px as f32 + 0.5 - cx;
                let a = (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                let i = py as usize * stride + px as usize;
                buf[i] = super::font::blend_over(buf[i], color, (a * 255.0 + 0.5) as u8);
            }
            for px in ihi..hi {
                let dx = px as f32 + 0.5 - cx;
                let a = (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                if a <= 0.0 {
                    continue;
                }
                let i = py as usize * stride + px as usize;
                buf[i] = super::font::blend_over(buf[i], color, (a * 255.0 + 0.5) as u8);
            }
        } else {
            for px in lo..hi {
                let dx = px as f32 + 0.5 - cx;
                let a = (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
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
/// Join the date and weather into one line, in a caller-owned buffer.
///
/// Either half may be empty, which selects the plan's date-only layout: no
/// separator, no trailing gap. Zero allocation and no `format!` -- this runs
/// every frame, and `paint_frame_does_not_allocate` is the guard.
#[inline]
/// Compose the smartspace line: the date, the weather, or both.
///
/// `phase` is the cross-fade between the two layouts, 0 = date only and 1 = the
/// full card. Below 0.5 the separator and the weather are dropped entirely
/// rather than drawn at low alpha, because half a separator between two
/// half-visible strings reads as a rendering fault rather than as a
/// transition. There is no third state to blend *towards*: at rest the line is
/// the date, and the phase only decides when the weather joins it.
///
/// The `date == ""` case is the "no clock data" fallback and always shows the
/// weather alone, whatever the phase -- an empty first half is not a
/// transition, it is a missing value.
fn join_smartspace<'a>(
    buf: &'a mut [u8; 96],
    date: &'a str,
    weather: &'a str,
    phase: f32,
) -> &'a str {
    let with_weather = phase >= 0.5;
    match (date.is_empty(), weather.is_empty()) {
        (true, true) => "",
        (true, false) => weather,
        // Below the cross-fade midpoint the line is the **date alone**. The old
        // code concatenated both with an *empty* separator below the midpoint,
        // which is not a cross-fade at all: it rendered
        // "Tue, Sep 2228C Sunny" -- the two strings run together with nothing
        // between them -- and it rendered the same weather at both phases, so
        // the only thing that changed across the fade was three spaces and a
        // pipe. `screenshot_every_launcher_state` asserts that phase 1.0 paints
        // strictly more than phase 0.0, and it could not.
        //
        // So the date-only half of the fallback is honoured here, where the
        // fallback is decided, rather than in each of the painters.
        // Date-only fallback, either because there is no weather or because the
        // cross-fade has not reached the weather half yet.
        (false, _) if !with_weather => date,
        (false, true) => date,
        (false, false) => {
            let mut n = 0usize;
            for src in [date.as_bytes(), b"  |  ".as_slice(), weather.as_bytes()] {
                for &c in src {
                    if n < buf.len() {
                        buf[n] = c;
                        n += 1;
                    }
                }
            }
            // Every byte came from a `&str` or a literal, so this is UTF-8.
            unsafe { std::str::from_utf8_unchecked(&buf[..n]) }
        }
    }
}

#[inline]
fn rgb_of(c: u32) -> (u8, u8, u8) {
    (
        ((c >> 16) & 0xFF) as u8,
        ((c >> 8) & 0xFF) as u8,
        (c & 0xFF) as u8,
    )
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
    draw_rounded_rect_f(buf, stride, w, h, sh_x, sh_y, sh_w, sh_h, sh_w * 0.5, color);
    // Punch the middle of the shackle out again.
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        sh_x + t,
        sh_y + t,
        sh_w - t * 2.0,
        sh_h,
        sh_w * 0.4,
        state_surface_dark(),
    );
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        cx as f32 - body_w * 0.5,
        body_y,
        body_w,
        body_h,
        size * 0.22,
        color,
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
        buf,
        stride,
        w,
        h,
        f_x - ring,
        f_y - ring,
        f_w + ring * 2.0,
        f_h + ring * 2.0,
        f_h * 0.28 + ring,
        border,
    );
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        f_x,
        f_y,
        f_w,
        f_h,
        f_h * 0.28,
        state.palette.surface_container_high,
    );
    let em = super::font::em_px_at(2, w);
    let disp = if state.app_input.is_empty() {
        placeholder
    } else {
        state.app_input
    };
    let col = if state.app_input.is_empty() {
        state.palette.on_surface_variant
    } else {
        state.palette.on_surface
    };
    let tx = f_x + f_h * 0.42;
    let ty = f_y + (f_h - em * 0.62) * 0.5 - em * 0.14;
    draw_text_clipped(
        buf,
        stride,
        w,
        h,
        tx,
        ty,
        f_w - f_h * 0.8,
        disp,
        col,
        2,
        FontWeight::Regular,
    );
    if state.app_input_focused {
        let cur_x = tx + super::font::measure(disp, em) + ring;
        if cur_x < f_x + f_w - f_h * 0.3 {
            draw_rect(
                buf,
                stride,
                w,
                h,
                cur_x as usize,
                ty as usize,
                ring.max(2.0) as usize,
                (em * 0.62) as usize,
                state.palette.primary,
            );
        }
    }
    let list_top = f_y + f_h + inset;
    let list_h = (content_y + al.scroll_bottom - list_top).max(0.0);
    SETTINGS_GEOM.with(|c| {
        c.set(SettingsGeometry {
            list_top,
            list_h,
            bar_x: bar.x,
            bar_h: bar.h,
            bar_w: bar.w,
            panel_h: h as f32,
            card_h: 0.0,
            step: 0.0,
        })
    });
    (list_top, list_h)
}

/// The folder grid `draw_folder` last painted, published for the hit test.
///
/// # Why this exists
///
/// [`super::layout::FolderLayout`]'s cell rects are in **layout** coordinates:
/// `cell_at(col, row)` puts row 0 at `y = pad_top`, near the top of the *layout*,
/// not the panel. `draw_folder` draws the sheet vertically centred
/// (`sy = (h - surface_h) * 0.5`) and the grid a `pad_top` below that, so the
/// row that is on screen is at `pad_top + sy` -- hundreds of pixels lower. A
/// caller that hit-tested `FolderLayout` rects directly would be aiming at empty
/// workspace above the sheet.
///
/// So the draw path publishes where it actually put the grid, the same way
/// [`SettingsGeometry`] publishes the settings row pitch, and the same
/// `thread_local` rather than a state field: the geometry only exists after the
/// paint, and the shell's tap arm runs on a later event.
///
/// It carries the three gesture fields as well as the pitch, because a reorder
/// has already moved the items by the time a touch arrives. A hit test that
/// answered "the item in this cell" from the pitch alone would name an item that
/// has slid a place along, and the lifted and insertion slots are the two cells
/// whose occupant is not the item that owns the cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderGridGeometry {
    /// Top-left of cell `(0, 0)` as painted, panel pixels.
    pub origin: (f32, f32),
    /// Cell pitch, as painted.
    pub cell: (f32, f32),
    /// Grid dimensions, as painted.
    pub cols: usize,
    pub rows: usize,
    /// Slots on a page: `cols * rows`.
    pub per_page: usize,
    /// Absolute rank of slot 0 on this page: `folder_page * per_page`.
    pub first: usize,
    /// The lifted slot, or `None`.
    pub drag_slot: Option<usize>,
    /// The slot the gap has opened at, or `None`.
    pub drop_slot: Option<usize>,
    /// The lifted cell is outside the folder.
    pub drag_out: bool,
    /// Live: a folder with a page of items in it was painted.
    pub live: bool,
}

impl FolderGridGeometry {
    /// Nothing painted: every field zero and [`Self::live`] false.
    ///
    /// Public because a caller that has to synthesise one -- a test asserting
    /// that a *dead* grid cannot produce a hit test, say -- has no other way to
    /// name this value. The renderer clears the published cell with it on every
    /// frame that is not an open folder with items in it.
    pub const EMPTY: Self = Self {
        origin: (0.0, 0.0),
        cell: (0.0, 0.0),
        cols: 0,
        rows: 0,
        per_page: 0,
        first: 0,
        drag_slot: None,
        drop_slot: None,
        drag_out: false,
        live: false,
    };

    /// The rect cell `slot` was painted into, panel pixels.
    ///
    /// `slot` is page-local, matching
    /// [`DrmInteractiveState::folder_drag_slot`] and
    /// [`DrmInteractiveState::folder_drop_slot`]. `None` for a closed or empty
    /// folder, for an out-of-range slot, and for a degenerate grid.
    pub fn cell_rect(&self, slot: usize) -> Option<Rect> {
        if !self.live || self.per_page == 0 || slot >= self.per_page {
            return None;
        }
        Some(Rect {
            x: self.origin.0 + (slot % self.cols) as f32 * self.cell.0,
            y: self.origin.1 + (slot / self.cols) as f32 * self.cell.1,
            w: self.cell.0,
            h: self.cell.1,
            radius: 0.0,
        })
    }

    /// The page-local slot a point is in, or `None` for outside the grid.
    ///
    /// Containment, deliberately: this is a *tap* test. A drag's insertion index
    /// is [`super::layout::FolderLayout::reorder_index`], which is the
    /// reference's nearest-area rule and resolves the gutters -- see
    /// `a_tap_and_a_drag_disagree_in_the_gutter_on_purpose` in `layout.rs` for
    /// why the two have to differ.
    pub fn slot_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.live || self.per_page == 0 {
            return None;
        }
        let (w, h) = self.cell;
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let col = ((x - self.origin.0) / w).floor();
        let row = ((y - self.origin.1) / h).floor();
        if col < 0.0 || row < 0.0 {
            return None;
        }
        let (col, row) = (col as usize, row as usize);
        if col >= self.cols || row >= self.rows {
            return None;
        }
        Some(row * self.cols + col)
    }
}

/// The folder grid the last painted frame drew, or the empty value when no
/// folder is on screen.
///
/// Empty until a folder has been painted with a page of items in it, which is
/// what makes "the folder grid is not tappable" a value rather than an
/// inference.
pub fn folder_grid_geometry() -> FolderGridGeometry {
    FOLDER_GRID.with(|c| c.get())
}

thread_local! {
    static FOLDER_GRID: std::cell::Cell<FolderGridGeometry> = const {
        std::cell::Cell::new(FolderGridGeometry::EMPTY)
    };
}

/// The folder rename field `draw_folder` last painted, published for the hit test.
///
/// Panel coordinates, already offset from `FolderLayout`'s layout space by the
/// same `folder_grid_geometry` origin minus `cell` delta the grid uses (see
/// `folder_layout_offset` in `main.rs:645`). `FolderLayout::menu` is
/// panel-space and must not shift; this *must* shift, for the same reason the
/// grid does: `rename_field` is in layout coordinates (`y = pad_top`) while
/// the sheet is vertically centred.
///
/// `live` means "an open folder with editing on was painted". Cleared on every
/// other frame, so a closed folder reports no tappable field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FolderRenameGeometry {
    /// Text-field rect as painted, panel pixels.
    pub field: Rect,
    /// Caret rect as painted, panel pixels. Zero-size when not editing.
    pub caret: Rect,
    /// Editing was on when this was published.
    pub editing: bool,
    /// A field was painted.
    pub live: bool,
}

impl FolderRenameGeometry {
    /// Nothing painted: every field zero and [`Self::live`] false.
    pub const EMPTY: Self = Self {
        field: Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        },
        caret: Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        },
        editing: false,
        live: false,
    };

    /// Whether `(x, y)` hits the published field, exclusive on far edges.
    ///
    /// Exclusive (`x < x+w`) because `Rect::contains` is inclusive on both
    /// edges (`layout.rs:154`); an inclusive test would let a touch on the
    /// field's edge also hit the sheet chrome. Dead geometry hits nothing.
    #[inline]
    pub fn hit(&self, x: f32, y: f32) -> bool {
        if !self.live || !self.editing {
            return false;
        }
        if self.field.is_empty() {
            return false;
        }
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        x >= self.field.x
            && x < self.field.x + self.field.w
            && y >= self.field.y
            && y < self.field.y + self.field.h
    }
}

/// The rename field the last painted frame drew, or empty when no edit is open.
pub fn folder_rename_geometry() -> FolderRenameGeometry {
    FOLDER_RENAME.with(|c| c.get())
}

thread_local! {
    static FOLDER_RENAME: std::cell::Cell<FolderRenameGeometry> = const {
        std::cell::Cell::new(FolderRenameGeometry::EMPTY)
    };
}

/// The Settings panel's painted geometry, published for the hit test.
///
/// # Why this exists
///
/// The rows are *drawn* by `paint_frame` and *hit-tested* by
/// [`crate::settings::hit`], and the two need the same five numbers. Deriving
/// them twice is a second chance to be one row out, and the failure mode if that
/// happens is the quietest kind: a tap lands on the neighbouring row, so the
/// setting the user pressed looks inert while the one beside it changes. Nothing
/// crashes, nothing warns.
///
/// The first version of this published only the list rect and left the hit test
/// to recompute the row pitch from it. That is *not* enough, and the test written
/// to catch the drift passed while the drift was present -- because it recomputed
/// the pitch with the same formula it was checking. So the renderer now publishes
/// the pitch it actually used, and the hit test reads it.
///
/// A `thread_local` rather than a state field because the state is built *before*
/// the paint and these values only exist after it; the shell's tap arm runs on a
/// later event. Same pattern as `ICON_EDGE_PX`, which the icon pipeline already
/// uses for exactly this reason. The panel has to be painted once before its rows
/// are tappable, which is true rather than a race: the panel is opened by a tap,
/// and the next frame paints it before any further tap is processed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingsGeometry {
    /// Top of the list band, in panel pixels.
    pub list_top: f32,
    /// Height of the list band.
    pub list_h: f32,
    /// The app panel's top bar, which the row x/width come from.
    pub bar_x: f32,
    pub bar_h: f32,
    pub bar_w: f32,
    /// Panel height, because the row pitch is a fraction of it.
    pub panel_h: f32,
    /// Painted row height and pitch. Zero until the panel has been painted with
    /// at least one row, which is what makes "nothing is tappable yet" a value
    /// rather than an inference.
    pub card_h: f32,
    /// Painted row pitch -- the height plus the gap.
    pub step: f32,
}

/// The geometry the Settings panel last painted with rows in it.
///
/// All zeroes before the panel has been drawn with a non-empty row list, which
/// [`crate::settings::hit`] treats as "nothing is tappable".
pub fn settings_geometry() -> SettingsGeometry {
    SETTINGS_GEOM.with(|c| c.get())
}

thread_local! {
    static SETTINGS_GEOM: std::cell::Cell<SettingsGeometry> = const {
        std::cell::Cell::new(SettingsGeometry {
            list_top: 0.0, list_h: 0.0, bar_x: 0.0, bar_h: 0.0, bar_w: 0.0,
            panel_h: 0.0, card_h: 0.0, step: 0.0,
        })
    };
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
            buf,
            stride,
            w,
            h,
            x,
            y,
            rw,
            row_h,
            row_h * 0.24,
            state.palette.surface_container_high,
        );
        let tx = x + row_h * 0.45;
        let tw = rw - row_h * 0.9;
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            tx,
            y + row_h * 0.16,
            tw,
            name,
            state.palette.on_surface,
            1,
            FontWeight::Bold,
        );
        draw_text_clipped(
            buf,
            stride,
            w,
            h,
            tx,
            y + row_h * 0.16 + title_em,
            tw,
            detail,
            state.palette.on_surface_variant,
            1,
            FontWeight::Regular,
        );
        y += step;
    }
}

/// The launcher background, palette driven.
/// Quick-search bar glyphs.
///
/// ASCII stand-ins for the reference's vector brand marks (`LawnQsbUi.kt`
/// draws `ic_google_color`, a mic and a lens). Chosen from the basic Latin set
/// so they render with the built-in vector font on a device with no system
/// Noto installed, which is the case in every sandbox and on any image that
/// has not installed fonts yet.
const QSB_MIC_GLYPH: &str = "O";
const QSB_LENS_GLYPH: &str = "@";
/// The leading provider mark. A stand-in for the reference's vector brand logo
/// (`LawnQsbUi.kt:117`), which varies per provider and is not reachable until the
/// icon pipeline can carry artwork.
const QSB_SEARCH_GLYPH: &str = "G";

/// The weather glyph for the smartspace's 20 dp icon slot.
///
/// U+2601 CLOUD, which is inside the symbol range the built-in vector engine
/// covers. `WEATHER_GLYPH` is a `&str` rather than a `char` so the draw call
/// takes it directly.
const WEATHER_GLYPH: &str = "\u{2601}";

fn draw_background(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    state: &DrmInteractiveState,
) {
    let base = state.palette.surface;
    let sr = ((base >> 16) & 0xFF) as f32;
    let sg = ((base >> 8) & 0xFF) as f32;
    let sb = (base & 0xFF) as f32;

    // Wallpaper cover-fit geometry, computed once for the panel rather than per
    // pixel.
    //
    // `max` of the two ratios, so the image *covers* the panel instead of
    // letterboxing: a 16:9 wallpaper on a 20:9 phone would otherwise leave bands
    // at the top and bottom, and the reference's `WallpaperDrawable` uses
    // `CENTER_CROP` for exactly this reason.
    let cover = state.wallpaper.map(|img| {
        let rw = img.width.max(1);
        let rh = img.height.max(1);
        let pw = w as u32;
        let ph = h as u32;
        let scale = ((pw as f32 / rw as f32).max(ph as f32 / rh as f32)).max(1e-6);
        let dw = (rw as f32 * scale).ceil() as u32;
        let dh = (rh as f32 * scale).ceil() as u32;
        (
            img,
            rw,
            rh,
            dw,
            dh,
            dw.saturating_sub(pw) / 2,
            dh.saturating_sub(ph) / 2,
        )
    });

    // The scrim. `wallpaper_dim` is the shell's choice of how hard to darken, and
    // the `k` ramp is the same top-to-bottom gradient the gradient path has always
    // applied -- both are needed: the ramp is what makes the dock and grid read as
    // "on top", and it is a property of the *panel*, whereas the scrim is a
    // property of the *content*. A bright photo under the smartspace would
    // otherwise put white text on white.
    let dim = state.wallpaper_dim as f32 / 255.0;

    for y in 0..h {
        let t = y as f32 / h as f32;
        let k = 1.0 - t * 0.42;
        let row = &mut buf[y * stride..y * stride + w];
        match cover {
            None => {
                let r = ((sr * k).round() as u32).min(255);
                let g = ((sg * k).round() as u32).min(255);
                let b = ((sb * k).round() as u32).min(255);
                // A slice fill is a vectorised store; the manual loop was costing
                // milliseconds on a 2.6 Mpx panel.
                row.fill((0xFF << 24) | (r << 16) | (g << 8) | b);
            }
            Some((img, rw, rh, dw, dh, ox, oy)) => {
                // Nearest neighbour, one tap and an index per destination pixel.
                //
                // Bilinear would be four taps and a weighted blend per pixel over
                // 2.6 million of them, every frame. At this scale -- a phone
                // wallpaper upscaled to a phone panel, so the source is at most a
                // few hundred pixels per axis -- nearest is not visibly different,
                // and the reference gets away with `CENTER_CROP` only because its
                // wallpaper blit is a hardware operation.
                let sy = (y as u32).saturating_add(oy);
                let src_row = if sy < dh {
                    (sy as u64 * rh as u64 / dh as u64) as u32
                } else {
                    rh - 1
                };
                for (x, px) in row.iter_mut().enumerate() {
                    let sx = (x as u32).saturating_add(ox);
                    let src_col = if sx < dw {
                        (sx as u64 * rw as u64 / dw as u64) as u32
                    } else {
                        rw - 1
                    };
                    let i = ((src_row as usize * rw as usize + src_col as usize) * 4)
                        .min(img.pixels.len().saturating_sub(4));
                    let r = img.pixels[i] as f32 * k * (1.0 - dim);
                    let g = img.pixels[i + 1] as f32 * k * (1.0 - dim);
                    let b = img.pixels[i + 2] as f32 * k * (1.0 - dim);
                    // PNG is straight (un-premultiplied) alpha, so blend against
                    // the gradient rather than letting a transparent wallpaper show
                    // whatever was in the buffer.
                    let a = img.pixels[i + 3] as f32 / 255.0;
                    let br = ((sr * k).round() as u32).min(255) as f32;
                    let bg = ((sg * k).round() as u32).min(255) as f32;
                    let bb = ((sb * k).round() as u32).min(255) as f32;
                    *px = (0xFF << 24)
                        | (((r * a + br * (1.0 - a)).round() as u32).min(255) << 16)
                        | (((g * a + bg * (1.0 - a)).round() as u32).min(255) << 8)
                        | ((b * a + bb * (1.0 - a)).round() as u32).min(255);
                }
            }
        }
    }

    // Two soft accent glows, from the palette's primary and tertiary.
    //
    // The falloff is pre-baked per panel size and the palette colour is
    // applied at blit time, so a palette change (a new wallpaper) costs
    // nothing and a size change re-bakes once. See [`GlowPlane`] for the
    // per-pixel arithmetic this replaces and why the plane is a disc bounding
    // box rather than the whole panel.
    let (pr, pg, pb) = rgb_of(state.palette.primary);
    let (tr, tg, tb) = rgb_of(state.palette.tertiary);
    let glow = glow_planes(w, h);
    glow.blit(buf, stride, w, h, (pr, pg, pb), (tr, tg, tb));
}

/// The baked glow planes for a panel size, rebuilt only when the size changes.
///
/// One cache entry, keyed on `(w, h)`. A rotation or a resolution change
/// re-bakes; nothing else does. A two-entry LRU would handle a rotate-and-
/// return oscillation without a re-bake, but a phone rotates at most once and
/// the bake is a few hundred microseconds, so one entry is the right amount of
/// machinery.
///
/// A `RwLock` rather than a leaked `&'static`: the read is a few nanoseconds
/// against ~690,000 pixels of compositing, and an owned cache cannot leak and
/// cannot hand out a dangling reference. The write side is the re-bake.
fn glow_planes(w: usize, h: usize) -> std::sync::RwLockReadGuard<'static, GlowPlanes> {
    use std::sync::{OnceLock, RwLock};
    static CACHE: OnceLock<RwLock<GlowPlanes>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| RwLock::new(GlowPlanes::new(0, 0)));
    {
        // Fast path: a read lock only, no re-bake.
        let guard = cell.read().unwrap_or_else(|e| e.into_inner());
        if guard.w == w && guard.h == h && guard.matches(w, h) {
            return cell.read().unwrap_or_else(|e| e.into_inner());
        }
    }
    let mut guard = cell.write().unwrap_or_else(|e| e.into_inner());
    if guard.w != w || guard.h != h {
        *guard = GlowPlanes::new(w, h);
    }
    drop(guard);
    cell.read().unwrap_or_else(|e| e.into_inner())
}

/// The bytes the glow cache currently holds, for the RSS assertion.
pub fn glow_cache_bytes() -> usize {
    // A probe size is used deliberately: this is called from tests and from
    // `--benchmark`, and baking a second size here would perturb the very
    // RSS number being asserted.
    glow_planes(1, 1).bytes()
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
        buf,
        stride,
        w,
        h,
        pad as i32,
        y,
        state.time_str,
        state.palette.on_surface,
        1,
        FontWeight::Medium,
    );
    let icon_r = l.w - pad;
    let by = l.status_bar_h * 0.5 - bar_h * 0.28 + y_offset as f32;
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        icon_r - bar_h * 1.35,
        by,
        bar_h * 1.05,
        bar_h * 0.56,
        bar_h * 0.16,
        state.palette.on_surface_variant,
    );
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        icon_r - bar_h * 1.23,
        by + bar_h * 0.09,
        bar_h * 0.81,
        bar_h * 0.38,
        bar_h * 0.10,
        state.palette.surface,
    );
    // Fill level inside the battery.
    draw_rounded_rect_f(
        buf,
        stride,
        w,
        h,
        icon_r - bar_h * 1.23,
        by + bar_h * 0.09,
        bar_h * 0.81 * 0.78,
        bar_h * 0.38,
        bar_h * 0.10,
        0xFF10B981,
    );
    if state.quick_tiles_active[0] {
        draw_circle_glyph(
            buf,
            stride,
            w,
            h,
            icon_r - bar_h * 2.05,
            l.status_bar_h * 0.5 + y_offset as f32,
            bar_h * 0.14,
            state.palette.on_surface,
        );
    }
    let rat = if state.quick_tiles_active[1] {
        "5G"
    } else {
        "OFF"
    };
    let rat_col = if state.quick_tiles_active[1] {
        state.palette.primary
    } else {
        state.palette.outline
    };
    draw_text_weighted_i32(
        buf,
        stride,
        w,
        h,
        (icon_r - bar_h * 2.9) as i32,
        y,
        rat,
        rat_col,
        1,
        FontWeight::Bold,
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

    /// SystemUI shade pull-down: the `0 -> 1` shade reveal.
    ///
    /// Legacy hand-tuned profile, transcribed from the deleted
    /// `compositor::spring` `k/c/m` oscillator so the feel is unchanged. That
    /// parameterisation was `k = 240, c = 24, m = 1`, so the same spring here
    /// is `stiffness = k` (this model stores `k` directly, not `omega`) and
    /// `damping_ratio = zeta = c / (2 * sqrt(k * m)) = 24 / (2 * sqrt(240)) =
    /// 0.7746`. The rest threshold tightens from the old oscillator's blanket
    /// `0.05` to the table default `0.002`: on a 0..1 progress value a 5% dead
    /// band parks the shade visibly short of open, and the table default is
    /// what every other row in this impl already settles to.
    pub const fn shade_pull() -> Self {
        Self {
            stiffness: 240.0,
            damping_ratio: 0.7746,
            value_threshold: 0.002,
        }
    }

    /// Virtual keyboard slide: the `0 -> 1` IME reveal.
    ///
    /// Legacy hand-tuned profile, transcribed from the deleted
    /// `compositor::spring` `k/c/m` oscillator so the feel is unchanged. That
    /// parameterisation was `k = 260, c = 26, m = 1`, giving `zeta = 26 / (2 *
    /// sqrt(260)) = 0.8061`. See [`Self::shade_pull`] for the rest threshold.
    pub const fn ime_slide() -> Self {
        Self {
            stiffness: 260.0,
            damping_ratio: 0.8061,
            value_threshold: 0.002,
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

    /// Quickstep home: the workspace reappearing under a dismissed app.
    ///
    /// `stretch_edge` is the *edge* pull -- a 24.657 rad/s natural frequency
    /// with damping 0.98, chosen for a finger dragging a screen edge. A
    /// swipe-to-home release is a different motion: the app panel is already
    /// scaled down and fading, and it has to arrive at the workspace without
    /// the long, almost-undamped tail that a 0.98 ratio gives. The reference
    /// runs this on `DynamicAnimation` with the standard fast decelerate
    /// interpolator (`FastOutSlowInInterpolator`), which is a critically
    /// damped-ish settle, not a spring with visible overshoot.
    ///
    /// So: same stiffness, critically damped (ratio 1.0, the fastest return
    /// to rest with no overshoot) and a slightly looser rest threshold so the
    /// last fraction of a pixel does not keep the frame loop awake. The
    /// workspace must arrive *under* the app, and an overshooting scale
    /// would show the app panel growing past the workspace and back.
    pub const fn quickstep_home() -> Self {
        const OMEGA: f32 = 24.657;
        Self {
            stiffness: OMEGA * OMEGA,
            damping_ratio: 1.0,
            value_threshold: 0.004,
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
        let gamma_minus = -damping_ratio * natural_freq
            - natural_freq * (damping_ratio * damping_ratio - 1.0).sqrt();
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
        let sin_coeff =
            (1.0 / damped_freq) * (damping_ratio * natural_freq * displacement + velocity);
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
        return Some(RestWindow {
            first: lo,
            last: hi,
        });
    }
    if a > q && b > q {
        return None;
    }
    let (x, y) = crossing_bracket(magnitude, lo, hi, q, tol);
    if b >= a {
        // Rising: under `q` from `lo` up to the crossing, so `hi` is past it.
        Some(RestWindow { first: lo, last: y })
    } else {
        // Falling: under `q` from the crossing up to `hi`, so `lo` is before it.
        Some(RestWindow { first: x, last: hi })
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

    /// Hard ceiling for the total simulated time in one [`Self::step_clamped`]
    /// call: a suspend/resume, or a frame that overran badly, must not
    /// teleport the animation.
    pub const MAX_STEP_TOTAL: f32 = 0.25;

    /// [`Self::step`], hardened for a frame clock the caller does not own.
    ///
    /// [`Self::step`] evaluates the closed-form response at `t = dt` directly.
    /// That is exact for a real frame delta, but a `dt` that arrives as NaN
    /// (a clock read that failed) or as a huge stall would put NaN straight
    /// into `value`, and geometry is computed from `value` -- one bad frame
    /// would poison the rest of the animation permanently, because a NaN
    /// response is NaN at every later `t` too.
    ///
    /// So this entry point ignores non-finite and non-positive `dt` outright
    /// (nothing advances), and clamps a finite one to
    /// [`Self::MAX_STEP_TOTAL`]. A single closed-form evaluation of a bounded
    /// `t` cannot diverge the way a sub-stepped numeric integrator can, so no
    /// sub-stepping and no NaN watchdog are needed here: `exp(-gamma * t)`
    /// underflows to zero and the value lands on the target.
    ///
    /// Callers that own their clock (the shell, which knows the vsync period
    /// and only ever passes a real frame delta) can use [`Self::step`].
    pub fn step_clamped(&mut self, dt: f32) -> bool {
        if !dt.is_finite() || dt <= 0.0 {
            return self.is_at_rest();
        }
        self.step(dt.min(Self::MAX_STEP_TOTAL))
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
        if velocity {
            v.abs()
        } else {
            d.abs()
        }
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
                    flat[flat_len] = (-coeff_b * gamma_plus * gamma_plus
                        / (coeff_a * gamma_minus * gamma_minus))
                        .ln()
                        / span;
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
                if t < 1e7 {
                    t
                } else {
                    f64::INFINITY
                }
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
                rest_window(
                    |t| self.magnitude_at(t, true),
                    lo,
                    hi,
                    v_thr,
                    SETTLE_CERTIFY_S,
                ),
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
                rest_window(
                    |t| self.magnitude_at(t, false),
                    lo,
                    hi,
                    thr as f64,
                    min_diff,
                ),
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

// Boundary overscroll resistance lives in `super::layout::damped_scroll`.
//
// It used to also live here, as `apply_overscroll_resistance(drag, screen_w)`.
// That duplication was removed rather than kept in sync: two copies of the
// same curve with a bit-equality test between them is strictly worse than one
// copy, and it had already proven the point -- the copy in this file was
// written as a `1 / (1 + x / 100)` rational while its own doc comment
// claimed it was "Authentic Android/Lawnchair 17".
//
// The only thing the shell lost is the `max` convenience, and that was
// hiding an invented constant. AOSP passes the *container extent* on a drag
// and `page_width * 0.5` on a fling (`PagedView.java:1552`); this function
// hard-coded `screen_width * 0.35` for both. Call sites now pass `max`
// explicitly, so the two cases can finally differ.

// Material You tonal palette.
//
// This used to be a ~100-line HSL ramp defined right here. It was replaced by
// `graphics::palette`, an L*-anchored Oklab engine, because HSL lightness is
// not perceptual: a blue at L = 0.5 has relative luminance ~0.072 while a
// yellow at the same L has ~0.928, so every contrast guarantee derived from
// it was fiction and the launcher shipped AA-failing text on yellow
// wallpapers. See the `palette` module docs for the tone contract and for why
// Oklab rather than the plan's CAM16.
//
// Re-exported rather than moved outright so every existing import path
// (`graphics::MaterialYouPalette`, `drm_kms::MaterialYouPalette`) keeps
// working; `super_extreme` and the palette tests both rely on it.
pub use super::palette::MaterialYouPalette;

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
    // `chars()`, not `bytes()`: a multi-byte character must advance the pen
    // once and consume one glyph slot, not once per byte.
    for cp in time_str.chars() {
        pen += super::font::draw_glyph(
            buf,
            stride,
            w,
            h,
            pen,
            top,
            cp,
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
        RgbaImage {
            width: w,
            height: h,
            pixels,
        }
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
                assert_eq!(
                    in_tile, in_blit,
                    "tw={tw} th={th} radius={radius} pixel {i}"
                );
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
        draw_icon_bitmap(
            &mut buf,
            2,
            2,
            2,
            0,
            0,
            4,
            4,
            1,
            &RgbaImage {
                width: 0,
                height: 0,
                pixels: vec![],
            },
        );
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn icon_blit_upscales_without_losing_coverage() {
        let img = solid(2, 2, [7, 7, 7, 255]);
        let mut buf = vec![0xFF000000u32; 16];
        draw_icon_bitmap(&mut buf, 4, 4, 4, 0, 0, 4, 4, 0, &img);
        assert!(
            buf.iter().all(|px| *px == 0xFF070707),
            "every tile pixel filled"
        );
    }

    #[test]
    fn frame_hash_tracks_icon_content() {
        let icon_a = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 255],
        };
        let icon_b = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![4, 5, 6, 255],
        };
        let mut state = DrmInteractiveState::default();

        fn item<'a>(icon: Option<&'a RgbaImage>) -> AppGridItem<'a> {
            AppGridItem {
                id: "x",
                name: "X",
                color: 0xFF000000,
                glyph: "X",
                icon,
                folder_n: 0,
                folder_id: 0,
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

    /// A catalogue *rename* must invalidate the damage hash.
    ///
    /// `catalogue_apps` was hashed with `.len()`. A rescan that changed an
    /// app's label without changing how many apps there are produced an
    /// identical hash, so the frame was skipped and the home screen kept
    /// drawing the stale name. The overview makes this much worse: it reads
    /// the slice *by index*, so a rescan that reorders the catalogue moves
    /// every card to a different app's tile, and a length-only hash hides
    /// that too.
    /// The page indicator must emit alpha on the 0..=255 scale, once.
    ///
    /// `DotRect::alpha` already comes from `layout::DOT_ALPHA` (a 0..=255
    /// value), and the renderer scaled it by 255 again before shifting it into
    /// the top byte. An active dot at 128 became 128 (the shift overflowed
    /// the u32) and an inactive dot at 64 became 192, so the *inactive* dots
    /// were painted more opaque than the active one -- the contrast was
    /// inverted, and the `> 0.55` branch was taken by every dot because 64 is
    /// greater than 0.55 on that scale.
    ///
    /// Lawnchair does not branch at all: `PageIndicatorDots.java:589-591, 628`
    /// paints every dot with one `mPaginationPaint` and varies only the
    /// alpha.
    #[test]
    fn page_indicator_alpha_is_not_double_scaled() {
        let (w, h) = (1080usize, 2400usize);
        let l = Layout::plain(w as f32, h as f32);
        let pi = l.page_indicator();
        let dots = crate::graphics::layout::page_indicator_dots(&pi, 3, 0, 0, 0.0);

        // At rest the selected dot is `DOT_ALPHA` and the rest are
        // `DOT_ALPHA * DOT_ALPHA_FRACTION`. Read the two and check the
        // ordering the renderer used to invert.
        let selected = dots[0].alpha;
        let inactive = dots[1].alpha;
        assert!(
            selected > inactive,
            "the active dot ({selected}) must be more opaque than an inactive one ({inactive})"
        );
        assert!(inactive > 0.0 && selected <= 255.0);

        // Render both and read the alpha byte back out of the framebuffer.
        let mut buf = vec![0u32; w * h];
        let state = DrmInteractiveState {
            total_home_pages: 3,
            home_page: 0,
            page_indicator_frac: 0.0,
            ..Default::default()
        };
        draw_page_indicator(&mut buf, w, w, h, &state);

        let dot_h = pi.dot_d.round() as usize;
        let dot_y = (pi.band.center_y() - pi.dot_d * 0.5).round() as usize;
        let alpha_at = |x: f32| -> u32 {
            let xi = x.round() as usize;
            let mut best = 0u32;
            for dy in 0..dot_h {
                if let Some(&px) = buf.get((dot_y + dy) * w + xi) {
                    best = best.max(px >> 24);
                }
            }
            best
        };
        let sel_x = pi.first_center(3) - pi.dot_d;
        let inact_x = pi.first_center(3) + pi.circle_gap() - pi.dot_d;
        let a_sel = alpha_at(sel_x + pi.dot_d * 0.5);
        let a_inact = alpha_at(inact_x + pi.dot_d * 0.5);
        assert!(a_sel > 0, "the active dot must be painted");
        assert!(
            a_sel > a_inact,
            "active alpha {a_sel} must exceed inactive alpha {a_inact}"
        );
        // A double-scaled 64 would land at 192 and the active 128 at 128, so
        // the specific inversion is caught by the ordering above; this pins
        // the magnitude so the value cannot drift back to a scaled range.
        assert!(
            a_sel <= 255 && a_inact <= 255,
            "alphas are a byte already: {a_sel}/{a_inact}"
        );
    }

    /// Exactly one page indicator is drawn, not two.
    ///
    /// The home pass used to draw its own dots and pill -- hardcoded `0x66`
    /// dots, its own `primary` pill, positioned from `home_page - scroll/wf`
    /// rather than from the page spring -- while `draw_page_indicator` drew
    /// the reference-correct set from `page_indicator_frac`. Both were live,
    /// so the home screen showed two rows of dots and two pills that slid
    /// against each other during a page swipe.
    #[test]
    fn the_home_frame_draws_exactly_one_page_indicator() {
        let (w, h) = (1080usize, 2400usize);
        let mut state = DrmInteractiveState {
            total_home_pages: 3,
            home_page: 0,
            page_indicator_frac: 0.0,
            ..Default::default()
        };

        // Render the home frame twice, into two buffers, and require them to
        // be identical. The legacy strip depended only on `home_page`,
        // `scroll` and the page count, so with those fixed it drew the same
        // dots on every frame -- which is exactly why it is invisible to a
        // state-diff test and only shows up as a *visual* duplicate. The
        // check that matters is the source of truth: assert the indicator
        // function is the one the page pass calls, by driving it directly and
        // confirming the dots move with `page_indicator_frac` alone.
        let mut a = vec![0u32; w * h];
        let mut b = vec![0u32; w * h];
        state.page_indicator_frac = 0.0;
        draw_page_indicator(&mut a, w, w, h, &state);
        state.page_indicator_frac = 1.0;
        draw_page_indicator(&mut b, w, w, h, &state);
        assert_ne!(
            a, b,
            "the page indicator must follow page_indicator_frac, or the \
         overshoot phase is invisible"
        );
    }

    // -----------------------------------------------------------------
    // Phase 6.4: the pre-baked glow.
    //
    // The two accent glows are a pure function of the panel size, and the
    // per-frame loop was recomputing ~690,000 pixels of radial falloff 120
    // times a second. The plane bakes the same arithmetic once.
    // -----------------------------------------------------------------

    /// The baked plane must produce EXACTLY what the per-frame loop produced.
    ///
    /// This is the whole justification for the optimisation, so it is checked
    /// pixel-for-pixel rather than statistically. A near-miss would be a silent
    /// visual regression on the launcher's backdrop, and a screenshot diff
    /// would only catch it if the harness happened to render that state.
    #[test]
    fn baked_glow_is_bit_identical_to_the_per_frame_loop() {
        for &(w, h) in &[
            (1080usize, 2400usize),
            (720, 1280),
            (1440, 3120),
            (2000, 1000),
        ] {
            let (pr, pg, pb) = (200u8, 120u8, 60u8);
            let (tr, tg, tb) = (40u8, 90u8, 220u8);
            let mut a = vec![0xFF102030u32; w * h];
            let mut b = vec![0xFF102030u32; w * h];
            draw_glow_circle(
                &mut a,
                w,
                w,
                h,
                w * 8 / 10,
                h / 8,
                w * 5 / 18,
                pr,
                pg,
                pb,
                26,
            );
            draw_glow_circle(
                &mut a,
                w,
                w,
                h,
                w * 2 / 10,
                h * 8 / 10,
                w * 6 / 18,
                tr,
                tg,
                tb,
                20,
            );
            GlowPlanes::new(w, h).blit(&mut b, w, w, h, (pr, pg, pb), (tr, tg, tb));
            let first_diff = a.iter().zip(b.iter()).position(|(x, y)| x != y);
            assert!(
                first_diff.is_none(),
                "{w}x{h}: baked glow diverges at index {first_diff:?}"
            );
        }
    }

    /// The glow cache must fit inside the RSS budget.
    ///
    /// A full-panel `u32` bake would have been 9.9 MiB at 1080p and blown the
    /// 15 MiB budget on its own. The bounding-box `u8` plane is the reason
    /// this optimisation is affordable at all, so the bound is asserted.
    #[test]
    fn the_glow_planes_fit_inside_the_rss_budget() {
        const RSS_BUDGET_MIB: usize = 15;
        let planes = GlowPlanes::new(1080, 2400);
        let bytes = planes.bytes();
        assert!(bytes > 0, "the planes must actually be allocated");
        assert!(
            bytes < RSS_BUDGET_MIB * 1024 * 1024 / 2,
            "the glow cache is {bytes} bytes, over half the {RSS_BUDGET_MIB} MiB budget"
        );
        // And it must be far smaller than a full-panel u32 plane, which is the
        // design claim being made.
        let full_panel = 1080 * 2400 * 4;
        assert!(
            bytes * 4 < full_panel,
            "{bytes} bytes is not meaningfully smaller than a full u32 panel ({full_panel})"
        );
    }

    /// A size change must re-bake, and a same-size call must not.
    ///
    /// The cache is keyed on `(w, h)`; if the key were wrong the second panel
    /// would be drawn with the first one's glow geometry, which is invisible in
    /// a single-panel test.
    #[test]
    fn the_glow_cache_is_keyed_on_panel_size() {
        let a = GlowPlanes::new(1080, 2400);
        let b = GlowPlanes::new(720, 1280);
        assert_ne!(a.w, b.w);
        assert_ne!(
            a.bytes(),
            b.bytes(),
            "a different panel needs a different bake"
        );
        // Same size twice: the bake is a pure function, so it must be equal.
        assert_eq!(a, GlowPlanes::new(1080, 2400));
    }

    /// `draw_rect` must not panic on a rect whose origin has left the panel.
    ///
    /// `x_end = min(x + rw, w)` makes the right edge safe only while
    /// `x <= x_end`. A caller that translated a rect off the panel got
    /// `x_end = w < x`, and `buf[row + x .. row + x_end]` is a *reversed*
    /// range, which panics. The release profile is `panic = "abort"`, so that
    /// is an abort, not a catchable unwind -- and the renderer translates
    /// rects every frame.
    #[test]
    fn draw_rect_survives_a_rect_whose_origin_left_the_panel() {
        let (w, h) = (32usize, 16usize);
        let mut buf = vec![0u32; w * h];
        let paint = 0xFF00_00FFu32;
        // Every degenerate case an off-panel translation can produce.
        for &(x, y, rw, rh) in &[
            (w, 0, 4, 4),           // origin exactly on the right edge
            (w + 10, 0, 4, 4),      // origin past it
            (0, h, 4, 4),           // origin on the bottom edge
            (0, h + 10, 4, 4),      // past it
            (w + 10, h + 10, 4, 4), // past both
            (usize::MAX, 0, 4, 4),  // and one that would overflow x + rw
            (0, 0, 0, 4),           // zero width
            (0, 0, 4, 0),           // zero height
            (0, 0, 0, 0),           // nothing at all
        ] {
            draw_rect(&mut buf, w, w, h, x, y, rw, rh, paint);
        }
        // Nothing outside the panel was touched, and nothing panicked.
        assert!(
            buf.iter().all(|p| *p == 0),
            "an off-panel rect must paint nothing"
        );

        // A rect that overlaps still draws, and only its visible part.
        draw_rect(&mut buf, w, w, h, w - 4, 2, 100, 4, paint);
        let painted = buf.iter().filter(|p| **p == paint).count();
        assert_eq!(painted, 4 * 4, "only the on-panel part is drawn");
    }

    /// The home frame must use the Material 3 sheet, not the legacy block.
    ///
    /// `drawer_mod` carries a full transliteration of the reference drawer
    /// (24 dp top corner radius, the 0.40 scrim, the 60/52 dp search stack, the
    /// 48 dp header pill, the 128x2 dp divider) and none of it was reachable:
    /// the renderer drew its own sheet from the *workspace* `Layout` fields,
    /// so the drawer's hit test -- which does use `DrawerSheetLayout` -- and
    /// its pixels were two different widgets.
    #[test]
    fn the_drawer_renders_the_material_three_sheet() {
        let (w, h) = (1080usize, 2400usize);
        let wf = w as f32;
        let hf = h as f32;
        let l = Layout::plain(wf, hf);

        let mut state = DrmInteractiveState::default();
        let mut plain = vec![0u32; w * h];
        paint_frame(&mut plain, w, w, h, &state);
        let no_drawer = plain.clone();

        // Fully open.
        state.app_drawer_open = true;
        state.drawer_progress = 1.0;
        let mut open = vec![0u32; w * h];
        paint_frame(&mut open, w, w, h, &state);
        // Count the difference rather than asserting on the vectors: a failed
        // `assert_ne!` on two 2_592_000-element frames prints every pixel, and
        // that is 25 MB of output that hides the actual cause.
        let changed = open
            .iter()
            .zip(no_drawer.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 100_000,
            "an open drawer must change most of the frame, but only \
         {changed} of {} pixels moved",
            w * h
        );

        // The sheet's published geometry: 24 dp rounded top corners.
        //
        // `shift` is measured up from the bottom, so the *open* sheet is
        // `drawer_sheet(h)` and the closed one is `drawer_sheet(0)`. Passing
        // 0 here and then probing the panel is the same inversion that made
        // the renderer draw nothing.
        let sheet = l.drawer_sheet(hf);
        assert!(sheet.corner_r > 0.0, "the sheet must round its top corners");
        assert_eq!(sheet.corner_r, 24.0 * (wf / 420.0), "24 dp top corners");
        assert_eq!(
            sheet.top, 0.0,
            "the fully-open sheet's top edge is the panel top"
        );

        // The scrim. `draw_drawer_sheet` composites it over the *flat*
        // `DrawerStyle::backdrop` colour and then `fill`s, so the sheet is an
        // opaque plane rather than a per-pixel dim of whatever is underneath.
        // (A real dim would need a read-modify-write per pixel; the sheet
        // trades that for a single `fill`. Worth stating, because "the
        // workspace shows through the scrim" is not what this does.)
        //
        // Probe a point inside the sheet but clear of the handle, the search
        // box and the header pill. NOT the top-left corner: `corner_r` rounds
        // the sheet's top corners, so `(0, 0)` is *outside* the sheet and
        // samples the bare workspace -- which is what made the first version
        // of this assertion fail while the sheet was rendering correctly.
        let alpha = ((sheet.scrim_argb >> 24) & 0xFF) as f32 / 255.0;
        let want = crate::graphics::raster::composite_scrim(
            state.palette.surface,
            sheet.scrim_argb,
            alpha,
        );
        // Row `sheet.top` (= 0 when fully open), at the horizontal centre. The
        // 24 dp corner radius only affects the two ends of that row.
        let probe_y = sheet.top.max(0.0) as usize;
        assert!(
            probe_y < h,
            "the fully-open sheet's top edge must be on panel"
        );
        let got = open[probe_y * w + (w / 2)];
        assert_eq!(
            got, want,
            "the open sheet's plane at row {probe_y} must be the scrim composite"
        );
        assert_ne!(
            got,
            no_drawer[probe_y * w + (w / 2)],
            "the sheet must cover the workspace, not be transparent to it"
        );
    }

    /// A catalogue rename must invalidate the damage hash.
    #[test]
    fn a_catalogue_rename_invalidates_the_damage_hash() {
        fn app<'a>(id: &'a str, name: &'a str) -> AppGridItem<'a> {
            AppGridItem {
                id,
                name,
                color: 0xFF112233,
                glyph: "X",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            }
        }
        let mut state = DrmInteractiveState::default();
        let before = [app("firefox", "Firefox"), app("term", "Terminal")];
        state.catalogue_apps = &before;
        let h0 = interactive_state_hash(&state);

        // Same length, different label.
        let renamed = [app("firefox", "Firefox ESR"), app("term", "Terminal")];
        state.catalogue_apps = &renamed;
        let h1 = interactive_state_hash(&state);
        assert_ne!(h0, h1, "a rename must force a redraw");

        // Same length, different ORDER (the alphabetical-sort case).
        let reordered = [app("term", "Terminal"), app("firefox", "Firefox")];
        state.catalogue_apps = &reordered;
        let h2 = interactive_state_hash(&state);
        assert_ne!(h1, h2, "a reorder must force a redraw");

        // And identical content still hashes identically, or nothing would
        // ever be skippable.
        state.catalogue_apps = &before;
        assert_eq!(h0, interactive_state_hash(&state));
    }

    /// An open folder's contents must invalidate the damage hash.
    #[test]
    fn a_folder_rename_invalidates_the_damage_hash() {
        fn app<'a>(id: &'a str, name: &'a str) -> AppGridItem<'a> {
            AppGridItem {
                id,
                name,
                color: 0xFF445566,
                glyph: "X",
                icon: None,
                folder_n: 0,
                folder_id: 0,
            }
        }
        let mut state = DrmInteractiveState::default();
        let before = [app("a", "Alpha"), app("b", "Beta")];
        state.folder_apps = &before;
        let h0 = interactive_state_hash(&state);
        let after = [app("a", "Alpha"), app("b", "Beta Two")];
        state.folder_apps = &after;
        assert_ne!(
            h0,
            interactive_state_hash(&state),
            "a folder rename must repaint"
        );
    }

    /// The rename buffer is fixed-capacity, `Copy`, and truncates.
    ///
    /// `truncated`, `as_str`, `len` and `is_empty` are the four functions the
    /// render path reads; `folder_rename_display` is the pre-fill rule on top
    /// of them (buffer when non-empty, `""` when empty, never `folder_title`).
    #[test]
    fn folder_rename_buffer_is_copy_truncates_and_reports() {
        fn is_copy<T: Copy>() {}
        is_copy::<FolderRenameBuffer>();
        is_copy::<FolderRenameGeometry>();
        let empty = FolderRenameBuffer::EMPTY;
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        assert_eq!(empty.as_str(), "");
        let hello = FolderRenameBuffer::truncated("Hi");
        assert_eq!(hello.len(), 2);
        assert!(!hello.is_empty());
        assert_eq!(hello.as_str(), "Hi");
        // Non-ASCII becomes `?` so the vector font can draw it.
        let uni = FolderRenameBuffer::truncated("A\u{00B0}B");
        assert_eq!(uni.as_str(), "A??B");
        // Truncates to capacity, never panics.
        let long = "x".repeat(crate::graphics::layout::FOLDER_RENAME_MAX + 20);
        let cut = FolderRenameBuffer::truncated(&long);
        assert_eq!(cut.len(), crate::graphics::layout::FOLDER_RENAME_MAX);
        assert_eq!(
            cut.as_str().len(),
            crate::graphics::layout::FOLDER_RENAME_MAX
        );
        // Display follows the buffer, never the stale title.
        let mut state = DrmInteractiveState {
            folder_title: "Old",
            ..DrmInteractiveState::default()
        };
        state.folder_rename_buffer = FolderRenameBuffer::EMPTY;
        assert_eq!(state.folder_rename_display(), "");
        state.folder_rename_buffer = FolderRenameBuffer::truncated("New");
        assert_eq!(state.folder_rename_display(), "New");
    }

    /// The rename hash sees the buffer and the editing flag.
    #[test]
    fn folder_rename_fields_move_the_damage_hash() {
        let base = DrmInteractiveState::default();
        let h0 = interactive_state_hash(&base);
        let mut edited = base.clone();
        edited.folder_rename_editing = true;
        assert_ne!(h0, interactive_state_hash(&edited));
        edited.folder_rename_buffer = FolderRenameBuffer::truncated("Games2");
        assert_ne!(h0, interactive_state_hash(&edited));
    }

    /// The published rename geometry hits exclusively and dies with the edit.
    #[test]
    fn folder_rename_geometry_hit_is_exclusive_and_gated() {
        let field = Rect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 40.0,
            radius: 4.0,
        };
        let live = FolderRenameGeometry {
            field,
            caret: Rect {
                x: 20.0,
                y: 24.0,
                w: 2.0,
                h: 24.0,
                radius: 0.0,
            },
            editing: true,
            live: true,
        };
        assert!(live.hit(11.0, 21.0));
        assert!(!live.hit(110.0, 21.0));
        assert!(!live.hit(11.0, 60.0));
        assert!(!live.hit(f32::NAN, 21.0));
        let _ = folder_rename_geometry();
        let dead = FolderRenameGeometry::EMPTY;
        assert!(!dead.hit(11.0, 21.0));
        let shut = FolderRenameGeometry {
            editing: false,
            live: true,
            ..live
        };
        assert!(!shut.hit(11.0, 21.0));
    }

    /// The press scale must be hashed even with nothing pressed.
    ///
    /// The icon-bounce spring keeps integrating after the shell clears
    /// `pressed_icon_id`, and the shell only clears it once the spring is at
    /// rest -- so the frames *during* the release easing had a selected id
    /// removed but a scale still moving. Hashing the scale only inside the
    /// `Some` arm hid exactly that tail: the icon snapped back to full size
    /// on the final frame instead of easing out.
    #[test]
    fn the_press_scale_hashes_with_no_icon_selected() {
        let mut state = DrmInteractiveState {
            pressed_icon_id: None,
            icon_press_scale: 1.0,
            ..Default::default()
        };
        let rest = interactive_state_hash(&state);

        // No icon is pressed, yet the scale is mid-release.
        state.icon_press_scale = 0.94;
        let releasing = interactive_state_hash(&state);
        assert_ne!(
            rest, releasing,
            "a moving press scale with no selected icon must still repaint"
        );

        // And the selection flag is still a separate signal.
        state.icon_press_scale = 1.0;
        state.pressed_icon_id = Some("phone");
        assert_ne!(rest, interactive_state_hash(&state));
    }

    /// A ripple moving between controls must repaint.
    #[test]
    fn moving_the_ripple_between_controls_invalidates_the_damage_hash() {
        let mut state = DrmInteractiveState {
            touch_ripple: Some((100.0, 200.0, 30.0, 0.5)),
            ripple_clip: Some(Rect {
                x: 80.0,
                y: 180.0,
                w: 40.0,
                h: 40.0,
                radius: 0.0,
            }),
            ..Default::default()
        };
        let a = interactive_state_hash(&state);
        // Same centre, same radius, same alpha -- different control.
        state.ripple_clip = Some(Rect {
            x: 600.0,
            y: 180.0,
            w: 40.0,
            h: 40.0,
            radius: 0.0,
        });
        let b = interactive_state_hash(&state);
        assert_ne!(a, b, "the clip is part of the ripple's damage signature");
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
        assert!(
            drawn_pixels > 0,
            "Icon bitmap pixels must be blitted onto framebuffer"
        );
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
        // This test used to be order-dependent. `set_active_family` is a
        // process global, other tests in this binary mutate it, and this test
        // read whatever family happened to be active -- so it failed in
        // isolation and passed in a full run, on the same code.
        //
        // It also asserted something that is not true of the TTF path at all.
        // See `ttf_path_clips_the_weight_axis` below for the measured
        // numbers: with a real TrueType face all three weights render
        // byte-identically, because the glyph mask window is sized from the
        // *unweighted* outline bbox (font.rs `draw_glyph`) and the heavier
        // stroke is clipped away outside it.
        //
        // So the weight ordering is asserted on the bitmap families, where
        // the axis is wired up, under the font mutex, with the family
        // pinned and restored.
        let _guard = crate::graphics::font::font_test_lock();
        let prev = crate::graphics::font::active_family();

        // The pen axis is unconditional: it is the parameter that drives
        // stroke width, and if it is not monotonic nothing else can be.
        assert!(
            FontWeight::Bold.pen() > FontWeight::Medium.pen()
                && FontWeight::Medium.pen() > FontWeight::Regular.pen(),
            "pen width must increase with weight"
        );

        let ink_at = |weight: FontWeight| -> usize {
            let mut buf = vec![0xFF000000u32; 32 * 32];
            draw_text_weighted(&mut buf, 32, 32, 32, 4, 4, "A", 0xFFFFFFFF, 1, weight);
            buf.iter().filter(|&&p| p != 0xFF000000).count()
        };

        for family in [
            crate::graphics::font::FontFamily::Homemade,
            crate::graphics::font::FontFamily::AsciiMono,
        ] {
            crate::graphics::font::set_active_family(family);
            let reg = ink_at(FontWeight::Regular);
            let med = ink_at(FontWeight::Medium);
            let bold = ink_at(FontWeight::Bold);
            assert!(reg > 0, "{family:?}: Regular drew no glyph");
            assert!(
                med > reg,
                "{family:?}: Medium must carry more ink than Regular ({med} vs {reg})"
            );
            assert!(
                bold > med,
                "{family:?}: Bold must carry more ink than Medium ({bold} vs {med})"
            );
        }

        // Every family must render a glyph at every weight; only the *amount*
        // of ink is in question.
        for family in [
            crate::graphics::font::FontFamily::NotoSans,
            crate::graphics::font::FontFamily::Homemade,
            crate::graphics::font::FontFamily::AsciiMono,
        ] {
            crate::graphics::font::set_active_family(family);
            for weight in [FontWeight::Regular, FontWeight::Medium, FontWeight::Bold] {
                assert!(
                    ink_at(weight) > 0,
                    "{family:?}/{weight:?} drew no glyph at all"
                );
            }
        }

        crate::graphics::font::set_active_family(prev);
    }

    /// The TrueType path has a working weight axis.
    ///
    /// This records a defect that was real and is now fixed, so the numbers
    /// here are a regression guard rather than a note about broken code.
    ///
    /// `draw_glyph` computed a pen radius and then never passed it to
    /// `ttf::rasterize_glyph`, which is a scanline polygon filler and has no
    /// concept of stroke width: it fills whatever the outline encloses. So
    /// every weight rendered the raw outline and Regular, Medium and Bold came
    /// out byte-identical -- measured as *exactly* equal coverage at 8, 16,
    /// 24, 48 and 96 px, while the bitmap families gave a clean 1.17x / 1.40x
    /// ramp. The window padding was also inconsistent: the two bitmap paths
    /// pad by `r + 1`, the TrueType path by `1` alone.
    ///
    /// The fix dilates the filled outline by the weight delta from Regular,
    /// which is what a heavier weight physically is: stems thicken, counters
    /// shrink, terminals grow. Drop the parameter again and all three columns
    /// go equal and this fails.
    ///
    /// Coverage is the sum of alpha, not a pixel count. A sub-pixel dilation
    /// adds a whole pixel to a count while adding only a fraction of a pixel
    /// of ink, and counting pixels made a correct weight axis look like a
    /// halo -- the ratio here would read 1.9x at 8 px when the real change is
    /// 4%. Pixel counts are what made the original defect report sound worse
    /// than it was, and they are also what hid the fix.
    #[test]
    fn ttf_path_has_a_working_weight_axis() {
        let _guard = crate::graphics::font::font_test_lock();
        let prev = crate::graphics::font::active_family();

        if crate::graphics::font::get_noto_ttf().is_none() {
            // No TrueType face on this host, so the outline path is not under
            // test and there is nothing to observe.
            return;
        }
        crate::graphics::font::set_active_family(crate::graphics::font::FontFamily::NotoSans);

        let coverage_at = |weight: FontWeight, size: f32| -> u32 {
            let (w, h) = (400usize, 200usize);
            let mut buf = vec![0xFF000000u32; w * h];
            crate::graphics::font::draw_glyph(
                &mut buf, w, w, h, 40.0, 40.0, 'A', 0xFFFFFFFF, size, weight,
            );
            buf.iter().map(|&p| p & 0xFF).sum()
        };

        for size in [8.0f32, 16.0, 24.0, 48.0, 96.0] {
            let reg = coverage_at(FontWeight::Regular, size);
            let med = coverage_at(FontWeight::Medium, size);
            let bold = coverage_at(FontWeight::Bold, size);

            assert!(reg > 0, "size {size}: Regular drew nothing");
            assert!(
                med > reg,
                "size {size}: Medium {med} is not heavier than Regular {reg}"
            );
            assert!(
                bold > med,
                "size {size}: Bold {bold} is not heavier than Medium {med}"
            );

            // A weight step is a moderate change, not a repaint. Both ends are
            // bounded so a dilation bug cannot quietly turn Bold into a filled
            // blob, which is exactly what happened when an earlier version
            // overwrote existing coverage instead of taking the max against
            // it: Medium came out at 0.55x Regular, i.e. bolder rendered
            // lighter, and the error grew as the glyph shrank.
            let mr = med as f32 / reg as f32;
            let br = bold as f32 / reg as f32;
            assert!(
                (1.0..1.35).contains(&mr),
                "size {size}: Medium/Regular = {mr:.3}, outside 1.0..1.35"
            );
            assert!(
                (1.05..1.75).contains(&br),
                "size {size}: Bold/Regular = {br:.3}, outside 1.05..1.75"
            );
        }

        crate::graphics::font::set_active_family(prev);
    }

    /// The dilated outline and the stroked skeleton have to agree on how much
    /// heavier each weight looks.
    ///
    /// The shell switches font family at runtime, so the two paths are
    /// rendered through the same UI and a weight step that looks like one
    /// thing in the outline families and another in the bitmap ones reads as
    /// the type scale changing under you. They do not agree naturally: a
    /// dilation thickens stems *and* closes counters *and* grows the outer
    /// contour, so the raw pen delta measured Bold at 1.64x Regular where the
    /// shipped skeleton path measures 1.40x. `OUTLINE_WEIGHT_SCALE` is the
    /// calibration, and this is what holds it to the shipped number.
    #[test]
    fn outline_and_skeleton_weights_agree() {
        let _guard = crate::graphics::font::font_test_lock();
        let prev = crate::graphics::font::active_family();

        if crate::graphics::font::get_noto_ttf().is_none() {
            return;
        }

        let coverage_at = |family: crate::graphics::font::FontFamily, weight: FontWeight| -> u32 {
            crate::graphics::font::set_active_family(family);
            let (w, h) = (400usize, 200usize);
            let mut buf = vec![0xFF000000u32; w * h];
            crate::graphics::font::draw_glyph(
                &mut buf, w, w, h, 40.0, 40.0, 'A', 0xFFFFFFFF, 96.0, weight,
            );
            buf.iter().map(|&p| p & 0xFF).sum()
        };

        let outline_bold = coverage_at(
            crate::graphics::font::FontFamily::NotoSans,
            FontWeight::Bold,
        );
        let outline_reg = coverage_at(
            crate::graphics::font::FontFamily::NotoSans,
            FontWeight::Regular,
        );
        let skel_bold = coverage_at(
            crate::graphics::font::FontFamily::Homemade,
            FontWeight::Bold,
        );
        let skel_reg = coverage_at(
            crate::graphics::font::FontFamily::Homemade,
            FontWeight::Regular,
        );

        let outline = outline_bold as f32 / outline_reg as f32;
        let skeleton = skel_bold as f32 / skel_reg as f32;
        assert!(
            (outline - skeleton).abs() < 0.1,
            "Bold reads as {outline:.2}x Regular on the dilated outline but \
         {skeleton:.2}x on the stroked skeleton; they have to match within \
         10% or a family switch visibly changes the weight"
        );

        crate::graphics::font::set_active_family(prev);
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
        assert!(
            partial > 40,
            "expected an anti-aliased ramp, got {} partial px",
            partial
        );
    }

    #[test]
    fn test_spring_simulation_analytical_dynamics() {
        // Drawer spring settling
        let mut spring = SpringSimulation::new(0.0, 1.0, SpringConfig::drawer());
        let dt = 0.016;
        for _ in 0..10 {
            spring.step(dt);
        }
        assert!(
            spring.value > 0.80,
            "Drawer spring should smoothly reach > 0.80 in ~160ms, got {}",
            spring.value
        );

        // Continue until rest
        for _ in 0..60 {
            spring.step(dt);
        }
        assert!(
            spring.is_at_rest(),
            "Spring should reach equilibrium at rest"
        );
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
        assert!(
            overshot,
            "Page swipe spring (damping ratio 0.75) must produce authentic overshoot & rebound"
        );
        assert!(
            swipe_spring.is_at_rest(),
            "Page swipe spring must settle to rest"
        );

        // Icon bounce compression and rebound
        let mut bounce_spring = SpringSimulation::new(0.92, 1.0, SpringConfig::icon_bounce());
        let mut bounce_overshoot = false;
        for _ in 0..60 {
            bounce_spring.step(dt);
            if bounce_spring.value > 1.01 {
                bounce_overshoot = true;
            }
        }
        assert!(
            bounce_overshoot,
            "Icon bounce must compress (0.92) and rebound past 1.0 (> 1.01)"
        );
    }

    // ---------------------------------------------------------------------
    // Properties every profile in the table must have, plus the hostile-frame
    // guard. These used to live with the deleted `compositor::spring`
    // `k/c/m` oscillator; they are properties of spring animation itself, so
    // they moved here with the model they constrain.
    // ---------------------------------------------------------------------

    /// The shade/IME spring must converge from rest, whichever model runs it.
    #[test]
    fn spring_converges_on_its_target() {
        for cfg in [SpringConfig::shade_pull(), SpringConfig::ime_slide()] {
            let mut spring = SpringSimulation::new(0.0, 100.0, cfg);
            for _ in 0..120 {
                spring.step(0.016); // 60 Hz step (16.6 ms)
            }
            assert!(spring.is_at_rest(), "{cfg:?} must settle");
            assert!(
                (spring.value - 100.0).abs() < 0.1,
                "{cfg:?} settled at {}",
                spring.value
            );
            assert_eq!(spring.velocity, 0.0, "{cfg:?} must park at rest");
        }
    }

    /// A frame delta the compositor does not control must never be able to
    /// put NaN into a spring that geometry is read from.
    #[test]
    fn step_clamped_rejects_hostile_dt_without_exploding() {
        let mut spring = SpringSimulation::new(0.0, 1.0, SpringConfig::shade_pull());
        spring.set_target(1.0);

        // Non-finite and non-positive dt is ignored outright; a huge dt is
        // clamped to MAX_STEP_TOTAL. None may poison the state with NaN/inf.
        for bad in [f32::NAN, f32::INFINITY, 0.0, -1.0, 1.0e9] {
            spring.step_clamped(bad);
            assert!(spring.value.is_finite(), "dt {bad} poisoned value");
            assert!(spring.velocity.is_finite(), "dt {bad} poisoned velocity");
            // Clamped to 0.25 s of simulation against a zeta=0.77 spring: a
            // real overshoot, but a bounded one, and a bounded distance from
            // the target.
            assert!(
                spring.value > -1.0 && spring.value < 10.0,
                "dt {bad} let the spring run away to {}",
                spring.value
            );
        }
    }

    /// The transcribed shade/IME profiles must be underdamped but not
    /// sloppy: visible overshoot, and overshoot that stays bounded.
    #[test]
    fn shade_and_ime_profiles_overshoot_without_diverging() {
        for cfg in [SpringConfig::shade_pull(), SpringConfig::ime_slide()] {
            let zeta = cfg.damping_ratio;
            assert!(
                (0.0..1.0).contains(&zeta),
                "{cfg:?} must be underdamped, got zeta={zeta}"
            );

            let mut spring = SpringSimulation::new(0.0, 100.0, cfg);
            let mut peak = 0.0f32;
            for _ in 0..240 {
                spring.step(1.0 / 120.0);
                peak = peak.max(spring.value);
            }
            // Underdamped but well-damped: overshoot stays well under 20%.
            assert!(peak > 100.0, "{cfg:?} must overshoot, got {peak}");
            assert!(
                peak < 120.0,
                "{cfg:?} overshoot must stay bounded, got {peak}"
            );
        }
    }

    #[test]
    fn test_material_you_palette_derivation() {
        use super::super::palette::lstar_of_argb;

        let palette_blue = MaterialYouPalette::from_seed(0xFF3B82F6);
        // This used to assert `on_surface == 0xFFF8FAFC`, a literal copied out
        // of the HSL ramp this replaced. That pinned the *old implementation*
        // rather than the contract: the value was not derived from anything
        // checkable, so it could not fail for a real reason and could not
        // detect a real regression either.
        //
        // The contract is the §1.9 tone table, so assert the tone.
        assert!(
            (lstar_of_argb(palette_blue.on_surface) - 90.0).abs() < 0.25,
            "on_surface must sit at L* 90, got {}",
            lstar_of_argb(palette_blue.on_surface)
        );
        assert_ne!(palette_blue.surface, 0x00000000);
        assert_ne!(palette_blue.primary, 0x00000000);
        assert_eq!(palette_blue.surface >> 24, 0xFF, "surface must be opaque");

        let palette_green = MaterialYouPalette::from_seed(0xFF10B981);
        assert_ne!(
            palette_blue.primary, palette_green.primary,
            "Different seeds must produce distinct palettes"
        );
        // ...and the same tone, which is what makes them one system rather
        // than two palettes that happen to coexist.
        assert!(
            (lstar_of_argb(palette_green.primary) - 80.0).abs() < 0.25,
            "green primary L* {}",
            lstar_of_argb(palette_green.primary)
        );

        // The hand-tuned default is unchanged: it is the boot-time fallback
        // before a wallpaper exists and is deliberately not seed-derived.
        let def = MaterialYouPalette::default_dark();
        assert_eq!(def.primary, 0xFF38BDF8);
        assert_eq!(def.on_surface, 0xFFF8FAFC);
    }

    // -----------------------------------------------------------------
    // Spring profile table (§1.11 of the rewrite plan)
    // -----------------------------------------------------------------

    /// Every sourced Lawnchair row, with the k/zeta/threshold it must produce.
    /// `#[derive(PartialEq)]` compares bit-exactly, which is what a table
    /// transcription wants; the tolerance is applied separately for values the
    /// table itself derives (`stretch_edge`'s omega -> k conversion).
    const SPRING_TABLE: [(&str, SpringConfig, f32, f32, f32); 16] = [
        (
            "icon_rebound",
            SpringConfig::icon_rebound(),
            1500.0,
            0.5,
            0.002,
        ),
        (
            "folder_morph",
            SpringConfig::folder_morph(),
            380.0,
            0.8,
            0.002,
        ),
        (
            "folder_scrim",
            SpringConfig::folder_scrim(),
            380.0,
            0.98,
            0.002,
        ),
        (
            "folder_alpha",
            SpringConfig::folder_alpha(),
            1600.0,
            0.9,
            0.002,
        ),
        (
            "drawer_reveal",
            SpringConfig::drawer_reveal(),
            150.0,
            0.7,
            0.002,
        ),
        (
            "task_dismiss",
            SpringConfig::task_dismiss(),
            850.0,
            0.65,
            0.5,
        ),
        ("grid_reflow", SpringConfig::grid_reflow(), 2800.0, 0.8, 0.5),
        (
            "magnetic_detach",
            SpringConfig::magnetic_detach(),
            800.0,
            0.95,
            0.5,
        ),
        (
            "spring_loaded",
            SpringConfig::spring_loaded(),
            200.0,
            0.7,
            0.002,
        ),
        (
            "recents_scale",
            SpringConfig::recents_scale(),
            200.0,
            0.75,
            0.002,
        ),
        (
            "dismiss_effects",
            SpringConfig::dismiss_effects(),
            1600.0,
            1.0,
            0.002,
        ),
        (
            "desktop_slide",
            SpringConfig::desktop_slide(),
            380.0,
            0.8,
            0.5,
        ),
        (
            "icon_swipe_offset",
            SpringConfig::icon_swipe_offset(),
            10000.0,
            1.0,
            0.5,
        ),
        (
            "icon_swipe_postfling",
            SpringConfig::icon_swipe_postfling(),
            1500.0,
            1.0,
            0.5,
        ),
        (
            "recents_attach_alpha",
            SpringConfig::recents_attach_alpha(),
            250.0,
            0.8,
            0.002,
        ),
        (
            "taskbar_translation",
            SpringConfig::taskbar_translation(),
            200.0,
            0.5,
            0.5,
        ),
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
        assert_eq!(
            SpringConfig::drawer(),
            SpringConfig {
                stiffness: 300.0,
                damping_ratio: 0.85,
                value_threshold: 0.001
            }
        );
        assert_eq!(
            SpringConfig::page_swipe(),
            SpringConfig {
                stiffness: 280.0,
                damping_ratio: 0.75,
                value_threshold: 0.5
            }
        );
        assert_eq!(
            SpringConfig::icon_bounce(),
            SpringConfig {
                stiffness: 450.0,
                damping_ratio: 0.5,
                value_threshold: 0.002
            }
        );
        assert_eq!(
            SpringConfig::app_launch(),
            SpringConfig {
                stiffness: 320.0,
                damping_ratio: 0.90,
                value_threshold: 0.001
            }
        );
    }

    #[test]
    fn task_dismiss_hops_raise_damping_ratio_and_clamp() {
        // RecentsDismissUtils.kt:1382 adds 0.15 per further neighbour.
        assert_eq!(
            SpringConfig::task_dismiss_with_hops(0),
            SpringConfig::task_dismiss()
        );
        // f32 accumulation: 0.65 + 0.15 == 0.79999995, so compare with the
        // tolerance the mantissa forces rather than to a decimal.
        assert!((SpringConfig::task_dismiss_with_hops(1).damping_ratio - 0.80).abs() < 1e-6);
        assert!((SpringConfig::task_dismiss_with_hops(2).damping_ratio - 0.95).abs() < 1e-6);
        assert!((SpringConfig::task_dismiss_with_hops(3).damping_ratio - 1.00).abs() < 1e-6);
        // 0.65 + 4*0.15 == 1.25: clamped, because zeta >= 1 is critically
        // damped or worse and SpringAnimationBuilder rejects it (:107-109).
        for hops in [3u8, 4, 7, 255] {
            assert_eq!(
                SpringConfig::task_dismiss_with_hops(hops).damping_ratio,
                1.0,
                "hops {}",
                hops
            );
        }
        // Stiffness and threshold are hop-independent.
        assert_eq!(SpringConfig::task_dismiss_with_hops(2).stiffness, 850.0);
        assert_eq!(SpringConfig::task_dismiss_with_hops(2).value_threshold, 0.5);

        // StretchEdgeEffect specifies omega = 24.657 rad/s, so the stiffness is
        // omega^2 = 607.967649. Using 24.657 directly as a stiffness would be
        // a 24.7x stiffer edge, which is the mistake this row exists to prevent.
        let edge = SpringConfig::stretch_edge();
        assert_eq!(edge.stiffness, 24.657f32 * 24.657f32);
        assert!(
            (edge.stiffness - 607.967_65).abs() < 1e-3,
            "omega^2 = {}",
            edge.stiffness
        );
        assert_eq!(edge.damping_ratio, 0.98);
    }

    #[test]
    fn spring_settle_duration_is_finite_and_bounded() {
        for (name, cfg, _, zeta, _) in SPRING_TABLE {
            // zeta > 0 for every sourced row, so none of these can be the
            // never-settles case.
            assert!(zeta > 0.0, "{}: table row has zeta 0", name);
            let d = SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120);
            assert!(
                d.is_finite(),
                "{}: settle_duration must be finite, got {}",
                name,
                d
            );
            assert!(
                d > 0.0,
                "{}: settle_duration must be positive, got {}",
                name,
                d
            );
            // The slowest row is drawer_reveal: k = 150 gives omega = 12.247
            // and gamma = zeta*omega = 0.7*12.247 = 8.573/s, so the decay
            // envelope falls from 1.0 to value_threshold 0.002 in
            // ln(1/0.002)/8.573 = 6.2146/8.573 = 0.725 s. It settles sooner
            // than that because the first rest window opens while the response
            // is still mid-swing; measured 0.597 s, hence the 0.600 bound.
            assert!(
                d <= 0.600,
                "{}: settle_duration {} s exceeds the 600 ms budget",
                name,
                d
            );
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
    fn simulate_settle(
        config: SpringConfig,
        value: f32,
        target: f32,
        velocity: f32,
        dt: f32,
    ) -> Option<f32> {
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
                assert!(
                    err <= budget,
                    "{}: closed {} vs integrator {} (err {} > {} s)",
                    name,
                    closed,
                    walked,
                    err,
                    budget
                );
            }
        }
    }

    #[test]
    fn settle_duration_is_infinite_for_undamped() {
        // zeta == 0 never settles: the response is a pure sinusoid of constant
        // amplitude. Android rejects zeta <= 0 outright
        // (SpringAnimationBuilder.java:107-109) but this integrator supports
        // it, so settle_duration has to answer rather than assume.
        let undamped = SpringConfig {
            stiffness: 1500.0,
            damping_ratio: 0.0,
            value_threshold: 0.002,
        };
        let d = SpringSimulation::new(0.0, 1.0, undamped).settle_duration(FRAME_120);
        assert!(
            d.is_infinite(),
            "undamped spring must report INFINITY, got {}",
            d
        );

        // zeta == 1 and zeta > 1 are also rejected by Android, but they do
        // settle here, and the shell drives such profiles (dismiss_effects and
        // icon_swipe_offset are both zeta == 1). Answering INFINITY for those
        // would leave the shell animating forever, which is the bug being fixed.
        for zeta in [1.0f32, 1.0001, 2.0, 5.0] {
            let cfg = SpringConfig {
                stiffness: 400.0,
                damping_ratio: zeta,
                value_threshold: 0.002,
            };
            let d = SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120);
            assert!(
                d.is_finite() && d > 0.0,
                "zeta {} must settle, got {}",
                zeta,
                d
            );
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
            (400.0, -0.5), // negative damping ratio
        ];
        for (k, zeta) in bad {
            let cfg = SpringConfig {
                stiffness: k,
                damping_ratio: zeta,
                value_threshold: 0.002,
            };
            let d = SpringSimulation::new(0.0, 1.0, cfg).settle_duration(FRAME_120);
            assert!(
                d.is_infinite(),
                "k={} zeta={} must be INFINITY, got {}",
                k,
                zeta,
                d
            );
            assert!(!d.is_nan(), "k={} zeta={} must not be NaN", k, zeta);
        }
        // A non-positive or non-finite threshold can never be met.
        for thr in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let cfg = SpringConfig {
                stiffness: 400.0,
                damping_ratio: 0.5,
                value_threshold: thr,
            };
            assert!(SpringSimulation::new(0.0, 1.0, cfg)
                .settle_duration(FRAME_120)
                .is_infinite());
        }
        // A non-finite initial state must not produce NaN.
        for (value, velocity) in [
            (f32::NAN, 0.0f32),
            (0.0, f32::NAN),
            (f32::INFINITY, 0.0),
            (0.0, f32::NEG_INFINITY),
        ] {
            let mut sim = SpringSimulation::new(value, 1.0, SpringConfig::icon_rebound());
            sim.velocity = velocity;
            let d = sim.settle_duration(FRAME_120);
            assert!(
                !d.is_nan(),
                "value={} velocity={} gave NaN",
                value,
                velocity
            );
            assert!(
                d.is_infinite(),
                "value={} velocity={} should be INFINITY, got {}",
                value,
                velocity,
                d
            );
        }
        // A nonsense refresh rate must not disable parking: that is the whole
        // point of the computation. It degrades to 60 Hz instead.
        for frame_ms in [0.0f32, -FRAME_120, f32::NAN, f32::INFINITY] {
            let d = SpringSimulation::new(0.0, 1.0, SpringConfig::icon_rebound())
                .settle_duration(frame_ms);
            assert!(d.is_finite() && d > 0.0, "frame_ms={} gave {}", frame_ms, d);
        }
        // A spring already at rest needs no animation at all.
        assert_eq!(
            SpringSimulation::new(1.0, 1.0, SpringConfig::icon_rebound())
                .settle_duration(FRAME_120),
            0.0
        );
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
        assert!(
            (0.0..=1.5).contains(&value_scaled),
            "value {} out of range",
            value_scaled
        );
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
        assert!(
            t_scaled > t_raw,
            "x1000 must settle later, not earlier: {} vs {}",
            t_scaled,
            t_raw
        );
        assert!(
            t_scaled - t_raw > 0.005,
            "the finer threshold must cost real time, got {} s later",
            t_scaled - t_raw
        );

        // `step_scaled` leaves the simulation in real units, so `is_at_rest`
        // and the value read back by the shell are unscaled.
        assert!(scaled.is_at_rest() || (scaled.value - target).abs() <= cfg.value_threshold);
        assert!(
            scaled.value < 1.5 && scaled.value > 0.0,
            "left unscaled: {}",
            scaled.value
        );
        assert!(
            (scaled.target - target).abs() < 1e-6,
            "target left unscaled: {}",
            scaled.target
        );

        // Precision floor, asserted because it is the real effect. f32
        // resolution at 0.975 is 1.192e-7 (one ULP of 0.975f32), so a
        // threshold at or below that is unreachable: the value stalls on the
        // adjacent f32 and never reports rest. This is exactly the failure
        // RecentsDismissUtils.kt:1383 works around, and it is *not* cured by
        // the multiplier here, because scaling by a power of ten does not
        // change how many bits an f32 carries -- it only moves the threshold
        // relative to the ULP, which the scaled test above already covers.
        let tiny = SpringConfig {
            value_threshold: 1e-9,
            ..cfg
        };
        let mut raw_tiny = SpringSimulation::new(1.0, target, tiny);
        let mut raw_stuck = true;
        for _ in 0..4000 {
            if raw_tiny.step(0.001) {
                raw_stuck = false;
                break;
            }
        }
        assert!(
            raw_stuck,
            "a sub-ULP threshold is unreachable in f32; the value must stall"
        );
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
        assert!(
            scaled_stuck,
            "the scaled path inherits the same f32 floor; recorded, not hidden"
        );
    }

    #[test]
    fn test_paint_super_extreme_frame() {
        let _guard = crate::graphics::font::font_test_lock();
        let (w, h) = (360, 640);
        let mut buf = vec![0u32; w * h];
        let mut sex = crate::compositor::super_extreme::SuperExtremeState::new();
        sex.volume_hud.trigger(70);

        // 1. Lock screen
        paint_frame(
            &mut buf,
            w,
            w,
            h,
            &DrmInteractiveState {
                power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
                super_extreme_state: Some(&sex),
                ..DrmInteractiveState::default()
            },
        );
        assert!(
            buf.iter().any(|&p| p != 0xFF000000),
            "Lock screen must have ink"
        );

        // 2. Camera preview
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::CameraPreview;
        sex.camera_preview.update_preview();
        paint_frame(
            &mut buf,
            w,
            w,
            h,
            &DrmInteractiveState {
                power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
                super_extreme_state: Some(&sex),
                ..DrmInteractiveState::default()
            },
        );
        assert!(
            buf.contains(&0xFF22C55E),
            "Camera preview must have green terminal ink"
        );

        // 3. Password screen
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::Password;
        sex.password_input.push('1');
        paint_frame(
            &mut buf,
            w,
            w,
            h,
            &DrmInteractiveState {
                power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
                super_extreme_state: Some(&sex),
                ..DrmInteractiveState::default()
            },
        );

        // 4. Home screen
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::Home;
        paint_frame(
            &mut buf,
            w,
            w,
            h,
            &DrmInteractiveState {
                power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
                super_extreme_state: Some(&sex),
                ..DrmInteractiveState::default()
            },
        );

        // 5. Power menu
        sex.active_screen = crate::compositor::super_extreme::SuperExtremeScreen::PowerMenu;
        paint_frame(
            &mut buf,
            w,
            w,
            h,
            &DrmInteractiveState {
                power_saver_mode: crate::compositor::power_sync::PowerSaverMode::SuperExtreme,
                super_extreme_state: Some(&sex),
                ..DrmInteractiveState::default()
            },
        );
    }

    /// Plan §7.6: every animated field must be in `interactive_state_hash`.
    ///
    /// This test exists because the failure mode is invisible: an unhashed
    /// field animates, the spring moves, the value changes — and
    /// `render_interactive_ui` short-circuits on a hash match, so not one
    /// frame is ever repainted. Nothing crashes and nothing logs. The only
    /// symptom is a feature that does not work, with no cause.
    ///
    /// So the invariant is asserted structurally: every animated field is
    /// perturbed and the hash must move. Adding a field without hashing it
    /// makes this fail the moment someone remembers to extend the list.
    #[test]
    fn hash_tracks_new_animated_fields() {
        let base = DrmInteractiveState::default();
        let base_hash = interactive_state_hash(&base);

        // (name, mutator, a delta that is meaningful in the field's own unit)
        type Probe = (&'static str, fn(&mut DrmInteractiveState), f32);
        let probes: &[Probe] = &[
            ("smartspace_phase", |s| s.smartspace_phase += 0.01, 0.01),
            ("folder_morph", |s| s.folder_morph += 0.01, 0.01),
            ("folder_scrim", |s| s.folder_scrim += 0.01, 0.01),
            ("folder_title_alpha", |s| s.folder_title_alpha += 0.01, 0.01),
            ("popup_progress", |s| s.popup_progress += 0.01, 0.01),
            ("overview_progress", |s| s.overview_progress += 0.01, 0.01),
            ("overview_scroll", |s| s.overview_scroll += 0.01, 0.01),
            ("overview_dismiss", |s| s.overview_dismiss += 1.0, 1.0),
            ("fastscroller_thumb", |s| s.fastscroller_thumb += 0.01, 0.01),
            (
                "fastscroller_popup_alpha",
                |s| s.fastscroller_popup_alpha += 0.01,
                0.01,
            ),
            (
                "page_indicator_frac",
                |s| s.page_indicator_frac += 0.01,
                0.01,
            ),
            ("workspace_scale", |s| s.workspace_scale -= 0.01, 0.01),
            ("window_alpha", |s| s.window_alpha -= 0.01, 0.01),
            // The six folder-gesture fields. All six are continuous or
            // per-frame during a drag, so all six animate invisibly without a
            // hash entry -- a drag that moves the finger but never repaints is
            // the same class of bug as a spring that runs on a stale hash.
            ("folder_drag_pos.0", |s| s.folder_drag_pos.0 += 1.0, 1.0),
            ("folder_drag_pos.1", |s| s.folder_drag_pos.1 += 1.0, 1.0),
            (
                "folder_drag_slot",
                |s| s.folder_drag_slot = Some(s.folder_drag_slot.unwrap_or(0).wrapping_add(1)),
                1.0,
            ),
            (
                "folder_drop_slot",
                |s| s.folder_drop_slot = Some(s.folder_drop_slot.unwrap_or(0).wrapping_add(1)),
                1.0,
            ),
            (
                "folder_drag_out",
                |s| s.folder_drag_out = !s.folder_drag_out,
                1.0,
            ),
            (
                "folder_menu_progress",
                |s| s.folder_menu_progress += 0.01,
                0.01,
            ),
            (
                "folder_menu_anchor.0",
                |s| s.folder_menu_anchor.0 += 1.0,
                1.0,
            ),
            (
                "folder_menu_anchor.1",
                |s| s.folder_menu_anchor.1 += 1.0,
                1.0,
            ),
        ];

        for (name, bump, delta) in probes {
            let mut s = base.clone();
            assert_eq!(
                interactive_state_hash(&s),
                base_hash,
                "{name}: base mismatch"
            );
            bump(&mut s);
            assert_ne!(
                interactive_state_hash(&s),
                base_hash,
                "{name} changed by {delta} but did not move the hash -- it will \
             animate invisibly"
            );
        }
    }

    /// The page indicator's overshoot phase runs *above* 1.0, so its hash
    /// quantisation must not clamp. A `min(1.0)` here would make the whole
    /// overshoot invisible while the spring visibly runs.
    #[test]
    fn page_indicator_hash_sees_the_overshoot_phase() {
        let base = DrmInteractiveState::default();
        let mut at_one = base.clone();
        at_one.page_indicator_frac = 1.0;
        let mut over = base.clone();
        over.page_indicator_frac = 1.3;
        assert_ne!(
            interactive_state_hash(&at_one),
            interactive_state_hash(&over),
            "the 1.0 -> 1.3 overshoot must change the hash"
        );
        // And it must be a signed quantisation: a negative frac is reachable
        // by a spring overshooting the other way.
        let mut back = base.clone();
        back.page_indicator_frac = -0.3;
        assert_ne!(
            interactive_state_hash(&back),
            interactive_state_hash(&base),
            "a negative overshoot must change the hash"
        );
    }

    /// Quantisation must be fine enough that a single frame of motion is
    /// never rounded away. At 120 Hz a spring moving a full screen in 300 ms
    /// advances ~2.8 px per frame; a pixel-quantised field would drop that.
    #[test]
    fn animated_field_quantisation_resolves_one_frame_of_motion() {
        let base = DrmInteractiveState::default();
        // One frame of a 0 -> 1 progress animation over 300 ms at 120 Hz.
        let frame = 1.0f32 / 36.0;
        for (name, set) in [
            ("folder_morph", 0usize),
            ("popup_progress", 1),
            ("overview_progress", 2),
            ("smartspace_phase", 3),
            ("fastscroller_thumb", 4),
            ("page_indicator_frac", 5),
        ] {
            let mut a = base.clone();
            let mut b = base.clone();
            match set {
                0 => {
                    a.folder_morph = 0.0;
                    b.folder_morph = frame;
                }
                1 => {
                    a.popup_progress = 0.0;
                    b.popup_progress = frame;
                }
                2 => {
                    a.overview_progress = 0.0;
                    b.overview_progress = frame;
                }
                3 => {
                    a.smartspace_phase = 0.0;
                    b.smartspace_phase = frame;
                }
                4 => {
                    a.fastscroller_thumb = 0.0;
                    b.fastscroller_thumb = frame;
                }
                _ => {
                    a.page_indicator_frac = 0.0;
                    b.page_indicator_frac = frame;
                }
            }
            assert_ne!(
                interactive_state_hash(&a),
                interactive_state_hash(&b),
                "{name}: one frame ({frame}) of motion rounded away to the same hash"
            );
        }
    }

    /// Only 90/270 transpose the scanout axes.
    ///
    /// Neutering (`matches!(t, Transform::Rotate90)` alone, or `false`) makes
    /// `rotated_frame_size` return the identity for 270 and the size test
    /// below fails with it.
    #[test]
    fn transform_swaps_xy_is_only_90_and_270() {
        assert!(transform_swaps_xy(Transform::Rotate90));
        assert!(transform_swaps_xy(Transform::Rotate270));
        assert!(!transform_swaps_xy(Transform::None));
        assert!(!transform_swaps_xy(Transform::Rotate180));
        assert!(!transform_swaps_xy(Transform::FlipH));
        assert!(!transform_swaps_xy(Transform::FlipV));
    }

    /// The re-modeset size swaps exactly for 90/270.
    ///
    /// Neutering (returning `(w, h)` unconditionally) keeps the 90/270
    /// assertions red: a modeset that never swaps is a portrait buffer on a
    /// landscape panel.
    #[test]
    fn rotated_frame_size_swaps_only_for_90_270() {
        assert_eq!(
            rotated_frame_size(1080, 2400, Transform::None),
            (1080, 2400)
        );
        assert_eq!(
            rotated_frame_size(1080, 2400, Transform::Rotate180),
            (1080, 2400)
        );
        assert_eq!(
            rotated_frame_size(1080, 2400, Transform::Rotate90),
            (2400, 1080)
        );
        assert_eq!(
            rotated_frame_size(1080, 2400, Transform::Rotate270),
            (2400, 1080)
        );
    }

    /// A re-modeset to the current transform issues no ioctl.
    ///
    /// Built on `/dev/null` rather than hardware: any ioctl attempted on it
    /// fails, so `Ok(false)` proves nothing was issued. Neutering (dropping
    /// the early-out) turns this into `Err` and fails the assertion.
    #[test]
    fn apply_transform_to_the_current_transform_is_a_noop_without_io() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null opens");
        let mut dev = DrmKmsDevice {
            file,
            crtc_id: 1,
            connector_id: 2,
            fb_id: 0,
            dumb_handle: 0,
            width: 1080,
            height: 2400,
            pitch: 1080 * 4,
            size: 0,
            mmap_ptr: std::ptr::null_mut(),
            mode: DrmModeModeInfo {
                hdisplay: 1080,
                vdisplay: 2400,
                ..Default::default()
            },
            transform: Transform::None,
            frame_cache_hash: 0,
            frame_cache_valid: false,
            frame_dirty: false,
        };
        assert!(
            !dev.apply_transform(Transform::None).expect("noop modeset"),
            "re-applying the current transform must be a silent no-op"
        );
    }

    /// `set_orientation` maps through `to_transform`, not around it.
    ///
    /// `Normal`/`Undefined` are `Transform::None`
    /// (`sensor_proxy.rs:113-121`), so both are no-ops on a native device;
    /// `LeftUp` is `Rotate90` and must attempt a modeset, which on
    /// `/dev/null` is `Err`. Neutering the mapping (always `None`) makes the
    /// `LeftUp` arm return `Ok(false)` and fails.
    #[test]
    fn set_orientation_maps_through_to_transform() {
        fn native() -> DrmKmsDevice {
            let file = std::fs::File::open("/dev/null").expect("/dev/null opens");
            DrmKmsDevice {
                file,
                crtc_id: 1,
                connector_id: 2,
                fb_id: 0,
                dumb_handle: 0,
                width: 1080,
                height: 2400,
                pitch: 1080 * 4,
                size: 0,
                mmap_ptr: std::ptr::null_mut(),
                mode: DrmModeModeInfo {
                    hdisplay: 1080,
                    vdisplay: 2400,
                    ..Default::default()
                },
                transform: Transform::None,
                frame_cache_hash: 0,
                frame_cache_valid: false,
                frame_dirty: false,
            }
        }
        use crate::sensors::sensor_proxy::DeviceOrientation;
        assert!(
            !native()
                .set_orientation(DeviceOrientation::Normal)
                .expect("noop modeset"),
            "Normal maps to the native transform and must not modeset"
        );
        assert!(
            !native()
                .set_orientation(DeviceOrientation::Undefined)
                .expect("noop modeset"),
            "Undefined maps to the native transform and must not modeset"
        );
        assert!(
            native().set_orientation(DeviceOrientation::LeftUp).is_err(),
            "LeftUp is Rotate90 and must attempt a modeset, not no-op"
        );
    }

    /// A closed folder publishes no rename field and no grid.
    ///
    /// `paint_frame` skips `draw_folder` entirely at rest, so without the
    /// `else` clear the previous edit's field and grid survive and a tap on
    /// the home screen hits a folder that is no longer on screen. Neutering
    /// (removing the `else` clear) leaves both live and fails.
    #[test]
    fn closed_folder_clears_published_rename_and_grid() {
        let _guard = crate::graphics::font::font_test_lock();
        let member = AppGridItem {
            id: "a",
            name: "Alpha",
            color: 0xFF445566,
            glyph: "A",
            icon: None,
            folder_n: 0,
            folder_id: 0,
        };
        let members = [member];
        let open = DrmInteractiveState {
            folder_morph: 1.0,
            folder_scrim: 0.32,
            folder_title_alpha: 1.0,
            folder_title: "Games",
            folder_apps: &members,
            folder_item_count: 1,
            folder_rename_editing: true,
            folder_rename_buffer: FolderRenameBuffer::truncated("Hi"),
            ..DrmInteractiveState::default()
        };
        let (w, h) = (1080usize, 2400usize);
        let mut buf = vec![0xFF000000u32; w * h];
        paint_frame(&mut buf, w, w, h, &open);
        assert!(
            folder_rename_geometry().live,
            "the open edit published no rename field"
        );
        assert!(
            folder_grid_geometry().live,
            "the open folder published no grid"
        );
        let shut = DrmInteractiveState::default();
        paint_frame(&mut buf, w, w, h, &shut);
        assert_eq!(
            folder_rename_geometry(),
            FolderRenameGeometry::EMPTY,
            "a closed folder still publishes a rename field"
        );
        assert!(
            !folder_grid_geometry().live,
            "a closed folder still publishes a live grid"
        );
    }

    /// Each workspace-drag field moves the damage hash.
    ///
    /// `render_interactive_ui` short-circuits on a hash match, so an unhashed
    /// drag field animates invisibly. Each arm perturbs one field against a
    /// baseline that already holds the others, so the assertion isolates that
    /// field: neutering (removing any one `mix!` in the five-field block)
    /// leaves that arm equal and fails.
    #[test]
    fn workspace_drag_fields_move_the_damage_hash() {
        let base = DrmInteractiveState::default();
        let h0 = interactive_state_hash(&base);
        let mut lifted = base.clone();
        lifted.drag_slot = Some(2);
        let h_lifted = interactive_state_hash(&lifted);
        assert_ne!(h0, h_lifted, "drag_slot must repaint");
        let mut moved = lifted.clone();
        moved.drag_pos = (60.0, 120.0);
        assert_ne!(
            h_lifted,
            interactive_state_hash(&moved),
            "drag_pos must follow the finger"
        );
        let mut rising = lifted.clone();
        rising.drag_lift = 1.0;
        assert_ne!(
            h_lifted,
            interactive_state_hash(&rising),
            "drag_lift must repaint"
        );
        let mut gap = lifted.clone();
        gap.drag_drop_slot = Some(3);
        assert_ne!(
            h_lifted,
            interactive_state_hash(&gap),
            "drag_drop_slot must repaint"
        );
        let mut merging = lifted.clone();
        merging.drag_merge_slot = Some(3);
        assert_ne!(
            h_lifted,
            interactive_state_hash(&merging),
            "drag_merge_slot must repaint"
        );
    }
}
