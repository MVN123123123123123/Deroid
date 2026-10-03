//! D-Bus net.hadess.SensorProxy (iio-sensor-proxy) Service & Display Auto-Rotation.
//! Calculates device physical orientation from 3-axis accelerometer gravity vectors,
//! applies anti-jitter hysteresis, and feeds rotation events into the UTLC Wayland Compositor.
//! Conforms strictly to GEMINI.md systems discipline.
//!
//! # The D-Bus producer: `net.hadess.SensorProxy` polling without dependencies
//!
//! [`SensorProxyService`] had no live callers: classifying a gravity vector is
//! useless without a socket to the daemon that publishes one, so the orientation
//! it produced never reached [`crate::rotation::RotationPolicy`]. This section
//! closes that half. It is deliberately split the way
//! [`crate::notification`] is:
//!
//! * **Parse step, pure.** [`DeviceOrientation::from_dbus_str`],
//!   [`decode_get_reply`], [`parse_unix_path`] and the `encode_*` constructors
//!   do no I/O, never panic, and are tested without a daemon (the tests bring
//!   their own fake one).
//! * **Transport, thin.** [`SensorProxyConnection`] owns one `UnixStream`,
//!   performs the SASL handshake, `Hello`, [`CLAIM_ACCELEROMETER_MEMBER`] and a
//!   `Properties.Get` poll for [`ACCELEROMETER_ORIENTATION_PROPERTY`], and
//!   nothing else. Every read is timeout-bounded and every buffer is a stack
//!   array: the frame-path datum is `Option<DeviceOrientation>` (`Copy`, one
//!   byte), so a poll allocates nothing.
//!
//! # Safety when the daemon is absent
//!
//! There is no `net.hadess.SensorProxy` socket on most devices this image boots
//! on. [`SensorProxyConnection::connect`] then returns `None`, and the shell
//! reports the sensor absent (`RotationPolicy::set_sensor_present(false)`),
//! which holds the panel -- the correct answer, not a silent failure. A poll
//! that fails mid-session returns `None` without touching the policy's history:
//! a transport hiccup must not reset the dwell count, while a genuine flat
//! reading arrives as `Some(DeviceOrientation::Undefined)` and does, matching
//! `SensorProxyService::process_accelerometer`'s own flat reset.
//!
//! # Shell wiring
//!
//! ```text
//! let mut link = SensorProxyConnection::connect(); // None => no daemon
//! policy.set_auto_rotate(state.auto_rotate);      // PreferenceManager.kt:75 defaults false
//! policy.set_sensor_present(link.is_some());
//! if let Some(c) = link.as_mut() {
//!     if c.claim_accelerometer() {
//!         // ... at a few Hz, off the draw path:
//!         if let Some(o) = c.poll_orientation() {
//!             // Capture `current` BEFORE `update`: it commits the decision
//!             // and moves the panel, so deciding after it would compare the
//!             // new position against itself and always answer `None`.
//!             let before = policy.current();
//!             if let Some(wanted) = policy.update(o).orientation() {
//!                 match decide_modeset(before, wanted, policy.natural(), supported) {
//!                     ModesetAction::Rotate(_) => { /* KMS re-modeset (drm_kms.rs) */ }
//!                     ModesetAction::Unsupported(_) | ModesetAction::None => {}
//!                 }
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! The request ladder the policy resolves (`RotationRequest::None` / `Rotate` /
//! `Lock`) is the reference's `REQUEST_NONE` / `REQUEST_ROTATE` / `REQUEST_LOCK`
//! (`RotationHelper.java:64-66`), resolved in the same order as its
//! `notifyChange` (`RotationHelper.java:213-231`), and `update` commits the
//! move so a settled panel is never re-decided (`RotationHelper.java:232-236`).
//! Polling `Get` rather than subscribing to `PropertiesChanged` is deliberate:
//! the property the signal would carry is the property polled, and a signal
//! demuxer is framing this client does not need.
//!
//! No heap on the frame path, no new dependencies (`std` + the existing `libc`
//! for `getuid` only), every `pub fn` below total over its inputs.

use super::sensor_hal::{SensorData, SensorEvent};
use crate::graphics::composer::Transform;

/// Physical Screen Orientation (net.hadess.SensorProxy AccelerometerOrientation)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceOrientation {
    Normal,    // 0 deg (Portrait upright)
    BottomUp,  // 180 deg (Portrait upside down)
    LeftUp,    // 90 deg (Landscape, rotated counter-clockwise)
    RightUp,   // 270 deg (Landscape, rotated clockwise)
    Undefined, // Flat / FaceUp / FaceDown
}

impl DeviceOrientation {
    pub fn as_dbus_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::BottomUp => "bottom-up",
            Self::LeftUp => "left-up",
            Self::RightUp => "right-up",
            Self::Undefined => "undefined",
        }
    }

    /// Parse the `AccelerometerOrientation` property word the daemon publishes.
    ///
    /// The exact inverse of [`Self::as_dbus_str`]: `"normal"`, `"bottom-up"`,
    /// `"left-up"`, `"right-up"`, `"undefined"`, matched byte-for-byte. Anything
    /// else -- including `"Normal"` -- is `None` rather than a guess, because a
    /// misclassified orientation rotates the panel the wrong way while an
    /// unknown one only holds it. Total: never panics, allocates nothing, so it
    /// is safe to call on every poll off bytes from another process.
    pub fn from_dbus_str(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Self::Normal),
            "bottom-up" => Some(Self::BottomUp),
            "left-up" => Some(Self::LeftUp),
            "right-up" => Some(Self::RightUp),
            "undefined" => Some(Self::Undefined),
            _ => None,
        }
    }

    /// Map device orientation to Wayland Compositor plane transformation
    pub fn to_transform(&self) -> Transform {
        match self {
            Self::Normal => Transform::None,
            Self::BottomUp => Transform::Rotate180,
            Self::LeftUp => Transform::Rotate90,
            Self::RightUp => Transform::Rotate270,
            Self::Undefined => Transform::None,
        }
    }
}

/// D-Bus iio-sensor-proxy emulation service
pub struct SensorProxyService {
    pub has_accelerometer: bool,
    pub has_ambient_light: bool,
    pub current_orientation: DeviceOrientation,
    pub light_level_lux: f32,
    candidate_orientation: DeviceOrientation,
    candidate_sample_count: u32,
    pub orientation_change_count: u64,
    pub auto_rotate_enabled: bool,
}

impl Default for SensorProxyService {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorProxyService {
    pub fn new() -> Self {
        Self {
            has_accelerometer: true,
            has_ambient_light: true,
            current_orientation: DeviceOrientation::Normal,
            light_level_lux: 100.0,
            candidate_orientation: DeviceOrientation::Normal,
            candidate_sample_count: 0,
            orientation_change_count: 0,
            auto_rotate_enabled: true,
        }
    }

    /// Process incoming sensor event from Sensors HAL
    pub fn process_event(&mut self, ev: &SensorEvent) -> Option<DeviceOrientation> {
        match &ev.data {
            SensorData::Acceleration { x, y, z } => self.process_accelerometer(*x, *y, *z),
            SensorData::Light { lux } => {
                self.light_level_lux = *lux;
                None
            }
            _ => None,
        }
    }

    /// Calculate orientation from 3-axis accelerometer gravity vector with hysteresis
    pub fn process_accelerometer(&mut self, x: f32, y: f32, z: f32) -> Option<DeviceOrientation> {
        // If phone is lying relatively flat (face up or face down), hold current orientation
        if z.abs() > 8.0 {
            self.candidate_sample_count = 0;
            return None;
        }

        let raw_orientation = if x.abs() > y.abs() {
            if x > 3.5 {
                DeviceOrientation::LeftUp
            } else if x < -3.5 {
                DeviceOrientation::RightUp
            } else {
                self.current_orientation
            }
        } else if y > 3.5 {
            DeviceOrientation::Normal
        } else if y < -3.5 {
            DeviceOrientation::BottomUp
        } else {
            self.current_orientation
        };

        if raw_orientation == self.current_orientation {
            self.candidate_sample_count = 0;
            return None;
        }

        // Hysteresis: require 3 consecutive matching readings before switching orientation
        if raw_orientation == self.candidate_orientation {
            self.candidate_sample_count += 1;
            if self.candidate_sample_count >= 3 {
                self.candidate_sample_count = 0;
                // H12: when auto-rotate is off, do not mutate internal state
                // at all, so enabling it later still produces a transition.
                if !self.auto_rotate_enabled {
                    return None;
                }
                self.current_orientation = raw_orientation;
                self.orientation_change_count += 1;
                return Some(self.current_orientation);
            }
        } else {
            self.candidate_orientation = raw_orientation;
            self.candidate_sample_count = 1;
        }

        None
    }
}

// ---------------------------------------------------------------------------
// The D-Bus producer wire contract: constants, pure codec, thin transport.
// ---------------------------------------------------------------------------

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Well-known bus name of iio-sensor-proxy.
pub const SENSOR_PROXY_BUS: &str = "net.hadess.SensorProxy";
/// Object path exposing the sensor properties.
pub const SENSOR_PROXY_PATH: &str = "/net/hadess/SensorProxy";
/// Interface owning `ClaimAccelerometer` and the orientation property.
pub const SENSOR_PROXY_IFACE: &str = "net.hadess.SensorProxy";
/// Standard interface polled for the property.
pub const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
/// The property polled: one of the words [`DeviceOrientation::from_dbus_str`]
/// accepts.
pub const ACCELEROMETER_ORIENTATION_PROPERTY: &str = "AccelerometerOrientation";
/// Method that starts the daemon's accelerometer sampling.
pub const CLAIM_ACCELEROMETER_MEMBER: &str = "ClaimAccelerometer";
/// System bus socket, conventional location first.
pub const SYSTEM_BUS_SOCKET: &str = "/run/dbus/system_bus_socket";
/// System bus socket where distributions without `/run` keep it.
pub const SYSTEM_BUS_SOCKET_FALLBACK: &str = "/var/run/dbus/system_bus_socket";
/// Environment override for the system bus address, honoured before the two
/// paths above. Only `unix:path=` is understood; anything else falls through.
pub const SYSTEM_BUS_ADDRESS_ENV: &str = "DBUS_SYSTEM_BUS_ADDRESS";
/// Largest single D-Bus message this client will buffer.
///
/// A `Get` reply for a ten-byte orientation word is well under two hundred
/// bytes; two kibibytes is headroom, not a target. Anything larger is
/// drain-discarded so the stream stays aligned, and the poll reports `None`.
pub const MAX_DBUS_MESSAGE: usize = 2048;
/// Handshake I/O budget. Startup path only, never frame path.
pub const CONNECT_TIMEOUT_MS: u64 = 2000;
/// Per-poll I/O budget. A local round trip is sub-millisecond; this only bites
/// when the daemon is wedged, which is why the shell polls at a few Hz off
/// the draw path rather than per frame.
pub const POLL_TIMEOUT_MS: u64 = 100;
/// Messages consumed per poll while waiting for the reply that matches. Only
/// our own replies arrive -- this client never subscribes -- so one is the
/// common case and four is a stuck-signal budget, not a loop.
pub const MAX_POLL_MESSAGES: usize = 4;

const DBUS_TYPE_METHOD_CALL: u8 = 1;
const DBUS_TYPE_METHOD_RETURN: u8 = 2;
const DBUS_TYPE_ERROR: u8 = 3;

/// Parse a `DBUS_SYSTEM_BUS_ADDRESS`-style value into a filesystem socket path.
///
/// Accepts `unix:path=/run/dbus/system_bus_socket` with optional trailing
/// `,key=value` options, and nothing else: `unix:abstract=` has no filesystem
/// path to connect to, and `tcp:` is not a transport this client speaks. Total
/// over its input; the returned borrow lives as long as the address.
pub fn parse_unix_path(address: &str) -> Option<&str> {
    let rest = address.strip_prefix("unix:")?;
    for part in rest.split(',') {
        if let Some(path) = part.strip_prefix("path=") {
            if path.is_empty() || path.contains([';', '\0']) {
                return None;
            }
            return Some(path);
        }
    }
    None
}

/// Checked alignment-up for power-of-two alignments. `None` on overflow.
fn align_up(i: usize, align: usize) -> Option<usize> {
    let add = align.checked_sub(1)?;
    Some(i.checked_add(add)? & !add)
}

/// `i + n` bounded by `len`. Every cursor advance in the decoder goes through
/// here or an equivalent check, which is what makes hostile input a `None`
/// rather than a panic.
fn end(i: usize, n: usize, len: usize) -> Option<usize> {
    let j = i.checked_add(n)?;
    if j > len {
        return None;
    }
    Some(j)
}

/// A cursor that marshals one D-Bus method call into a caller-owned buffer.
///
/// Caller-owned so the frame path encodes into a stack array: no allocator is
/// involved at any point between the poll tick and the orientation.
struct Writer<'a> {
    buf: &'a mut [u8],
    i: usize,
}

impl Writer<'_> {
    fn put_u8(&mut self, v: u8) -> Option<()> {
        *self.buf.get_mut(self.i)? = v;
        self.i = self.i.checked_add(1)?;
        Some(())
    }

    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let stop = end(self.i, bytes.len(), self.buf.len())?;
        self.buf.get_mut(self.i..stop)?.copy_from_slice(bytes);
        self.i = stop;
        Some(())
    }

    fn pad_to(&mut self, align: usize) -> Option<()> {
        while !self.i.is_multiple_of(align) {
            self.put_u8(0)?;
        }
        Some(())
    }

    fn put_u32(&mut self, v: u32) -> Option<()> {
        self.pad_to(4)?;
        self.put(&v.to_le_bytes())
    }

    /// D-Bus STRING / OBJECT_PATH: `u32` length, bytes, NUL.
    fn put_string(&mut self, s: &str) -> Option<()> {
        let n = u32::try_from(s.len()).ok()?;
        self.put_u32(n)?;
        self.put(s.as_bytes())?;
        self.put_u8(0)
    }

    /// D-Bus SIGNATURE: one length byte, bytes, NUL. No alignment: signatures
    /// are byte-aligned by definition.
    fn put_signature(&mut self, s: &str) -> Option<()> {
        let n = u8::try_from(s.len()).ok()?;
        self.put_u8(n)?;
        self.put(s.as_bytes())?;
        self.put_u8(0)
    }
}

/// One header field carrying a string-valued (`s`/`o`) variant.
fn put_field_text(w: &mut Writer<'_>, code: u8, sig: &str, value: &str) -> Option<()> {
    w.pad_to(8)?;
    w.put_u8(code)?;
    w.put_signature(sig)?;
    w.pad_to(4)?;
    w.put_string(value)
}

/// One header field carrying a signature-valued (`g`) variant (used for the
/// body's `SIGNATURE` field).
fn put_field_sig(w: &mut Writer<'_>, code: u8, value: &str) -> Option<()> {
    w.pad_to(8)?;
    w.put_u8(code)?;
    w.put_signature("g")?;
    w.put_signature(value)
}

/// Marshal a `METHOD_CALL` into `out`. `iface` is `None` only for messages
/// that carry none; `body` holds the zero, one or two STRING arguments and
/// determines the `SIGNATURE` field (`"s"`, `"ss"`, or omitted when empty).
///
/// Returns the encoded length. Pure: fixed header layout, length placeholders
/// backpatched, no I/O, no allocation.
fn encode_call(
    out: &mut [u8],
    serial: u32,
    dest: &str,
    path: &str,
    iface: Option<&str>,
    member: &str,
    body: &[&str],
) -> Option<usize> {
    if serial == 0 {
        return None;
    }
    let mut w = Writer { buf: out, i: 0 };
    w.put_u8(b'l')?;
    w.put_u8(DBUS_TYPE_METHOD_CALL)?;
    w.put_u8(0)?;
    w.put_u8(1)?;
    let body_len_at = w.i;
    w.put_u32(0)?;
    w.put_u32(serial)?;
    let fields_len_at = w.i;
    w.put_u32(0)?;
    let fields_start = w.i;
    put_field_text(&mut w, 1, "o", path)?;
    if let Some(f) = iface {
        put_field_text(&mut w, 2, "s", f)?;
    }
    put_field_text(&mut w, 3, "s", member)?;
    put_field_text(&mut w, 6, "s", dest)?;
    if !body.is_empty() {
        let sig = match body.len() {
            1 => "s",
            2 => "ss",
            _ => return None,
        };
        put_field_sig(&mut w, 8, sig)?;
    }
    let fields_len = u32::try_from(w.i.checked_sub(fields_start)?).ok()?;
    let at = fields_len_at.checked_add(4)?;
    w.buf
        .get_mut(fields_len_at..at)?
        .copy_from_slice(&fields_len.to_le_bytes());
    w.pad_to(8)?;
    let body_start = w.i;
    for s in body {
        w.put_string(s)?;
    }
    let body_len = u32::try_from(w.i.checked_sub(body_start)?).ok()?;
    let at = body_len_at.checked_add(4)?;
    w.buf
        .get_mut(body_len_at..at)?
        .copy_from_slice(&body_len.to_le_bytes());
    Some(w.i)
}

/// Encode `org.freedesktop.DBus.Hello`. Pure; see [`encode_call`].
pub fn encode_hello(out: &mut [u8], serial: u32) -> Option<usize> {
    encode_call(
        out,
        serial,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "Hello",
        &[],
    )
}

/// Encode `net.hadess.SensorProxy.ClaimAccelerometer`. Pure; see [`encode_call`].
pub fn encode_claim_accelerometer(out: &mut [u8], serial: u32) -> Option<usize> {
    encode_call(
        out,
        serial,
        SENSOR_PROXY_BUS,
        SENSOR_PROXY_PATH,
        Some(SENSOR_PROXY_IFACE),
        CLAIM_ACCELEROMETER_MEMBER,
        &[],
    )
}

/// Encode `org.freedesktop.DBus.Properties.Get` for the orientation property.
/// Pure; see [`encode_call`].
pub fn encode_get_orientation(out: &mut [u8], serial: u32) -> Option<usize> {
    encode_call(
        out,
        serial,
        SENSOR_PROXY_BUS,
        SENSOR_PROXY_PATH,
        Some(PROPERTIES_IFACE),
        "Get",
        &[SENSOR_PROXY_IFACE, ACCELEROMETER_ORIENTATION_PROPERTY],
    )
}

/// Encode the SASL `AUTH EXTERNAL` line: NUL, `"AUTH EXTERNAL "`, the uid as
/// lowercase hex without leading zeros, CRLF.
///
/// Hex is written nibble by nibble rather than with `format!` so the frame
/// path -- and the handshake that precedes it -- never touches the allocator.
pub fn encode_auth_external(uid: u32, out: &mut [u8]) -> Option<usize> {
    const PREFIX: &[u8] = b"\0AUTH EXTERNAL ";
    const SUFFIX: &[u8] = b"\r\n";
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; 8];
    let mut n = 0usize;
    let mut started = false;
    for shift in (0..8).rev() {
        let nibble = ((uid >> (shift * 4)) & 0xF) as usize;
        if nibble != 0 || started || shift == 0 {
            started = true;
            digits[n] = HEX[nibble];
            n += 1;
        }
    }
    let total = PREFIX.len().checked_add(n)?.checked_add(SUFFIX.len())?;
    let dest = out.get_mut(..total)?;
    dest[..PREFIX.len()].copy_from_slice(PREFIX);
    dest[PREFIX.len()..PREFIX.len() + n].copy_from_slice(&digits[..n]);
    dest[PREFIX.len() + n..].copy_from_slice(SUFFIX);
    Some(total)
}

/// A parsed message header: just enough to route a reply to its call and find
/// the body. Lifetimes tie the signature view to the message bytes.
struct Header<'a> {
    msg_type: u8,
    reply_serial: Option<u32>,
    body_sig: Option<&'a [u8]>,
    body_start: usize,
    body_len: usize,
}

/// Step over one variant value of signature `sig` starting at `i`.
///
/// `depth` bounds `v`-in-`v` nesting: a hostile publisher nesting variants
/// arbitrarily deep must not turn a bounded parse into unbounded recursion.
/// Returns the offset just past the value.
fn skip_value(msg: &[u8], i: usize, sig: &[u8], depth: u8) -> Option<usize> {
    if sig.len() != 1 || depth > 2 {
        return None;
    }
    match sig[0] {
        b'y' | b'b' => end(i, 1, msg.len()),
        b'n' | b'q' => end(align_up(i, 2)?, 2, msg.len()),
        b'i' | b'u' | b'h' => end(align_up(i, 4)?, 4, msg.len()),
        b'x' | b't' | b'd' => end(align_up(i, 8)?, 8, msg.len()),
        b's' | b'o' => {
            let j = align_up(i, 4)?;
            let n =
                u32::from_le_bytes(msg.get(j..end(j, 4, msg.len())?)?.try_into().ok()?) as usize;
            end(end(j, 4, msg.len())?, n.checked_add(1)?, msg.len())
        }
        b'g' => {
            let n = usize::from(*msg.get(i)?);
            end(i, n.checked_add(2)?, msg.len())
        }
        b'v' => {
            let slen = usize::from(*msg.get(i)?);
            let inner = msg.get(i + 1..end(i + 1, slen, msg.len())?)?;
            let stop = end(i + 1, slen.checked_add(1)?, msg.len())?;
            if msg.get(stop - 1) != Some(&0) {
                return None;
            }
            skip_value(msg, stop, inner, depth + 1)
        }
        _ => None,
    }
}

/// Parse a message header: fixed part, then the fields array walked for
/// `REPLY_SERIAL` (field 5, `u`) and `SIGNATURE` (field 8, `g`).
///
/// Total: short reads, wrong endianness, wrong version and unknown field types
/// are all `None`. Only little-endian is supported; the system bus speaks it.
fn parse_header(msg: &[u8]) -> Option<Header<'_>> {
    if msg.len() < 16 || msg[0] != b'l' || msg[3] != 1 {
        return None;
    }
    let msg_type = msg[1];
    let body_len = u32::from_le_bytes(msg.get(4..8)?.try_into().ok()?) as usize;
    let fields_len = u32::from_le_bytes(msg.get(12..16)?.try_into().ok()?) as usize;
    let fields_end = end(16, fields_len, msg.len())?;
    let mut i = 16;
    let mut reply_serial = None;
    let mut body_sig = None;
    while i < fields_end {
        i = align_up(i, 8)?;
        if i >= fields_end {
            break;
        }
        let code = *msg.get(i)?;
        i = end(i, 1, fields_end)?;
        let slen = usize::from(*msg.get(i)?);
        i = end(i, 1, fields_end)?;
        let sig = msg.get(i..end(i, slen, fields_end)?)?;
        i = end(i, slen, fields_end)?;
        if msg.get(i) != Some(&0) {
            return None;
        }
        i = end(i, 1, fields_end)?;
        if code == 5 && sig.len() == 1 && sig[0] == b'u' {
            i = align_up(i, 4)?;
            let v = u32::from_le_bytes(msg.get(i..end(i, 4, fields_end)?)?.try_into().ok()?);
            i = end(i, 4, fields_end)?;
            reply_serial = Some(v);
        } else if code == 8 && sig.len() == 1 && sig[0] == b'g' {
            let n = usize::from(*msg.get(i)?);
            let stop = end(i, n.checked_add(2)?, fields_end)?;
            if msg.get(stop - 1) != Some(&0) {
                return None;
            }
            body_sig = Some(msg.get(i + 1..stop - 1)?);
            i = stop;
        } else {
            i = skip_value(msg, i, sig, 0)?;
            if i > fields_end {
                return None;
            }
        }
    }
    let body_start = align_up(fields_end, 8)?;
    end(body_start, body_len, msg.len())?;
    Some(Header {
        msg_type,
        reply_serial,
        body_sig,
        body_start,
        body_len,
    })
}

/// Decode a `Properties.Get(AccelerometerOrientation)` reply.
///
/// `serial` is the serial the `Get` call went out with: a return for another
/// call is `None`, not a misattributed orientation. The body must be a variant
/// holding a known orientation word; anything else -- error replies, signals,
/// truncated or hostile bytes -- is `None`. Total and allocation-free.
pub fn decode_get_reply(msg: &[u8], serial: u32) -> Option<DeviceOrientation> {
    let h = parse_header(msg)?;
    if h.msg_type != DBUS_TYPE_METHOD_RETURN || h.reply_serial != Some(serial) {
        return None;
    }
    if !matches!(h.body_sig, Some(g) if g.len() == 1 && g[0] == b'v') {
        return None;
    }
    let b = msg.get(h.body_start..end(h.body_start, h.body_len, msg.len())?)?;
    let mut j = 0;
    let slen = usize::from(*b.get(j)?);
    j = end(j, 1, b.len())?;
    let sig = b.get(j..end(j, slen, b.len())?)?;
    j = end(j, slen, b.len())?;
    if b.get(j) != Some(&0) {
        return None;
    }
    j = end(j, 1, b.len())?;
    if sig.len() != 1 || sig[0] != b's' {
        return None;
    }
    j = align_up(j, 4)?;
    let n = u32::from_le_bytes(b.get(j..end(j, 4, b.len())?)?.try_into().ok()?) as usize;
    if n > 64 {
        return None;
    }
    j = end(j, 4, b.len())?;
    let word = b.get(j..end(j, n, b.len())?)?;
    j = end(j, n, b.len())?;
    if b.get(j) != Some(&0) {
        return None;
    }
    DeviceOrientation::from_dbus_str(core::str::from_utf8(word).ok()?)
}

/// Whether a SASL reply line accepts authentication: `OK` followed by a
/// space, CR or LF. Anything else -- `REJECTED`, garbage, EOF -- refuses.
fn is_auth_ok(line: &[u8]) -> bool {
    line.len() >= 2
        && line[0] == b'O'
        && line[1] == b'K'
        && (line.len() == 2 || line[2] == b' ' || line[2] == b'\r' || line[2] == b'\n')
}

/// One D-Bus connection to `net.hadess.SensorProxy`: handshake, claim, poll.
///
/// Owns a file descriptor, so it is deliberately *not* `Copy` and never lives
/// on the frame path: the shell holds it behind an `Option`, connects once at
/// startup, and polls at a few Hz. What crosses into frame code is the
/// `Option<DeviceOrientation>` a poll returns.
pub struct SensorProxyConnection {
    stream: UnixStream,
    next_serial: u32,
    claimed: bool,
}

impl SensorProxyConnection {
    /// Connect to the system bus: `$DBUS_SYSTEM_BUS_ADDRESS` when it parses as
    /// `unix:path=`, otherwise [`SYSTEM_BUS_SOCKET`] then
    /// [`SYSTEM_BUS_SOCKET_FALLBACK`].
    ///
    /// Performs the SASL handshake and `Hello`. `None` when there is no daemon
    /// -- the normal state on most devices -- and never a panic or an
    /// unbounded wait: every I/O is bounded by [`CONNECT_TIMEOUT_MS`].
    pub fn connect() -> Option<Self> {
        if let Ok(addr) = std::env::var(SYSTEM_BUS_ADDRESS_ENV) {
            if let Some(path) = parse_unix_path(&addr) {
                if let Some(conn) = Self::connect_to(path) {
                    return Some(conn);
                }
            }
        }
        Self::connect_to(SYSTEM_BUS_SOCKET).or_else(|| Self::connect_to(SYSTEM_BUS_SOCKET_FALLBACK))
    }

    /// Connect to the bus at an explicit socket path. Same handshake as
    /// [`Self::connect`], but addressable: tests bring their own daemon, and a
    /// device whose bus lives elsewhere passes it in.
    pub fn connect_to(socket_path: &str) -> Option<Self> {
        let stream = UnixStream::connect(socket_path).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(CONNECT_TIMEOUT_MS)))
            .ok()?;
        stream
            .set_write_timeout(Some(Duration::from_millis(CONNECT_TIMEOUT_MS)))
            .ok()?;
        let mut conn = Self {
            stream,
            next_serial: 1,
            claimed: false,
        };
        // SAFETY: `getuid` has no failure mode and touches no Rust state.
        let uid = unsafe { libc::getuid() };
        let mut auth = [0u8; 64];
        let n = encode_auth_external(uid, &mut auth)?;
        conn.stream.write_all(auth.get(..n)?).ok()?;
        let mut line = [0u8; 256];
        let ln = conn.read_line_capped(&mut line)?;
        if !is_auth_ok(line.get(..ln)?) {
            return None;
        }
        conn.stream.write_all(b"BEGIN\r\n").ok()?;
        let hello = conn.call(encode_hello)?;
        conn.wait_for_return(hello)?;
        conn.stream
            .set_read_timeout(Some(Duration::from_millis(POLL_TIMEOUT_MS)))
            .ok()?;
        conn.stream
            .set_write_timeout(Some(Duration::from_millis(POLL_TIMEOUT_MS)))
            .ok()?;
        Some(conn)
    }

    /// Whether the daemon accepted our accelerometer claim.
    ///
    /// False before [`Self::claim_accelerometer`] succeeds, and the value the
    /// shell feeds `RotationPolicy::set_sensor_present`: an unclaimed device
    /// holds the panel rather than rotating on stale data.
    pub const fn has_sensor(&self) -> bool {
        self.claimed
    }

    /// Send `ClaimAccelerometer` and wait for its reply.
    ///
    /// `false` on any failure, without touching `claimed`: a refused claim
    /// leaves the connection usable for a later retry rather than half-open.
    pub fn claim_accelerometer(&mut self) -> bool {
        let serial = match self.call(encode_claim_accelerometer) {
            Some(s) => s,
            None => return false,
        };
        if self.wait_for_return(serial).is_none() {
            return false;
        }
        self.claimed = true;
        true
    }

    /// Poll `AccelerometerOrientation` once.
    ///
    /// `Some(o)` -- including `Some(Undefined)` for a flat device -- is one
    /// sample for `RotationPolicy::update`. `None` is a transport event (no
    /// reply, error reply, oversize message): the shell skips the update and
    /// keeps the policy's history, which is what makes a wedged daemon cost a
    /// held panel rather than a reset dwell count. Bounded by
    /// [`POLL_TIMEOUT_MS`]; allocates nothing.
    pub fn poll_orientation(&mut self) -> Option<DeviceOrientation> {
        let serial = self.call(encode_get_orientation)?;
        let mut buf = [0u8; MAX_DBUS_MESSAGE];
        for _ in 0..MAX_POLL_MESSAGES {
            let n = self.read_message(&mut buf)?;
            let msg = buf.get(..n)?;
            let h = parse_header(msg)?;
            if h.reply_serial != Some(serial) {
                continue;
            }
            if h.msg_type == DBUS_TYPE_ERROR {
                return None;
            }
            if h.msg_type == DBUS_TYPE_METHOD_RETURN {
                return decode_get_reply(msg, serial);
            }
        }
        None
    }

    /// Next outgoing serial. Never zero: serial zero is not a valid D-Bus
    /// serial, so it is skipped on wrap.
    fn take_serial(&mut self) -> u32 {
        let s = self.next_serial;
        self.next_serial = s.wrapping_add(1);
        if self.next_serial == 0 {
            self.next_serial = 1;
        }
        s
    }

    /// Encode with `encode` into a stack buffer and write it whole.
    fn call(&mut self, encode: fn(&mut [u8], u32) -> Option<usize>) -> Option<u32> {
        let serial = self.take_serial();
        let mut buf = [0u8; MAX_DBUS_MESSAGE];
        let n = encode(&mut buf, serial)?;
        self.stream.write_all(buf.get(..n)?).ok()?;
        Some(serial)
    }

    /// Read until the next METHOD_RETURN matching `serial`, skipping at most
    /// [`MAX_POLL_MESSAGES`] unrelated messages (a `NameAcquired` signal after
    /// `Hello`, for example). `None` on an error reply, EOF, timeout or a
    /// malformed message.
    fn wait_for_return(&mut self, serial: u32) -> Option<()> {
        let mut buf = [0u8; MAX_DBUS_MESSAGE];
        for _ in 0..MAX_POLL_MESSAGES {
            let n = self.read_message(&mut buf)?;
            let h = parse_header(buf.get(..n)?)?;
            if h.reply_serial != Some(serial) {
                continue;
            }
            if h.msg_type == DBUS_TYPE_METHOD_RETURN {
                return Some(());
            }
            return None;
        }
        None
    }

    /// Read one line, capped at the buffer. `None` on EOF, timeout or a line
    /// longer than the buffer -- all handshake failures, never panics.
    fn read_line_capped(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut n = 0;
        let mut byte = [0u8; 1];
        while n < buf.len() {
            self.stream.read_exact(&mut byte).ok()?;
            buf[n] = byte[0];
            n += 1;
            if byte[0] == b'\n' {
                return Some(n);
            }
        }
        None
    }

    /// Read exactly one message into `buf`: the 16-byte fixed head, then
    /// `fields` plus `body` by their declared lengths.
    ///
    /// An oversize message is drain-discarded rather than truncated, so the
    /// stream stays aligned and the next poll starts clean. `None` on EOF,
    /// timeout, a non-D-Bus stream, or a length that overflows.
    fn read_message(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut head = [0u8; 16];
        self.stream.read_exact(&mut head).ok()?;
        if head[0] != b'l' || head[3] != 1 {
            return None;
        }
        let body_len = u32::from_le_bytes(head[4..8].try_into().ok()?) as usize;
        let fields_len = u32::from_le_bytes(head[12..16].try_into().ok()?) as usize;
        let fields_end = align_up(16usize.checked_add(fields_len)?, 8)?;
        let total = fields_end.checked_add(body_len)?;
        if total > buf.len() {
            self.drain(total.checked_sub(16)?)?;
            return None;
        }
        buf.get_mut(..16)?.copy_from_slice(&head);
        self.stream.read_exact(buf.get_mut(16..total)?).ok()?;
        Some(total)
    }

    /// Discard `n` bytes in bounded chunks. The 64 KiB abort cap is absurdly
    /// above any message this client solicits; past it the stream is left
    /// alone and the poll reports `None`.
    fn drain(&mut self, mut n: usize) -> Option<()> {
        let mut scratch = [0u8; 256];
        let mut guard = 0usize;
        while n > 0 {
            if guard >= 256 {
                return None;
            }
            guard += 1;
            let take = n.min(scratch.len());
            self.stream.read_exact(scratch.get_mut(..take)?).ok()?;
            n -= take;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    // ------------------------------------------------- fake wire, test only
    //
    // These builders speak real D-Bus framing (little-endian header, aligned
    // fields array, variant body), assembled by hand rather than with the
    // encoders above: a decoder tested only against its own encoder's output
    // proves the two agree, not that either matches the bus.

    /// A `METHOD_RETURN` for `reply_to`, with an optional signature and body.
    fn fake_return(reply_to: u32, body_sig: Option<&str>, body: &[u8]) -> Vec<u8> {
        let mut fields = Vec::new();
        fields.push(5u8);
        fields.push(1);
        fields.extend_from_slice(b"u");
        fields.push(0);
        while !fields.len().is_multiple_of(4) {
            fields.push(0);
        }
        fields.extend_from_slice(&reply_to.to_le_bytes());
        if let Some(sig) = body_sig {
            while !fields.len().is_multiple_of(8) {
                fields.push(0);
            }
            fields.push(8);
            fields.push(1);
            fields.extend_from_slice(b"g");
            fields.push(0);
            fields.push(sig.len() as u8);
            fields.extend_from_slice(sig.as_bytes());
            fields.push(0);
        }
        let mut msg = vec![b'l', DBUS_TYPE_METHOD_RETURN, 0, 1];
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&7u32.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        while !msg.len().is_multiple_of(8) {
            msg.push(0);
        }
        msg.extend_from_slice(body);
        msg
    }

    /// A variant holding one string, as a `Get` reply body carries it.
    fn variant_string(value: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(1);
        v.push(b's');
        v.push(0);
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
        v.extend_from_slice(&(value.len() as u32).to_le_bytes());
        v.extend_from_slice(value.as_bytes());
        v.push(0);
        v
    }

    /// A `Get` reply for `reply_to` carrying `value` as the orientation word.
    fn fake_get_reply(reply_to: u32, value: &str) -> Vec<u8> {
        let body = variant_string(value);
        fake_return(reply_to, Some("v"), &body)
    }

    /// Structural check for an encoded `METHOD_CALL`: framing, serial echo,
    /// member and destination present as NUL-terminated strings, and header
    /// lengths that land exactly on the buffer end.
    fn check_call(buf: &[u8], serial: u32, member: &str, dest: &str) {
        assert!(buf.len() >= 16, "a call is at least a header");
        assert_eq!(&buf[0..4], b"l\x01\x00\x01", "LE method call, v1");
        assert_eq!(
            u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            serial,
            "serial echoes the call"
        );
        let body_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let fields_len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let fields_end = align_up(16 + fields_len, 8).expect("test alignment");
        assert_eq!(
            buf.len(),
            fields_end + body_len,
            "declared lengths land on the buffer end"
        );
        let word = |w: &str| [w.as_bytes(), b"\0"].concat();
        let m = word(member);
        assert!(
            buf.windows(m.len()).any(|w| w == m.as_slice()),
            "member {member:?} present NUL-terminated"
        );
        let d = word(dest);
        assert!(
            buf.windows(d.len()).any(|w| w == d.as_slice()),
            "destination {dest:?} present NUL-terminated"
        );
    }

    // ------------------------------------------------------------- the words

    /// Every orientation round-trips through its D-Bus name.
    #[test]
    fn orientation_strings_round_trip_through_the_dbus_names() {
        for o in [
            DeviceOrientation::Normal,
            DeviceOrientation::BottomUp,
            DeviceOrientation::LeftUp,
            DeviceOrientation::RightUp,
            DeviceOrientation::Undefined,
        ] {
            assert_eq!(DeviceOrientation::from_dbus_str(o.as_dbus_str()), Some(o));
        }
    }

    /// Unknown words hold the panel: they parse to `None`, never to a guess.
    /// Case matters -- the daemon emits lowercase, and `"Normal"` is not it.
    #[test]
    fn unknown_orientation_words_are_rejected_not_guessed() {
        for s in [
            "",
            "Normal",
            "LEFT-UP",
            "landscape",
            "normal ",
            " normal",
            "face-up",
        ] {
            assert_eq!(
                DeviceOrientation::from_dbus_str(s),
                None,
                "{s:?} must not parse"
            );
        }
    }

    // ------------------------------------------------------------ the codec

    /// The SASL line is byte-exact: leading NUL, lowercase hex without leading
    /// zeros, CRLF.
    #[test]
    fn auth_external_encodes_the_uid_as_lowercase_hex() {
        let mut buf = [0u8; 64];
        let n = encode_auth_external(0, &mut buf).expect("uid 0 fits");
        assert_eq!(&buf[..n], b"\0AUTH EXTERNAL 0\r\n");
        let n = encode_auth_external(1000, &mut buf).expect("uid 1000 fits");
        assert_eq!(&buf[..n], b"\0AUTH EXTERNAL 3e8\r\n");
        let n = encode_auth_external(u32::MAX, &mut buf).expect("uid max fits");
        assert_eq!(&buf[..n], b"\0AUTH EXTERNAL ffffffff\r\n");
        assert!(
            encode_auth_external(0, &mut []).is_none(),
            "no silent truncation"
        );
    }

    #[test]
    fn hello_encodes_a_well_formed_call() {
        let mut buf = [0u8; MAX_DBUS_MESSAGE];
        let n = encode_hello(&mut buf, 1).expect("hello fits");
        check_call(&buf[..n], 1, "Hello", "org.freedesktop.DBus");
    }

    #[test]
    fn claim_encodes_a_well_formed_call() {
        let mut buf = [0u8; MAX_DBUS_MESSAGE];
        let n = encode_claim_accelerometer(&mut buf, 41).expect("claim fits");
        check_call(&buf[..n], 41, CLAIM_ACCELEROMETER_MEMBER, SENSOR_PROXY_BUS);
    }

    #[test]
    fn get_encodes_a_well_formed_call_for_the_orientation_property() {
        let mut buf = [0u8; MAX_DBUS_MESSAGE];
        let n = encode_get_orientation(&mut buf, 7).expect("get fits");
        check_call(&buf[..n], 7, "Get", SENSOR_PROXY_BUS);
        let prop = [ACCELEROMETER_ORIENTATION_PROPERTY.as_bytes(), b"\0"].concat();
        assert!(
            buf[..n].windows(prop.len()).any(|w| w == prop.as_slice()),
            "property name rides in the body"
        );
        assert!(
            encode_hello(&mut [0u8; 8], 1).is_none(),
            "a short buffer fails loudly"
        );
        assert!(
            encode_hello(&mut buf, 0).is_none(),
            "serial zero is refused"
        );
    }

    /// A matching return with a known word decodes, including flat.
    #[test]
    fn decode_get_reply_accepts_a_matching_return() {
        let msg = fake_get_reply(41, "left-up");
        assert_eq!(decode_get_reply(&msg, 41), Some(DeviceOrientation::LeftUp));
        let msg = fake_get_reply(9, "undefined");
        assert_eq!(
            decode_get_reply(&msg, 9),
            Some(DeviceOrientation::Undefined),
            "flat is a sample, not a transport failure"
        );
    }

    /// Anything else is `None` and never a panic: wrong serial, error type,
    /// truncation at every prefix, garbage, and an unknown word.
    #[test]
    fn decode_get_reply_rejects_mismatch_truncation_and_garbage() {
        let msg = fake_get_reply(41, "left-up");
        assert_eq!(decode_get_reply(&msg, 42), None, "another call's reply");
        assert_eq!(decode_get_reply(&[], 41), None);
        for cut in 0..msg.len() {
            assert_eq!(
                decode_get_reply(&msg[..cut], 41),
                None,
                "prefix of {cut} bytes must not decode"
            );
        }
        let mut err = fake_return(41, None, &[]);
        err[1] = DBUS_TYPE_ERROR;
        assert_eq!(decode_get_reply(&err, 41), None, "errors are not samples");
        assert_eq!(
            decode_get_reply(&fake_get_reply(41, "sideways"), 41),
            None,
            "unknown words hold the panel"
        );
        assert_eq!(
            decode_get_reply(&fake_return(41, Some("v"), b"nope"), 41),
            None,
            "a malformed variant body"
        );
        let garbage = [0xFFu8; 64];
        assert_eq!(decode_get_reply(&garbage, 41), None);
    }

    /// Only `unix:path=` yields a path. Abstract sockets have no filesystem
    /// node, `tcp:` is not spoken, and empty or NUL-carrying paths refuse.
    #[test]
    fn unix_path_parsing_accepts_only_filesystem_paths() {
        assert_eq!(
            parse_unix_path("unix:path=/run/dbus/system_bus_socket"),
            Some("/run/dbus/system_bus_socket")
        );
        assert_eq!(
            parse_unix_path("unix:path=/run/bus,guid=abc123"),
            Some("/run/bus"),
            "options after the comma are not the path"
        );
        for bad in [
            "",
            "unix:abstract=/tmp/dbus-xyz",
            "tcp:host=localhost,port=0",
            "unix:path=",
            "unix:path=/a;b",
            "unix:tmpdir=/tmp",
        ] {
            assert_eq!(parse_unix_path(bad), None, "{bad:?} must not parse");
        }
    }

    // -------------------------------------------------------- the transport

    /// No socket, no daemon, no panic: the absent-daemon state every sandbox
    /// and most phones boot into.
    #[test]
    fn connect_to_a_missing_socket_is_none_not_a_panic() {
        let missing =
            std::env::temp_dir().join(format!("utim-sensorproxy-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let sock = missing.join("bus.sock");
        assert!(SensorProxyConnection::connect_to(sock.to_str().unwrap()).is_none());
    }

    /// A fake bus speaking SASL + Hello + Claim + Get, so the whole session is
    /// exercised with no daemon on the machine.
    struct FakeBus {
        dir: std::path::PathBuf,
        sock: String,
    }

    impl FakeBus {
        fn path(&self) -> &str {
            &self.sock
        }
    }

    impl Drop for FakeBus {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn read_exact_ok(s: &mut UnixStream, buf: &mut [u8]) -> bool {
        s.read_exact(buf).is_ok()
    }

    /// Serve exactly the session the client runs: SASL, BEGIN, then up to
    /// three calls. Reads are timeout-bounded and every failure ends the
    /// thread cleanly rather than hanging the test's `join`.
    fn start_fake_bus(tag: &str, reply_value: &str) -> (FakeBus, std::thread::JoinHandle<()>) {
        let dir =
            std::env::temp_dir().join(format!("utim-sensorproxy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fake bus dir");
        let sock_path = dir.join("bus.sock");
        let sock = sock_path.to_str().expect("temp path is UTF-8").to_string();
        let listener = UnixListener::bind(&sock_path).expect("fake bus bind");
        let value = reply_value.to_string();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            s.set_write_timeout(Some(Duration::from_secs(5))).ok();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                if !read_exact_ok(&mut s, &mut byte) || line.len() > 200 {
                    return;
                }
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            if !line.starts_with(b"\0AUTH") {
                return;
            }
            if s.write_all(b"OK deadbeef\r\n").is_err() {
                return;
            }
            line.clear();
            loop {
                if !read_exact_ok(&mut s, &mut byte) || line.len() > 200 {
                    return;
                }
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            if line != b"BEGIN\r\n" {
                return;
            }
            for _ in 0..3 {
                let mut head = [0u8; 16];
                if !read_exact_ok(&mut s, &mut head) {
                    break;
                }
                let body_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
                let fields_len = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
                let aligned = align_up(16 + fields_len, 8).expect("test alignment");
                let mut rest = vec![0u8; aligned - 16 + body_len];
                if !read_exact_ok(&mut s, &mut rest) {
                    break;
                }
                let serial = u32::from_le_bytes(head[8..12].try_into().unwrap());
                let mut raw = head.to_vec();
                raw.extend_from_slice(&rest);
                let is_call = raw.windows(5).any(|w| w == b"Hello".as_slice())
                    || raw
                        .windows(18)
                        .any(|w| w == b"ClaimAccelerometer".as_slice());
                let reply = if is_call {
                    fake_return(serial, None, &[])
                } else if raw.windows(3).any(|w| w == b"Get".as_slice()) {
                    fake_get_reply(serial, &value)
                } else {
                    break;
                };
                if s.write_all(&reply).is_err() {
                    break;
                }
            }
        });
        (FakeBus { dir, sock }, handle)
    }

    /// The full session against the fake bus: handshake, unclaimed, claim,
    /// poll. `connect()` honours the address override, so it is covered here
    /// rather than against whatever the host happens to run.
    #[test]
    fn connect_claim_and_poll_against_a_fake_daemon() {
        let (bus, handle) = start_fake_bus("session", "left-up");
        let mut conn = SensorProxyConnection::connect_to(bus.path()).expect("fake handshake");
        assert!(!conn.has_sensor(), "nothing claimed yet");
        assert!(conn.claim_accelerometer());
        assert!(conn.has_sensor());
        assert_eq!(
            conn.poll_orientation(),
            Some(DeviceOrientation::LeftUp),
            "one poll is one sample"
        );

        let (bus2, handle2) = start_fake_bus("env", "right-up");
        std::env::set_var(SYSTEM_BUS_ADDRESS_ENV, format!("unix:path={}", bus2.path()));
        let mut env_conn = SensorProxyConnection::connect().expect("env address wins");
        std::env::remove_var(SYSTEM_BUS_ADDRESS_ENV);
        assert!(env_conn.claim_accelerometer());
        assert_eq!(
            env_conn.poll_orientation(),
            Some(DeviceOrientation::RightUp)
        );
        handle.join().expect("fake bus thread");
        handle2.join().expect("second fake bus thread");
    }

    /// A daemon that goes away is a clean `None`, on claim and on poll, and
    /// the connection reports unclaimed rather than panicking.
    #[test]
    fn a_daemon_that_goes_away_is_a_clean_none() {
        let (bus, handle) = start_fake_bus("drop", "left-up");
        let mut conn = SensorProxyConnection::connect_to(bus.path()).expect("fake handshake");
        conn.stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shutdown a live socket");
        assert!(!conn.claim_accelerometer());
        assert!(!conn.has_sensor());
        assert_eq!(conn.poll_orientation(), None);
        handle.join().expect("fake bus thread");
    }

    /// `connect()` with no daemon anywhere never panics and never hangs past
    /// its timeouts. Asserts nothing about the value: on a host *with* a bus
    /// this may legitimately succeed.
    #[test]
    fn connect_with_no_daemon_never_panics_or_hangs() {
        let _ = SensorProxyConnection::connect();
    }
}
