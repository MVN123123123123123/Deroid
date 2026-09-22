//! Hardware Composer (HWC) abstraction supporting HIDL (composer@2.1-2.4)
//! and Stable AIDL (android.hardware.graphics.composer3).
//! Implements multi-plane hardware composition, layer transforms, and presentation fencing.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use super::vsync::VsyncConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwcVersion {
    Hwc2_1,        // HIDL @2.1
    Hwc2_2,        // HIDL @2.2
    Hwc2_3,        // HIDL @2.3
    Hwc2_4,        // HIDL @2.4
    AidlComposer3, // Stable AIDL composer3 (Android 13 - 16+)
}

impl HwcVersion {
    pub fn is_aidl(&self) -> bool {
        matches!(self, HwcVersion::AidlComposer3)
    }

    pub fn binder_device(&self) -> &'static str {
        if self.is_aidl() {
            "/dev/binder"
        } else {
            "/dev/hwbinder"
        }
    }

    pub fn service_name(&self) -> &'static str {
        if self.is_aidl() {
            "android.hardware.graphics.composer3.IComposer/default"
        } else {
            "android.hardware.graphics.composer@2.1::IComposer/default"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CompositionType {
    Invalid = 0,
    Client = 1,     // Composited into ClientTarget by Wayland/GPU
    Device = 2,     // Hardware scanout overlay plane
    SolidColor = 3, // Hardware-drawn solid color box
    Cursor = 4,     // Dedicated hardware cursor plane
    Sideband = 5,   // Hardware video decoder direct stream
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Transform {
    None = 0,
    FlipH = 1,
    FlipV = 2,
    Rotate180 = 3,
    Rotate90 = 4,
    Rotate270 = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum BlendMode {
    None = 0,
    Premultiplied = 1,
    Coverage = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayConfig {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub vsync: VsyncConfig,
    pub max_overlay_planes: usize,
}

impl DisplayConfig {
    pub fn standard_mobile(display_id: u32, width: u32, height: u32, refresh_rate_hz: f64) -> Self {
        let vsync =
            VsyncConfig::new(refresh_rate_hz).unwrap_or_else(|_| VsyncConfig::for_rate_60hz());
        Self {
            display_id,
            width,
            height,
            dpi_x: 440,
            dpi_y: 440,
            vsync,
            max_overlay_planes: 4, // Typical mobile display controller (e.g. Qualcomm DPU / ARM Mali DVALIN)
        }
    }
}

fn dup_cloexec_i32(fd: i32) -> Option<i32> {
    if fd >= 0 {
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup >= 0 {
            Some(dup)
        } else {
            None
        }
    } else {
        Some(fd)
    }
}

#[derive(Debug, PartialEq)]
pub struct HwcLayer {
    pub id: u64,
    pub z_order: u32,
    pub transform: Transform,
    pub blend_mode: BlendMode,
    pub source_crop: Rect,
    pub display_frame: Rect,
    pub plane_alpha: f32,
    pub composition_type: CompositionType,
    pub requested_composition_type: CompositionType,
    pub buffer_handle: Option<u64>,
    pub acquire_fence: Option<i32>,
    pub release_fence: Option<i32>,
}

impl Clone for HwcLayer {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            z_order: self.z_order,
            transform: self.transform,
            blend_mode: self.blend_mode,
            source_crop: self.source_crop,
            display_frame: self.display_frame,
            plane_alpha: self.plane_alpha,
            composition_type: self.composition_type,
            requested_composition_type: self.requested_composition_type,
            buffer_handle: self.buffer_handle,
            acquire_fence: self.acquire_fence.and_then(dup_cloexec_i32),
            release_fence: self.release_fence.and_then(dup_cloexec_i32),
        }
    }
}

impl Drop for HwcLayer {
    fn drop(&mut self) {
        if let Some(f) = self.acquire_fence.take() {
            if f >= 0 {
                unsafe { libc::close(f) };
            }
        }
        if let Some(f) = self.release_fence.take() {
            if f >= 0 {
                unsafe { libc::close(f) };
            }
        }
    }
}

impl HwcLayer {
    pub fn new(id: u64, z_order: u32) -> Self {
        Self {
            id,
            z_order,
            transform: Transform::None,
            blend_mode: BlendMode::Premultiplied,
            source_crop: Rect::default(),
            display_frame: Rect::default(),
            plane_alpha: 1.0,
            composition_type: CompositionType::Device,
            requested_composition_type: CompositionType::Device,
            buffer_handle: None,
            acquire_fence: None,
            release_fence: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwcError {
    DisplayNotFound(u32),
    LayerNotFound(u64),
    UnsupportedTransform(u32),
    UnsupportedBlendMode(u32),
    ValidationFailed(String),
    PresentationFailed(String),
    NoClientTarget,
}

impl fmt::Display for HwcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HwcError::DisplayNotFound(id) => write!(f, "Display ID {} not found", id),
            HwcError::LayerNotFound(id) => write!(f, "Layer ID {} not found", id),
            HwcError::UnsupportedTransform(t) => write!(f, "Unsupported transform: {}", t),
            HwcError::UnsupportedBlendMode(b) => write!(f, "Unsupported blend mode: {}", b),
            HwcError::ValidationFailed(s) => write!(f, "HWC display validation failed: {}", s),
            HwcError::PresentationFailed(s) => write!(f, "HWC presentation failed: {}", s),
            HwcError::NoClientTarget => write!(
                f,
                "Client composition required but no ClientTarget buffer set"
            ),
        }
    }
}

impl std::error::Error for HwcError {}

/// Presentation output: present fence and per-layer release fences.
pub type PresentFences = (Option<i32>, HashMap<u64, Option<i32>>);

/// Dynamic Hardware Composer Bridge (supports AIDL composer3 and HIDL composer@2.x).
pub struct HwcComposer {
    version: HwcVersion,
    displays: HashMap<u32, DisplayConfig>,
    layers: HashMap<u32, HashMap<u64, HwcLayer>>,
    client_targets: HashMap<u32, (u64, Option<i32>)>, // display_id -> (buffer_handle, fence)
    next_layer_id: u64,
    validated: HashMap<u32, bool>,
}

impl HwcComposer {
    pub fn new(version: HwcVersion) -> Self {
        Self {
            version,
            displays: HashMap::new(),
            layers: HashMap::new(),
            client_targets: HashMap::new(),
            next_layer_id: 1,
            validated: HashMap::new(),
        }
    }

    pub fn version(&self) -> HwcVersion {
        self.version
    }

    /// Auto-detect Composer version from vendor manifest or existing system environment.
    pub fn detect_version_from_manifest(manifest_content: Option<&str>) -> HwcVersion {
        if let Some(content) = manifest_content {
            if content.contains("android.hardware.graphics.composer3")
                || (content.contains("android.hardware.graphics.composer")
                    && content.contains("aidl"))
            {
                return HwcVersion::AidlComposer3;
            }
            if content.contains("android.hardware.graphics.composer@2.4") {
                return HwcVersion::Hwc2_4;
            }
            if content.contains("android.hardware.graphics.composer@2.3") {
                return HwcVersion::Hwc2_3;
            }
            if content.contains("android.hardware.graphics.composer@2.2") {
                return HwcVersion::Hwc2_2;
            }
            if content.contains("android.hardware.graphics.composer@2.1") {
                return HwcVersion::Hwc2_1;
            }
        }

        // Fallback: Check Binder device nodes
        if Path::new("/dev/binder").exists() && !Path::new("/dev/hwbinder").exists() {
            HwcVersion::AidlComposer3
        } else {
            HwcVersion::Hwc2_1
        }
    }

    pub fn register_display(&mut self, config: DisplayConfig) {
        let display_id = config.display_id;
        self.displays.insert(display_id, config);
        // Overwriting layers triggers Drop on previous HwcLayer instances, safely closing fences
        self.layers.insert(display_id, HashMap::new());
        if let Some((_, Some(old_fence))) = self.client_targets.remove(&display_id) {
            if old_fence >= 0 {
                unsafe { libc::close(old_fence) };
            }
        }
        self.validated.insert(display_id, false);
    }

    pub fn display_config(&self, display_id: u32) -> Option<&DisplayConfig> {
        self.displays.get(&display_id)
    }

    pub fn create_layer(&mut self, display_id: u32) -> Result<u64, HwcError> {
        let display_layers = self
            .layers
            .get_mut(&display_id)
            .ok_or(HwcError::DisplayNotFound(display_id))?;

        let layer_id = self.next_layer_id;
        self.next_layer_id = self.next_layer_id.wrapping_add(1);

        let layer = HwcLayer::new(layer_id, 0);
        display_layers.insert(layer_id, layer);
        self.validated.insert(display_id, false);

        Ok(layer_id)
    }

    pub fn destroy_layer(&mut self, display_id: u32, layer_id: u64) -> Result<(), HwcError> {
        let display_layers = self
            .layers
            .get_mut(&display_id)
            .ok_or(HwcError::DisplayNotFound(display_id))?;

        // Removing layer triggers Drop::drop, closing acquire and release fences
        display_layers
            .remove(&layer_id)
            .ok_or(HwcError::LayerNotFound(layer_id))?;

        self.validated.insert(display_id, false);
        Ok(())
    }

    pub fn set_layer_composition_type(
        &mut self,
        display_id: u32,
        layer_id: u64,
        comp_type: CompositionType,
    ) -> Result<(), HwcError> {
        let layer = self.get_layer_mut(display_id, layer_id)?;
        layer.requested_composition_type = comp_type;
        layer.composition_type = comp_type;
        self.validated.insert(display_id, false);
        Ok(())
    }

    pub fn set_layer_buffer(
        &mut self,
        display_id: u32,
        layer_id: u64,
        buffer_handle: u64,
        acquire_fence: Option<i32>,
    ) -> Result<(), HwcError> {
        let layer = self.get_layer_mut(display_id, layer_id)?;
        if let Some(old_fence) = layer.acquire_fence.take() {
            if old_fence >= 0 {
                unsafe { libc::close(old_fence) };
            }
        }
        layer.buffer_handle = Some(buffer_handle);
        layer.acquire_fence = acquire_fence;
        Ok(())
    }

    pub fn set_layer_z_order(
        &mut self,
        display_id: u32,
        layer_id: u64,
        z_order: u32,
    ) -> Result<(), HwcError> {
        let layer = self.get_layer_mut(display_id, layer_id)?;
        layer.z_order = z_order;
        self.validated.insert(display_id, false);
        Ok(())
    }

    pub fn set_layer_display_frame(
        &mut self,
        display_id: u32,
        layer_id: u64,
        frame: Rect,
    ) -> Result<(), HwcError> {
        let layer = self.get_layer_mut(display_id, layer_id)?;
        layer.display_frame = frame;
        self.validated.insert(display_id, false);
        Ok(())
    }

    pub fn set_layer_source_crop(
        &mut self,
        display_id: u32,
        layer_id: u64,
        crop: Rect,
    ) -> Result<(), HwcError> {
        let layer = self.get_layer_mut(display_id, layer_id)?;
        layer.source_crop = crop;
        self.validated.insert(display_id, false);
        Ok(())
    }

    pub fn set_layer_transform(
        &mut self,
        display_id: u32,
        layer_id: u64,
        transform: Transform,
    ) -> Result<(), HwcError> {
        let layer = self.get_layer_mut(display_id, layer_id)?;
        layer.transform = transform;
        self.validated.insert(display_id, false);
        Ok(())
    }

    pub fn set_client_target(
        &mut self,
        display_id: u32,
        buffer_handle: u64,
        acquire_fence: Option<i32>,
    ) -> Result<(), HwcError> {
        if !self.displays.contains_key(&display_id) {
            return Err(HwcError::DisplayNotFound(display_id));
        }
        if let Some((_, Some(old_fence))) = self
            .client_targets
            .insert(display_id, (buffer_handle, acquire_fence))
        {
            if old_fence >= 0 {
                unsafe { libc::close(old_fence) };
            }
        }
        Ok(())
    }

    /// Multi-plane hardware composition validation:
    /// Inspects layers, hardware overlay capacity, and assigns layers to Device overlays or Client composition.
    /// Hardware constraint: When Client composition is used, ClientTarget itself consumes 1 hardware overlay plane.
    /// Returns: (number_of_changed_layers, has_client_composition)
    pub fn validate_display(&mut self, display_id: u32) -> Result<(usize, bool), HwcError> {
        let display_config = self
            .displays
            .get(&display_id)
            .ok_or(HwcError::DisplayNotFound(display_id))?
            .clone();

        let display_layers = self
            .layers
            .get_mut(&display_id)
            .ok_or(HwcError::DisplayNotFound(display_id))?;

        // Collect and sort layers by z_order ascending
        let mut sorted_layer_ids: Vec<(u64, u32)> = display_layers
            .iter()
            .map(|(&id, l)| (id, l.z_order))
            .collect();
        sorted_layer_ids.sort_by_key(|&(_, z)| z);

        let max_planes = display_config.max_overlay_planes;

        // Check if any layer requested client or if total requested hardware layers exceeds max_planes
        let requested_client_count = display_layers
            .values()
            .filter(|l| l.requested_composition_type == CompositionType::Client)
            .count();
        let requested_hw_count = display_layers
            .values()
            .filter(|l| l.requested_composition_type != CompositionType::Client)
            .count();

        // If any layer is Client, or if requested hardware layers exceed max_planes,
        // client composition is required. ClientTarget itself consumes 1 hardware plane.
        let requires_client = requested_client_count > 0 || requested_hw_count > max_planes;
        let max_device_planes = if requires_client {
            max_planes.saturating_sub(1)
        } else {
            max_planes
        };

        let mut overlay_planes_used = 0;
        let mut changed_count = 0;
        let mut has_client = false;

        for (id, _) in sorted_layer_ids {
            let layer = display_layers.get_mut(&id).unwrap();

            if layer.requested_composition_type == CompositionType::Device {
                if overlay_planes_used < max_device_planes {
                    // Fits in a hardware overlay plane
                    if layer.composition_type != CompositionType::Device {
                        layer.composition_type = CompositionType::Device;
                        changed_count += 1;
                    }
                    overlay_planes_used += 1;
                } else {
                    // Exceeds hardware plane capacity -> demote to Client composition
                    if layer.composition_type != CompositionType::Client {
                        layer.composition_type = CompositionType::Client;
                        changed_count += 1;
                    }
                    has_client = true;
                }
            } else if layer.requested_composition_type == CompositionType::Client {
                if layer.composition_type != CompositionType::Client {
                    layer.composition_type = CompositionType::Client;
                    changed_count += 1;
                }
                has_client = true;
            } else {
                // SolidColor, Cursor, Sideband
                if overlay_planes_used < max_device_planes {
                    if layer.composition_type != layer.requested_composition_type {
                        layer.composition_type = layer.requested_composition_type;
                        changed_count += 1;
                    }
                    overlay_planes_used += 1;
                } else {
                    if layer.composition_type != CompositionType::Client {
                        layer.composition_type = CompositionType::Client;
                        changed_count += 1;
                    }
                    has_client = true;
                }
            }
        }

        self.validated.insert(display_id, true);
        Ok((changed_count, has_client))
    }

    pub fn accept_display_changes(&mut self, display_id: u32) -> Result<(), HwcError> {
        if !self.displays.contains_key(&display_id) {
            return Err(HwcError::DisplayNotFound(display_id));
        }
        self.validated.insert(display_id, true);
        Ok(())
    }

    /// Present display to screen:
    /// Validates readiness, generates simulated or real presentation fence,
    /// consumes acquire fences, and issues release fences per layer.
    pub fn present_display(&mut self, display_id: u32) -> Result<PresentFences, HwcError> {
        let is_val = self.validated.get(&display_id).copied().unwrap_or(false);
        if !is_val {
            return Err(HwcError::ValidationFailed(
                "Display must be validated before presentation".into(),
            ));
        }

        let display_layers = self
            .layers
            .get_mut(&display_id)
            .ok_or(HwcError::DisplayNotFound(display_id))?;

        let mut needs_client = false;
        for layer in display_layers.values() {
            if layer.composition_type == CompositionType::Client {
                needs_client = true;
                break;
            }
        }

        if needs_client && !self.client_targets.contains_key(&display_id) {
            return Err(HwcError::NoClientTarget);
        }

        // Close acquire fences since presentation consumes them
        for layer in display_layers.values_mut() {
            if let Some(acq) = layer.acquire_fence.take() {
                if acq >= 0 {
                    unsafe { libc::close(acq) };
                }
            }
        }

        // Close ClientTarget acquire fence since presentation consumes it
        if let Some((_, fence_opt)) = self.client_targets.get_mut(&display_id) {
            if let Some(acq) = fence_opt.take() {
                if acq >= 0 {
                    unsafe { libc::close(acq) };
                }
            }
        }

        // Generate mock fences (in production backed by sync_file / sync_fence)
        let present_fence = Some(-1); // -1 signifies immediately signaled / no fence needed

        let mut release_fences = HashMap::new();
        for (&id, layer) in display_layers.iter_mut() {
            if let Some(old_rel) = layer.release_fence.take() {
                if old_rel >= 0 {
                    unsafe { libc::close(old_rel) };
                }
            }
            layer.release_fence = Some(-1);
            release_fences.insert(id, Some(-1));
        }

        Ok((present_fence, release_fences))
    }

    fn get_layer_mut(&mut self, display_id: u32, layer_id: u64) -> Result<&mut HwcLayer, HwcError> {
        let display_layers = self
            .layers
            .get_mut(&display_id)
            .ok_or(HwcError::DisplayNotFound(display_id))?;
        display_layers
            .get_mut(&layer_id)
            .ok_or(HwcError::LayerNotFound(layer_id))
    }
}

impl Drop for HwcComposer {
    fn drop(&mut self) {
        for layers in self.layers.values_mut() {
            for layer in layers.values_mut() {
                if let Some(f) = layer.acquire_fence.take() {
                    if f >= 0 {
                        unsafe { libc::close(f) };
                    }
                }
                if let Some(f) = layer.release_fence.take() {
                    if f >= 0 {
                        unsafe { libc::close(f) };
                    }
                }
            }
        }
        for (_, fence) in self.client_targets.values_mut() {
            if let Some(f) = fence.take() {
                if f >= 0 {
                    unsafe { libc::close(f) };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hwc_version_detection() {
        let manifest_aidl = r#"
            <hal format="aidl">
                <name>android.hardware.graphics.composer3</name>
            </hal>
        "#;
        assert_eq!(
            HwcComposer::detect_version_from_manifest(Some(manifest_aidl)),
            HwcVersion::AidlComposer3
        );

        let manifest_hidl = r#"
            <hal format="hidl">
                <name>android.hardware.graphics.composer@2.4</name>
            </hal>
        "#;
        assert_eq!(
            HwcComposer::detect_version_from_manifest(Some(manifest_hidl)),
            HwcVersion::Hwc2_4
        );
    }

    #[test]
    fn test_hwc_layer_lifecycle_and_validation() {
        let mut hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let config = DisplayConfig::standard_mobile(0, 1080, 2400, 120.0);
        hwc.register_display(config);

        let l1 = hwc.create_layer(0).expect("Create layer 1");
        let l2 = hwc.create_layer(0).expect("Create layer 2");

        hwc.set_layer_z_order(0, l1, 1).unwrap();
        hwc.set_layer_z_order(0, l2, 2).unwrap();

        hwc.set_layer_composition_type(0, l1, CompositionType::Device)
            .unwrap();
        hwc.set_layer_composition_type(0, l2, CompositionType::Device)
            .unwrap();

        hwc.set_layer_buffer(0, l1, 101, None).unwrap();
        hwc.set_layer_buffer(0, l2, 102, None).unwrap();

        let (changed, has_client) = hwc.validate_display(0).expect("Validate display");
        assert_eq!(changed, 0); // Both fit in 4 overlay planes
        assert!(!has_client);

        let (present_fence, release_fences) = hwc.present_display(0).expect("Present display");
        assert!(present_fence.is_some());
        assert_eq!(release_fences.len(), 2);
    }

    #[test]
    fn test_hwc_multi_plane_overlay_exhaustion() {
        let mut hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut config = DisplayConfig::standard_mobile(0, 1080, 2400, 60.0);
        config.max_overlay_planes = 2; // Only 2 overlay planes available
        hwc.register_display(config);

        let l1 = hwc.create_layer(0).unwrap();
        let l2 = hwc.create_layer(0).unwrap();
        let l3 = hwc.create_layer(0).unwrap();

        hwc.set_layer_z_order(0, l1, 1).unwrap();
        hwc.set_layer_z_order(0, l2, 2).unwrap();
        hwc.set_layer_z_order(0, l3, 3).unwrap();

        hwc.set_layer_composition_type(0, l1, CompositionType::Device)
            .unwrap();
        hwc.set_layer_composition_type(0, l2, CompositionType::Device)
            .unwrap();
        hwc.set_layer_composition_type(0, l3, CompositionType::Device)
            .unwrap();

        let (changed, has_client) = hwc.validate_display(0).expect("Validate");
        // Because ClientTarget consumes 1 hardware plane, only 2-1=1 layer can be Device.
        // Layers l2 and l3 are both demoted to Client!
        assert_eq!(changed, 2);
        assert!(has_client);

        // Attempt present without ClientTarget must fail
        assert_eq!(
            hwc.present_display(0).unwrap_err(),
            HwcError::NoClientTarget
        );

        // Supply ClientTarget
        hwc.set_client_target(0, 999, None).unwrap();
        let (present_fence, release_fences) = hwc.present_display(0).expect("Present with target");
        assert!(present_fence.is_some());
        assert_eq!(release_fences.len(), 3);
    }

    fn make_test_fence_pair() -> (i32, i32) {
        let mut sv = [0i32; 2];
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0, "socketpair creation failed");
        (sv[0], sv[1])
    }

    fn assert_fence_closed_via_peer(peer_fd: i32, msg: &str) {
        let byte = [1u8; 1];
        let ret = unsafe {
            libc::send(
                peer_fd,
                byte.as_ptr() as *const libc::c_void,
                1,
                libc::MSG_NOSIGNAL,
            )
        };
        let errno = std::io::Error::last_os_error().raw_os_error();
        unsafe { libc::close(peer_fd) };
        assert_eq!(ret, -1, "{}: send should fail on closed peer", msg);
        assert_eq!(
            errno,
            Some(libc::EPIPE),
            "{}: expected EPIPE when peer is closed",
            msg
        );
    }

    #[test]
    fn test_hwc_fence_lifecycle_and_clean_drop() {
        let mut hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let config = DisplayConfig::standard_mobile(0, 1080, 2400, 60.0);
        hwc.register_display(config);

        let l1 = hwc.create_layer(0).unwrap();
        let (fence_fd, peer_fd) = make_test_fence_pair();

        hwc.set_layer_buffer(0, l1, 100, Some(fence_fd)).unwrap();
        hwc.validate_display(0).unwrap();

        // Presentation consumes and closes acquire fence
        let _ = hwc.present_display(0).unwrap();

        // Verify that fence_fd was closed by present_display without racing on fd reuse
        assert_fence_closed_via_peer(
            peer_fd,
            "Acquire fence fd should have been closed by present_display",
        );
    }

    #[test]
    fn test_hwc_cursor_and_device_plane_overflow() {
        let mut hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut config = DisplayConfig::standard_mobile(0, 1080, 2400, 60.0);
        config.max_overlay_planes = 2; // Only 2 overlay planes available
        hwc.register_display(config);

        let l_cursor = hwc.create_layer(0).unwrap();
        let l_dev1 = hwc.create_layer(0).unwrap();
        let l_dev2 = hwc.create_layer(0).unwrap();

        hwc.set_layer_z_order(0, l_cursor, 0).unwrap();
        hwc.set_layer_z_order(0, l_dev1, 1).unwrap();
        hwc.set_layer_z_order(0, l_dev2, 2).unwrap();

        hwc.set_layer_composition_type(0, l_cursor, CompositionType::Cursor)
            .unwrap();
        hwc.set_layer_composition_type(0, l_dev1, CompositionType::Device)
            .unwrap();
        hwc.set_layer_composition_type(0, l_dev2, CompositionType::Device)
            .unwrap();

        let (changed, has_client) = hwc.validate_display(0).expect("Validate display");
        // 3 non-client layers on 2-plane display -> requires client composition!
        // ClientTarget takes 1 plane, leaving 1 plane for Cursor.
        // Both l_dev1 and l_dev2 must be demoted to Client!
        assert!(has_client);
        assert_eq!(changed, 2);

        hwc.set_client_target(0, 888, None).unwrap();
        let (present_fence, release_fences) = hwc.present_display(0).unwrap();
        assert!(present_fence.is_some());
        assert_eq!(release_fences.len(), 3);
    }

    #[test]
    fn test_hwc_client_target_fence_consumed_on_presentation() {
        let mut hwc = HwcComposer::new(HwcVersion::AidlComposer3);
        let mut config = DisplayConfig::standard_mobile(0, 1080, 2400, 60.0);
        config.max_overlay_planes = 1;
        hwc.register_display(config);

        let l1 = hwc.create_layer(0).unwrap();
        let l2 = hwc.create_layer(0).unwrap();
        hwc.set_layer_composition_type(0, l1, CompositionType::Device)
            .unwrap();
        hwc.set_layer_composition_type(0, l2, CompositionType::Device)
            .unwrap();
        hwc.validate_display(0).unwrap();

        let (target_fence, target_peer) = make_test_fence_pair();
        hwc.set_client_target(0, 777, Some(target_fence)).unwrap();

        let _ = hwc
            .present_display(0)
            .expect("Present with client target fence");

        // Verify target_fence was consumed and closed
        assert_fence_closed_via_peer(
            target_peer,
            "ClientTarget acquire fence must be closed on presentation",
        );
    }

    #[test]
    fn test_hwc_layer_clone_and_drop_isolation() {
        let (fence1, peer1) = make_test_fence_pair();
        let (fence2, peer2) = make_test_fence_pair();

        let mut layer = HwcLayer::new(1, 0);
        layer.acquire_fence = Some(fence1);
        layer.release_fence = Some(fence2);

        let cloned_layer = layer.clone();
        assert_ne!(layer.acquire_fence, cloned_layer.acquire_fence);
        assert_ne!(layer.release_fence, cloned_layer.release_fence);

        // Dropping cloned_layer must not close layer's fences
        drop(cloned_layer);

        let flags1 = unsafe { libc::fcntl(fence1, libc::F_GETFD) };
        let flags2 = unsafe { libc::fcntl(fence2, libc::F_GETFD) };
        assert!(flags1 >= 0);
        assert!(flags2 >= 0);

        drop(layer);
        assert_fence_closed_via_peer(peer1, "Layer acquire fence must be closed on drop");
        assert_fence_closed_via_peer(peer2, "Layer release fence must be closed on drop");
    }
}
