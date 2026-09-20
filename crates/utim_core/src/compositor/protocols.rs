//! Wayland protocol engine and mobile protocol extensions.
//! Provides zero-copy wire protocol parsing, serializing, and mobile interface definitions
//! conforming to xdg-shell, wlr-layer-shell, linux-dmabuf, presentation-time, wp-viewporter,
//! ext-idle-notify, text-input-v3, and zwp-tablet-v2.


pub const WAYLAND_VERSION_MAJOR: u32 = 1;
pub const WAYLAND_VERSION_MINOR: u32 = 22;

/// Standard Wayland Wire Message Header
/// Size: 8 bytes (4 bytes object ID, 2 bytes opcode, 2 bytes length)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WlHeader {
    pub object_id: u32,
    pub opcode: u16,
    pub length: u16,
}

impl WlHeader {
    pub fn new(object_id: u32, opcode: u16, length: u16) -> Self {
        Self {
            object_id,
            opcode,
            length,
        }
    }

    pub fn to_bytes(&self) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&self.object_id.to_ne_bytes());
        buf[4..6].copy_from_slice(&self.opcode.to_ne_bytes());
        buf[6..8].copy_from_slice(&self.length.to_ne_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let object_id = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let opcode = u16::from_ne_bytes([bytes[4], bytes[5]]);
        let length = u16::from_ne_bytes([bytes[6], bytes[7]]);
        if length < 8 {
            return None;
        }
        Some(Self {
            object_id,
            opcode,
            length,
        })
    }
}

/// Parsed Wayland message payload
#[derive(Debug, Clone, PartialEq)]
pub struct WlMessage<'a> {
    pub header: WlHeader,
    pub payload: &'a [u8],
}

impl<'a> WlMessage<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Option<(Self, usize)>, &'static str> {
        if buf.len() < 8 {
            return Ok(None);
        }
        let header = match WlHeader::from_bytes(buf) {
            Some(h) => h,
            None => return Err("Invalid Wayland header or length < 8"),
        };
        let total_len = header.length as usize;
        if buf.len() < total_len {
            return Ok(None);
        }
        let payload = &buf[8..total_len];
        Ok(Some((Self { header, payload }, total_len)))
    }

    pub fn read_u32(&self, offset: usize) -> Option<u32> {
        if offset + 4 <= self.payload.len() {
            Some(u32::from_ne_bytes([
                self.payload[offset],
                self.payload[offset + 1],
                self.payload[offset + 2],
                self.payload[offset + 3],
            ]))
        } else {
            None
        }
    }

    pub fn read_i32(&self, offset: usize) -> Option<i32> {
        self.read_u32(offset).map(|v| v as i32)
    }

    /// Read Wayland 24.8 fixed point number as f32
    pub fn read_fixed(&self, offset: usize) -> Option<f32> {
        self.read_i32(offset).map(|v| (v as f32) / 256.0)
    }

    /// Read null-terminated string padded to 4-byte boundary
    pub fn read_string(&self, offset: usize) -> Option<(&'a str, usize)> {
        let len = self.read_u32(offset)? as usize;
        if len == 0 {
            return Some(("", offset + 4));
        }
        let start = offset + 4;
        let end = start + len;
        if end > self.payload.len() {
            return None;
        }
        let slice = &self.payload[start..end];
        let trimmed = if slice.last() == Some(&0) {
            &slice[..slice.len() - 1]
        } else {
            slice
        };
        let s = std::str::from_utf8(trimmed).ok()?;
        let padded_len = (len + 3) & !3;
        Some((s, start + padded_len))
    }
}

/// Wayland Message Builder for serializing outgoing events
pub struct WlMessageBuilder {
    buf: Vec<u8>,
}

impl WlMessageBuilder {
    pub fn new(object_id: u32, opcode: u16) -> Self {
        let mut builder = Self {
            buf: Vec::with_capacity(64),
        };
        builder.buf.extend_from_slice(&object_id.to_ne_bytes());
        builder.buf.extend_from_slice(&opcode.to_ne_bytes());
        builder.buf.extend_from_slice(&0u16.to_ne_bytes()); // placeholder for length
        builder
    }

    pub fn put_u32(&mut self, val: u32) -> &mut Self {
        self.buf.extend_from_slice(&val.to_ne_bytes());
        self
    }

    pub fn put_i32(&mut self, val: i32) -> &mut Self {
        self.buf.extend_from_slice(&val.to_ne_bytes());
        self
    }

    pub fn put_fixed(&mut self, val: f32) -> &mut Self {
        let fixed = (val * 256.0) as i32;
        self.put_i32(fixed)
    }

    pub fn put_string(&mut self, s: &str) -> &mut Self {
        let bytes = s.as_bytes();
        let len_with_null = (bytes.len() + 1) as u32;
        self.put_u32(len_with_null);
        self.buf.extend_from_slice(bytes);
        self.buf.push(0); // null terminator
        let rem = (bytes.len() + 1) % 4;
        if rem != 0 {
            let padding = 4 - rem;
            for _ in 0..padding {
                self.buf.push(0);
            }
        }
        self
    }

    pub fn put_array(&mut self, data: &[u8]) -> &mut Self {
        self.put_u32(data.len() as u32);
        self.buf.extend_from_slice(data);
        let rem = data.len() % 4;
        if rem != 0 {
            for _ in 0..(4 - rem) {
                self.buf.push(0);
            }
        }
        self
    }

    pub fn build(mut self) -> Vec<u8> {
        let total_len = self.buf.len() as u16;
        self.buf[6..8].copy_from_slice(&total_len.to_ne_bytes());
        self.buf
    }
}

// -----------------------------------------------------------------------------
// Core & Mobile Protocol Extension Enumerations & Interfaces
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaylandInterface {
    WlDisplay,
    WlRegistry,
    WlCompositor,
    WlShm,
    WlSeat,
    WlOutput,
    WlSubcompositor,
    // Mobile Extension Protocols
    XdgWmBase,
    XdgSurface,
    XdgToplevel,
    ZwlrLayerShellV1,
    ZwlrLayerSurfaceV1,
    ZwpLinuxDmabufV1,
    ZwpLinuxBufferParamsV1,
    WpPresentation,
    WpPresentationFeedback,
    WpViewporter,
    WpViewport,
    ExtIdleNotifierV1,
    ExtIdleNotificationV1,
    ZwpTextInputV3,
    ZwpInputMethodV2,
    ZwpTabletManagerV2,
}

impl WaylandInterface {
    pub fn name(&self) -> &'static str {
        match self {
            WaylandInterface::WlDisplay => "wl_display",
            WaylandInterface::WlRegistry => "wl_registry",
            WaylandInterface::WlCompositor => "wl_compositor",
            WaylandInterface::WlShm => "wl_shm",
            WaylandInterface::WlSeat => "wl_seat",
            WaylandInterface::WlOutput => "wl_output",
            WaylandInterface::WlSubcompositor => "wl_subcompositor",
            WaylandInterface::XdgWmBase => "xdg_wm_base",
            WaylandInterface::XdgSurface => "xdg_surface",
            WaylandInterface::XdgToplevel => "xdg_toplevel",
            WaylandInterface::ZwlrLayerShellV1 => "zwlr_layer_shell_v1",
            WaylandInterface::ZwlrLayerSurfaceV1 => "zwlr_layer_surface_v1",
            WaylandInterface::ZwpLinuxDmabufV1 => "zwp_linux_dmabuf_v1",
            WaylandInterface::ZwpLinuxBufferParamsV1 => "zwp_linux_buffer_params_v1",
            WaylandInterface::WpPresentation => "wp_presentation",
            WaylandInterface::WpPresentationFeedback => "wp_presentation_feedback",
            WaylandInterface::WpViewporter => "wp_viewporter",
            WaylandInterface::WpViewport => "wp_viewport",
            WaylandInterface::ExtIdleNotifierV1 => "ext_idle_notifier_v1",
            WaylandInterface::ExtIdleNotificationV1 => "ext_idle_notification_v1",
            WaylandInterface::ZwpTextInputV3 => "zwp_text_input_v3",
            WaylandInterface::ZwpInputMethodV2 => "zwp_input_method_v2",
            WaylandInterface::ZwpTabletManagerV2 => "zwp_tablet_manager_v2",
        }
    }

    pub fn max_version(&self) -> u32 {
        match self {
            WaylandInterface::WlDisplay => 1,
            WaylandInterface::WlRegistry => 1,
            WaylandInterface::WlCompositor => 5,
            WaylandInterface::WlShm => 1,
            WaylandInterface::WlSeat => 8,
            WaylandInterface::WlOutput => 4,
            WaylandInterface::WlSubcompositor => 1,
            WaylandInterface::XdgWmBase => 3,
            WaylandInterface::XdgSurface => 3,
            WaylandInterface::XdgToplevel => 3,
            WaylandInterface::ZwlrLayerShellV1 => 4,
            WaylandInterface::ZwlrLayerSurfaceV1 => 4,
            WaylandInterface::ZwpLinuxDmabufV1 => 4,
            WaylandInterface::ZwpLinuxBufferParamsV1 => 4,
            WaylandInterface::WpPresentation => 1,
            WaylandInterface::WpPresentationFeedback => 1,
            WaylandInterface::WpViewporter => 1,
            WaylandInterface::WpViewport => 1,
            WaylandInterface::ExtIdleNotifierV1 => 1,
            WaylandInterface::ExtIdleNotificationV1 => 1,
            WaylandInterface::ZwpTextInputV3 => 1,
            WaylandInterface::ZwpInputMethodV2 => 1,
            WaylandInterface::ZwpTabletManagerV2 => 1,
        }
    }
}

/// wlr-layer-shell surface layer assignment
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WlrLayer {
    Background = 0,
    Bottom = 1,
    Top = 2,
    Overlay = 3,
}

impl WlrLayer {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(WlrLayer::Background),
            1 => Some(WlrLayer::Bottom),
            2 => Some(WlrLayer::Top),
            3 => Some(WlrLayer::Overlay),
            _ => None,
        }
    }
}

/// xdg_toplevel state flags
pub mod toplevel_state {
    pub const MAXIMIZED: u32 = 1;
    pub const FULLSCREEN: u32 = 2;
    pub const RESIZING: u32 = 3;
    pub const ACTIVATED: u32 = 4;
    pub const TILED_LEFT: u32 = 5;
    pub const TILED_RIGHT: u32 = 6;
    pub const TILED_TOP: u32 = 7;
    pub const TILED_BOTTOM: u32 = 8;
}

/// DRM FourCC pixel format definitions for linux-dmabuf
pub mod drm_formats {
    pub const DRM_FORMAT_XRGB8888: u32 = 0x34325258; // 'XR24'
    pub const DRM_FORMAT_ARGB8888: u32 = 0x34325241; // 'AR24'
    pub const DRM_FORMAT_RGBA8888: u32 = 0x34324152; // 'RA24'
    pub const DRM_FORMAT_NV12: u32 = 0x3231564e;     // 'NV12'
}

/// Protocol Global advertisement registry
#[derive(Debug, Clone)]
pub struct ProtocolGlobal {
    pub name: u32,
    pub interface: WaylandInterface,
    pub version: u32,
}

/// Registry manager for all supported mobile and core Wayland globals
pub struct ProtocolRegistry {
    globals: Vec<ProtocolGlobal>,
    next_name: u32,
}

impl Default for ProtocolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            globals: Vec::with_capacity(32),
            next_name: 1,
        };
        registry.register_core_and_mobile_globals();
        registry
    }

    fn register_core_and_mobile_globals(&mut self) {
        let interfaces = [
            WaylandInterface::WlCompositor,
            WaylandInterface::WlShm,
            WaylandInterface::WlSeat,
            WaylandInterface::WlOutput,
            WaylandInterface::WlSubcompositor,
            WaylandInterface::XdgWmBase,
            WaylandInterface::ZwlrLayerShellV1,
            WaylandInterface::ZwpLinuxDmabufV1,
            WaylandInterface::WpPresentation,
            WaylandInterface::WpViewporter,
            WaylandInterface::ExtIdleNotifierV1,
            WaylandInterface::ZwpTextInputV3,
            WaylandInterface::ZwpInputMethodV2,
            WaylandInterface::ZwpTabletManagerV2,
        ];

        for iface in interfaces {
            self.globals.push(ProtocolGlobal {
                name: self.next_name,
                interface: iface,
                version: iface.max_version(),
            });
            self.next_name += 1;
        }
    }

    pub fn globals(&self) -> &[ProtocolGlobal] {
        &self.globals
    }

    pub fn find_by_interface(&self, iface: WaylandInterface) -> Option<&ProtocolGlobal> {
        self.globals.iter().find(|g| g.interface == iface)
    }

    pub fn supports_mobile_protocols(&self) -> bool {
        let required = [
            WaylandInterface::XdgWmBase,
            WaylandInterface::ZwlrLayerShellV1,
            WaylandInterface::ZwpLinuxDmabufV1,
            WaylandInterface::WpPresentation,
            WaylandInterface::WpViewporter,
            WaylandInterface::ExtIdleNotifierV1,
            WaylandInterface::ZwpTextInputV3,
        ];

        required
            .iter()
            .all(|req| self.find_by_interface(*req).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wl_header_serialization() {
        let header = WlHeader::new(42, 3, 24);
        let bytes = header.to_bytes();
        let parsed = WlHeader::from_bytes(&bytes).expect("Failed to parse header");
        assert_eq!(header, parsed);
    }

    #[test]
    fn test_message_builder_and_parser() {
        let mut builder = WlMessageBuilder::new(100, 2);
        builder.put_u32(12345);
        builder.put_fixed(2.5);
        builder.put_string("org.freedesktop.wayland");
        let wire = builder.build();

        assert_eq!(wire.len() % 4, 0);

        let (msg, len) = WlMessage::parse(&wire).unwrap().unwrap();
        assert_eq!(len, wire.len());
        assert_eq!(msg.header.object_id, 100);
        assert_eq!(msg.header.opcode, 2);

        assert_eq!(msg.read_u32(0), Some(12345));
        assert_eq!(msg.read_fixed(4), Some(2.5));

        let (s, _) = msg.read_string(8).expect("String failed");
        assert_eq!(s, "org.freedesktop.wayland");
    }

    #[test]
    fn test_protocol_registry_mobile_coverage() {
        let reg = ProtocolRegistry::new();
        assert!(reg.supports_mobile_protocols());
        assert!(reg.find_by_interface(WaylandInterface::XdgWmBase).is_some());
        assert!(reg.find_by_interface(WaylandInterface::ZwlrLayerShellV1).is_some());
        assert!(reg.find_by_interface(WaylandInterface::ZwpLinuxDmabufV1).is_some());
        assert!(reg.find_by_interface(WaylandInterface::WpPresentation).is_some());
        assert!(reg.find_by_interface(WaylandInterface::WpViewporter).is_some());
        assert!(reg.find_by_interface(WaylandInterface::ExtIdleNotifierV1).is_some());
        assert!(reg.find_by_interface(WaylandInterface::ZwpTextInputV3).is_some());
    }
}
