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
        // Wayland wire format is little-endian on all supported targets;
        // never use native endian here (P17).
        buf[0..4].copy_from_slice(&self.object_id.to_le_bytes());
        buf[4..6].copy_from_slice(&self.opcode.to_le_bytes());
        buf[6..8].copy_from_slice(&self.length.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let object_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let opcode = u16::from_le_bytes([bytes[4], bytes[5]]);
        let length = u16::from_le_bytes([bytes[6], bytes[7]]);
        if length < 8 {
            return None;
        }
        // Wire length is always a multiple of 4; object id 0 is reserved
        // (wl_display is 1) and never valid on the wire (P18).
        if !length.is_multiple_of(4) {
            return None;
        }
        if object_id == 0 {
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
        if !total_len.is_multiple_of(4) {
            return Err("Wayland message length not a multiple of 4");
        }
        if header.object_id == 0 {
            return Err("Wayland object id 0 is reserved");
        }
        if buf.len() < total_len {
            return Ok(None);
        }
        let payload = &buf[8..total_len];
        Ok(Some((Self { header, payload }, total_len)))
    }

    pub fn read_u32(&self, offset: usize) -> Option<u32> {
        if offset + 4 <= self.payload.len() {
            Some(u32::from_le_bytes([
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

    /// Read null-terminated string padded to 4-byte boundary.
    /// The payload length includes the trailing NUL; a string without a
    /// terminating NUL is malformed and rejected (P19).
    pub fn read_string(&self, offset: usize) -> Option<(&'a str, usize)> {
        let len = self.read_u32(offset)? as usize;
        if len == 0 {
            return None;
        }
        let start = offset.checked_add(4)?;
        let padded_len = len.checked_add(3)? & !3;
        let next_offset = start.checked_add(padded_len)?;
        if next_offset > self.payload.len() {
            return None;
        }
        let end = start.checked_add(len)?;
        let slice = &self.payload[start..end];
        // Require the trailing NUL; also reject interior NULs so the
        // returned &str is exactly the wire string.
        if slice.last() != Some(&0) {
            return None;
        }
        let s = std::str::from_utf8(&slice[..slice.len() - 1]).ok()?;
        if s.contains('\0') {
            return None;
        }
        Some((s, next_offset))
    }
}

/// Wayland Message Builder for serializing outgoing events.
///
/// Allocation note (P26): `build()` allocates the exact wire buffer once.
/// Hot paths emitting many events should reuse a caller-owned buffer with
/// [`WlMessageBuilder::build_into`] instead, which appends the framed
/// message without allocating a fresh `Vec` per event.
pub struct WlMessageBuilder {
    buf: Vec<u8>,
}

impl WlMessageBuilder {
    pub fn new(object_id: u32, opcode: u16) -> Self {
        let mut builder = Self {
            buf: Vec::with_capacity(64),
        };
        builder.buf.extend_from_slice(&object_id.to_le_bytes());
        builder.buf.extend_from_slice(&opcode.to_le_bytes());
        builder.buf.extend_from_slice(&0u16.to_le_bytes()); // placeholder for length
        builder
    }

    pub fn put_u32(&mut self, val: u32) -> &mut Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    pub fn put_i32(&mut self, val: i32) -> &mut Self {
        self.buf.extend_from_slice(&val.to_le_bytes());
        self
    }

    /// Append a Wayland 24.8 fixed-point value.
    ///
    /// Returns `Err` on NaN/inf (which previously saturated silently via
    /// `as` casts, P25). Finite out-of-range values are clamped to the
    /// exactly representable 24.8 span `[-8388608.0, 8388607.996]`.
    pub fn put_fixed(&mut self, val: f32) -> Result<&mut Self, &'static str> {
        if !val.is_finite() {
            return Err("non-finite fixed-point value");
        }
        // i32::MAX / 256 = 8388607.99609375; clamp so the scaled value
        // always fits i32 without `as`-cast saturation.
        let clamped = val.clamp(-8_388_608.0, 8_388_607.996);
        let fixed = (clamped * 256.0).round() as i32;
        self.put_i32(fixed);
        Ok(self)
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

    /// Frame the message and append it to a caller-owned buffer, patching
    /// the 16-bit length field in place. Prefer this on hot paths to avoid
    /// one `Vec` allocation per emitted event (P26).
    pub fn build_into(mut self, out: &mut Vec<u8>) -> Result<(), &'static str> {
        let total_len = u16::try_from(self.buf.len()).map_err(|_| "message exceeds u16 length")?;
        self.buf[6..8].copy_from_slice(&total_len.to_le_bytes());
        out.extend_from_slice(&self.buf);
        Ok(())
    }

    /// Frame the message, returning the exact wire buffer.
    /// Fails if the framed length does not fit the 16-bit wire field
    /// instead of silently truncating (P4).
    pub fn build(mut self) -> Result<Vec<u8>, &'static str> {
        let total_len =
            u16::try_from(self.buf.len()).map_err(|_| "message exceeds u16 length")?;
        self.buf[6..8].copy_from_slice(&total_len.to_le_bytes());
        Ok(self.buf)
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
    pub const DRM_FORMAT_NV12: u32 = 0x3231564e; // 'NV12'
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
        builder.put_fixed(2.5).expect("finite fixed");
        builder.put_string("org.freedesktop.wayland");
        let wire = builder.build().expect("fits u16");

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
    fn test_builder_rejects_overflow_and_nonfinite() {
        // Length overflow: force a buffer larger than u16::MAX.
        let mut builder = WlMessageBuilder::new(1, 0);
        builder.put_array(&vec![0u8; u16::MAX as usize]);
        assert!(builder.build().is_err());

        let mut b2 = WlMessageBuilder::new(1, 0);
        assert!(b2.put_fixed(f32::NAN).is_err());
        assert!(b2.put_fixed(f32::INFINITY).is_err());
        // Clamp extremes instead of saturating.
        b2.put_fixed(1e30).expect("clamped");
        b2.put_fixed(-1e30).expect("clamped");
        let wire = b2.build().expect("fits");
        let (msg, _) = WlMessage::parse(&wire).unwrap().unwrap();
        assert_eq!(msg.read_i32(0), Some(i32::MAX));
        assert_eq!(msg.read_i32(4), Some(i32::MIN));
    }

    #[test]
    fn test_parse_rejects_bad_length_and_zero_id() {
        // Length not a multiple of 4.
        let mut raw = vec![0u8; 12];
        raw[0..4].copy_from_slice(&1u32.to_le_bytes());
        raw[4..6].copy_from_slice(&0u16.to_le_bytes());
        raw[6..8].copy_from_slice(&10u16.to_le_bytes());
        assert!(WlMessage::parse(&raw).is_err());

        // Object id 0.
        let mut raw0 = vec![0u8; 8];
        raw0[6..8].copy_from_slice(&8u16.to_le_bytes());
        assert!(WlMessage::parse(&raw0).is_err());
    }

    #[test]
    fn test_read_string_requires_nul() {
        // Build a payload whose string lacks the trailing NUL.
        let mut wire = vec![0u8; 8];
        wire[0..4].copy_from_slice(&7u32.to_le_bytes());
        wire[6..8].copy_from_slice(&16u16.to_le_bytes());
        wire.extend_from_slice(&4u32.to_le_bytes());
        wire.extend_from_slice(b"abcd"); // len claims 4, no NUL present
        let (msg, _) = WlMessage::parse(&wire).unwrap().unwrap();
        assert_eq!(msg.read_string(0), None);
    }

    #[test]
    fn test_protocol_registry_mobile_coverage() {
        let reg = ProtocolRegistry::new();
        assert!(reg.supports_mobile_protocols());
        assert!(reg.find_by_interface(WaylandInterface::XdgWmBase).is_some());
        assert!(reg
            .find_by_interface(WaylandInterface::ZwlrLayerShellV1)
            .is_some());
        assert!(reg
            .find_by_interface(WaylandInterface::ZwpLinuxDmabufV1)
            .is_some());
        assert!(reg
            .find_by_interface(WaylandInterface::WpPresentation)
            .is_some());
        assert!(reg
            .find_by_interface(WaylandInterface::WpViewporter)
            .is_some());
        assert!(reg
            .find_by_interface(WaylandInterface::ExtIdleNotifierV1)
            .is_some());
        assert!(reg
            .find_by_interface(WaylandInterface::ZwpTextInputV3)
            .is_some());
    }
}
