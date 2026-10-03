//! System theme source for `follow_system_theme`.
//!
//! The reference's `ThemeChoice.SYSTEM` (`ThemePreference.kt:12-30`) resolves
//! against `Configuration.uiMode` via `isSystemInDarkTheme()` (`Theme.kt:118-130`)
//! with a wallpaper fallback (`Theme.kt:132-147`). On freedesktop the equivalent
//! is `org.freedesktop.portal.Settings:color-scheme` (1=prefer-dark, 2=prefer-light,
//! 0=no-preference) and the legacy `org.freedesktop.appearance:color-scheme`.
//!
//! Split the way `sensors/sensor_proxy.rs` is:
//!
//! * **Parse step, pure.** [`parse_portal_color_scheme`],
//!   [`parse_appearance_string`], [`encode_portal_read`] and
//!   [`decode_portal_read_reply`] do no I/O, never panic, and are tested
//!   without a daemon (the tests bring their own fake portal).
//! * **Transport, thin.** [`PortalThemeConnection`] owns one `UnixStream` to
//!   the *session* bus, performs the SASL handshake, `Hello` and a
//!   `Settings.Read` for [`PORTAL_COLOR_KEY`], and nothing else. Every read is
//!   timeout-bounded and every buffer is a stack array: the frame-path datum
//!   is [`SystemTheme`] (`Copy`), so a refresh allocates nothing on the frame
//!   path (`get()` is a `Copy` read; `refresh()`/`probe()` do the I/O off the
//!   draw path).
//!
//! Absent portal yields [`SystemTheme::Unknown`], never a silent
//! [`SystemTheme::Light`]: an unanswered question must not pose as "light".

use crate::sensors::sensor_proxy::{encode_auth_external, encode_hello, parse_unix_path};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// What the system says, with "no answer" distinct from "light".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemTheme {
    Dark,
    Light,
    Unknown,
}

/// Portal `color-scheme` value: 1=prefer-dark, 2=prefer-light, 0=no-preference.
pub fn parse_portal_color_scheme(v: u32) -> SystemTheme {
    match v {
        1 => SystemTheme::Dark,
        2 => SystemTheme::Light,
        _ => SystemTheme::Unknown,
    }
}

/// Legacy string form (`prefer-dark` / `prefer-light`), case-insensitive.
pub fn parse_appearance_string(s: &str) -> SystemTheme {
    if s.eq_ignore_ascii_case("prefer-dark") || s.eq_ignore_ascii_case("dark") {
        SystemTheme::Dark
    } else if s.eq_ignore_ascii_case("prefer-light") || s.eq_ignore_ascii_case("light") {
        SystemTheme::Light
    } else {
        SystemTheme::Unknown
    }
}

/// Env override for testing and kiosks: `UTLC_THEME=dark|light|unknown`.
fn read_env_override() -> Option<SystemTheme> {
    let v = std::env::var("UTLC_THEME").ok()?;
    let t = parse_appearance_string(&v);
    if t == SystemTheme::Unknown && !v.eq_ignore_ascii_case("unknown") {
        return None;
    }
    Some(t)
}

/// Plain-file fallback: first line of `$XDG_CONFIG_HOME/utlc/theme` or
/// `~/.config/utlc/theme`. Always works, checked after env, before D-Bus.
fn read_file_fallback() -> Option<SystemTheme> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = std::path::PathBuf::from(h);
                p.push(".config");
                p
            })
        })?;
    let mut p = base;
    p.push("utlc");
    p.push("theme");
    let s = std::fs::read_to_string(p).ok()?;
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    Some(parse_appearance_string(line))
}

// ---------------------------------------------------------------------------
// The portal wire contract: constants, pure codec, thin transport.
// Hand-rolled D-Bus framing in the style of `sensors/sensor_proxy.rs`
// (little-endian header, aligned fields array, variant body), no new
// dependencies (`std` + the existing `libc` for `getuid` only).
// ---------------------------------------------------------------------------

/// Well-known bus name owning the Settings portal.
pub const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
/// Object path exposing the Settings portal.
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// Interface owning `Read`.
pub const PORTAL_IFACE: &str = "org.freedesktop.portal.Settings";
/// Namespace for the color-scheme key.
pub const PORTAL_NAMESPACE: &str = "org.freedesktop.appearance";
/// Key whose variant holds the `color-scheme` u32.
pub const PORTAL_COLOR_KEY: &str = "color-scheme";
/// Environment override for the session bus address, honoured before the
/// runtime fallbacks. Only `unix:path=` is understood; anything else falls
/// through. Reuses the `sensor_proxy` address grammar.
pub const SESSION_BUS_ADDRESS_ENV: &str = "DBUS_SESSION_BUS_ADDRESS";
/// Largest single D-Bus message this client will buffer.
///
/// A `Read` reply holding a u32 variant is well under two hundred bytes; two
/// kibibytes is headroom, not a target. Anything larger is drain-discarded so
/// the stream stays aligned, and the read reports `None`.
pub const MAX_PORTAL_MESSAGE: usize = 2048;
/// Handshake I/O budget. Startup/refresh path only, never frame path.
pub const PORTAL_CONNECT_TIMEOUT_MS: u64 = 2000;
/// Per-read I/O budget. A local round trip is sub-millisecond; this only bites
/// when the portal is wedged, which is why the shell refreshes off the draw
/// path rather than per frame.
pub const PORTAL_READ_TIMEOUT_MS: u64 = 500;
/// Messages consumed per read while waiting for the reply that matches. Only
/// our own replies arrive -- this client never subscribes -- so one is the
/// common case and four is a stuck-signal budget, not a loop.
pub const MAX_PORTAL_MESSAGES: usize = 4;

const DBUS_TYPE_METHOD_CALL: u8 = 1;
const DBUS_TYPE_METHOD_RETURN: u8 = 2;
const DBUS_TYPE_ERROR: u8 = 3;

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
/// Caller-owned so the refresh path encodes into a stack array: no allocator
/// is involved at any point between the refresh tick and the theme.
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

/// Encode `org.freedesktop.portal.Settings.Read(org.freedesktop.appearance,
/// color-scheme)`. Pure; see [`encode_call`].
pub fn encode_portal_read(out: &mut [u8], serial: u32) -> Option<usize> {
    encode_call(
        out,
        serial,
        PORTAL_BUS,
        PORTAL_PATH,
        Some(PORTAL_IFACE),
        "Read",
        &[PORTAL_NAMESPACE, PORTAL_COLOR_KEY],
    )
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
/// are all `None`. Only little-endian is supported; the session bus speaks it.
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

/// Decode a `Settings.Read(org.freedesktop.appearance, color-scheme)` reply.
///
/// `serial` is the serial the `Read` call went out with: a return for another
/// call is `None`, not a misattributed theme. The body must be a variant
/// holding a u32; anything else -- error replies, signals, truncated or
/// hostile bytes, a variant holding the wrong type -- is `None`. A
/// well-formed u32 maps through [`parse_portal_color_scheme`], so unknown
/// values yield `Some(Unknown)`, never `None` and never `Light`. Total and
/// allocation-free.
pub fn decode_portal_read_reply(msg: &[u8], serial: u32) -> Option<SystemTheme> {
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
    if sig.len() != 1 || sig[0] != b'u' {
        return None;
    }
    j = align_up(j, 4)?;
    let v = u32::from_le_bytes(b.get(j..end(j, 4, b.len())?)?.try_into().ok()?);
    Some(parse_portal_color_scheme(v))
}

/// Whether a SASL reply line accepts authentication: `OK` followed by a
/// space, CR or LF. Anything else -- `REJECTED`, garbage, EOF -- refuses.
fn is_auth_ok(line: &[u8]) -> bool {
    line.len() >= 2
        && line[0] == b'O'
        && line[1] == b'K'
        && (line.len() == 2 || line[2] == b' ' || line[2] == b'\r' || line[2] == b'\n')
}

/// One session-bus connection to the Settings portal: handshake, then reads.
///
/// Owns a file descriptor, so it is deliberately *not* `Copy` and never lives
/// on the frame path: the shell holds it only across a `refresh()`, connects
/// once per refresh at most, and polls at settings-open / periodic cadence.
/// What crosses into frame code is the [`SystemTheme`] a read returns.
pub struct PortalThemeConnection {
    stream: UnixStream,
    next_serial: u32,
}

impl PortalThemeConnection {
    /// Connect to the session bus: `$DBUS_SESSION_BUS_ADDRESS` when it parses
    /// as `unix:path=`, otherwise `$XDG_RUNTIME_DIR/bus`, otherwise
    /// `/run/user/<uid>/bus`.
    ///
    /// Performs the SASL handshake and `Hello`. `None` when there is no portal
    /// -- the normal state on minimal images -- and never a panic or an
    /// unbounded wait: every I/O is bounded by [`PORTAL_CONNECT_TIMEOUT_MS`].
    pub fn connect() -> Option<Self> {
        if let Ok(addr) = std::env::var(SESSION_BUS_ADDRESS_ENV) {
            if let Some(path) = parse_unix_path(&addr) {
                if let Some(conn) = Self::connect_to(path) {
                    return Some(conn);
                }
            }
        }
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            if !dir.is_empty() {
                let p = format!("{dir}/bus");
                if let Some(conn) = Self::connect_to(&p) {
                    return Some(conn);
                }
            }
        }
        // SAFETY: `getuid` has no failure mode and touches no Rust state.
        let uid = unsafe { libc::getuid() };
        let p = format!("/run/user/{uid}/bus");
        Self::connect_to(&p)
    }

    /// Connect to the bus at an explicit socket path. Same handshake as
    /// [`Self::connect`], but addressable: tests bring their own portal, and a
    /// device whose bus lives elsewhere passes it in.
    pub fn connect_to(socket_path: &str) -> Option<Self> {
        let stream = UnixStream::connect(socket_path).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(PORTAL_CONNECT_TIMEOUT_MS)))
            .ok()?;
        stream
            .set_write_timeout(Some(Duration::from_millis(PORTAL_CONNECT_TIMEOUT_MS)))
            .ok()?;
        let mut conn = Self {
            stream,
            next_serial: 1,
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
            .set_read_timeout(Some(Duration::from_millis(PORTAL_READ_TIMEOUT_MS)))
            .ok()?;
        conn.stream
            .set_write_timeout(Some(Duration::from_millis(PORTAL_READ_TIMEOUT_MS)))
            .ok()?;
        Some(conn)
    }

    /// Read `color-scheme` once.
    ///
    /// `Some(t)` -- including `Some(Unknown)` for no-preference -- is the
    /// portal's answer. `None` is a transport event (no reply, error reply,
    /// oversize message): the caller keeps its cached value, which is what
    /// makes a wedged portal cost a stale theme rather than a flipped one.
    /// Bounded by [`PORTAL_READ_TIMEOUT_MS`]; allocates nothing.
    pub fn read_color_scheme(&mut self) -> Option<SystemTheme> {
        let serial = self.call(encode_portal_read)?;
        let mut buf = [0u8; MAX_PORTAL_MESSAGE];
        for _ in 0..MAX_PORTAL_MESSAGES {
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
                return decode_portal_read_reply(msg, serial);
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
        let mut buf = [0u8; MAX_PORTAL_MESSAGE];
        let n = encode(&mut buf, serial)?;
        self.stream.write_all(buf.get(..n)?).ok()?;
        Some(serial)
    }

    /// Read until the next METHOD_RETURN matching `serial`, skipping at most
    /// [`MAX_PORTAL_MESSAGES`] unrelated messages (a `NameAcquired` signal
    /// after `Hello`, for example). `None` on an error reply, EOF, timeout or
    /// a malformed message.
    fn wait_for_return(&mut self, serial: u32) -> Option<()> {
        let mut buf = [0u8; MAX_PORTAL_MESSAGE];
        for _ in 0..MAX_PORTAL_MESSAGES {
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
    /// stream stays aligned and the next read starts clean. `None` on EOF,
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
    /// alone and the read reports `None`.
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

/// Best-effort portal read: `None` when no portal answers, `Some(theme)` when
/// one does (including `Some(Unknown)` for no-preference). Absent daemon is
/// the normal state on minimal images, so it is `None`, not a panic. Off the
/// frame path: called only from `probe()`/`refresh()`.
fn read_portal_fallback() -> Option<SystemTheme> {
    let mut conn = PortalThemeConnection::connect()?;
    conn.read_color_scheme()
}

/// Cached system value. `refresh()` does I/O off the frame path; `get()` is a
/// `Copy` read suitable for the frame path. Staleness is bounded by how often
/// the shell calls `refresh()` (on settings open + periodic poll, not per frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemThemeCache {
    value: SystemTheme,
}

impl SystemThemeCache {
    pub fn new() -> Self {
        Self {
            value: Self::probe(),
        }
    }

    pub fn get(self) -> SystemTheme {
        self.value
    }

    pub fn refresh(&mut self) {
        self.value = Self::probe();
    }

    fn probe() -> SystemTheme {
        if let Some(t) = read_env_override() {
            return t;
        }
        if let Some(t) = read_file_fallback() {
            if t != SystemTheme::Unknown {
                return t;
            }
        }
        // Genuine portal read, best-effort: absent daemon yields Unknown rather
        // than a silent "light". Bounded timeouts, off the frame path.
        if let Some(t) = read_portal_fallback() {
            return t;
        }
        SystemTheme::Unknown
    }
}

impl Default for SystemThemeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience for `effective_theme`: `None` when unknown.
pub fn system_theme_dark() -> Option<bool> {
    match SystemThemeCache::probe() {
        SystemTheme::Dark => Some(true),
        SystemTheme::Light => Some(false),
        SystemTheme::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Serializes tests that touch process-wide bus/theme env vars
    /// (`UTLC_THEME`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`) or call
    /// `probe()` (which reads them plus the bus). Without it a fake-portal
    /// address set by one test leaks into a concurrent `probe()` in another.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Restores one env var on drop, even when the test panics.
    struct RestoreEnv {
        key: &'static str,
        old: Option<String>,
    }

    impl RestoreEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, old }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            if let Some(ref v) = self.old {
                std::env::set_var(self.key, v);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

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

    /// A variant holding one u32, as a `Read` reply body carries it.
    fn variant_u32(value: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(1);
        v.push(b'u');
        v.push(0);
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
        v.extend_from_slice(&value.to_le_bytes());
        v
    }

    /// A variant holding one string (wrong type for this portal: used to prove
    /// the decoder refuses it rather than guessing).
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

    /// A `Read` reply for `reply_to` carrying `value` as the color-scheme u32.
    fn fake_read_reply(reply_to: u32, value: u32) -> Vec<u8> {
        let body = variant_u32(value);
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

    #[test]
    fn portal_values_map() {
        assert_eq!(parse_portal_color_scheme(1), SystemTheme::Dark);
        assert_eq!(parse_portal_color_scheme(2), SystemTheme::Light);
        assert_eq!(parse_portal_color_scheme(0), SystemTheme::Unknown);
        assert_eq!(parse_portal_color_scheme(99), SystemTheme::Unknown);
    }

    #[test]
    fn no_preference_is_not_light() {
        assert_ne!(
            parse_portal_color_scheme(0),
            SystemTheme::Light,
            "no-preference must stay distinct from prefer-light"
        );
    }

    #[test]
    fn appearance_strings_parse() {
        assert_eq!(parse_appearance_string("prefer-dark"), SystemTheme::Dark);
        assert_eq!(parse_appearance_string("prefer-light"), SystemTheme::Light);
        assert_eq!(
            parse_appearance_string("no-preference"),
            SystemTheme::Unknown
        );
    }

    #[test]
    fn unknown_maps_to_none_never_to_false_light() {
        let _guard = lock_env();
        let _t = RestoreEnv::set("UTLC_THEME", "unknown");
        assert_eq!(
            system_theme_dark(),
            None,
            "unknown must be None, never Some(false)"
        );
        std::env::set_var("UTLC_THEME", "dark");
        assert_eq!(system_theme_dark(), Some(true));
        std::env::set_var("UTLC_THEME", "light");
        assert_eq!(
            system_theme_dark(),
            Some(false),
            "light is Some(false); unknown is None: distinct"
        );
    }

    #[test]
    fn cache_refresh_is_idempotent_under_env_override() {
        let _guard = lock_env();
        let _t = RestoreEnv::set("UTLC_THEME", "dark");
        let mut c = SystemThemeCache::new();
        assert_eq!(c.get(), SystemTheme::Dark);
        let v = c.get();
        c.refresh();
        assert_eq!(c.get(), v, "env override wins over any daemon");
    }

    #[test]
    fn portal_read_encodes_a_well_formed_call() {
        let mut buf = [0u8; MAX_PORTAL_MESSAGE];
        let n = encode_portal_read(&mut buf, 7).expect("read fits");
        check_call(&buf[..n], 7, "Read", PORTAL_BUS);
        for key in [
            PORTAL_NAMESPACE,
            PORTAL_COLOR_KEY,
            PORTAL_IFACE,
            PORTAL_PATH,
        ] {
            let word = [key.as_bytes(), b"\0"].concat();
            assert!(
                buf[..n].windows(word.len()).any(|w| w == word.as_slice()),
                "{key:?} rides in the message"
            );
        }
        assert!(
            encode_portal_read(&mut [0u8; 8], 1).is_none(),
            "a short buffer fails loudly"
        );
        assert!(
            encode_portal_read(&mut buf, 0).is_none(),
            "serial zero is refused"
        );
    }

    #[test]
    fn decode_portal_read_accepts_a_matching_return() {
        assert_eq!(
            decode_portal_read_reply(&fake_read_reply(41, 1), 41),
            Some(SystemTheme::Dark)
        );
        assert_eq!(
            decode_portal_read_reply(&fake_read_reply(9, 2), 9),
            Some(SystemTheme::Light)
        );
        assert_eq!(
            decode_portal_read_reply(&fake_read_reply(10, 0), 10),
            Some(SystemTheme::Unknown),
            "no-preference is Unknown, not Light"
        );
        assert_eq!(
            decode_portal_read_reply(&fake_read_reply(11, 99), 11),
            Some(SystemTheme::Unknown),
            "future values stay Unknown, never Light"
        );
    }

    #[test]
    fn decode_portal_read_rejects_mismatch_truncation_and_garbage() {
        let msg = fake_read_reply(41, 1);
        assert_eq!(
            decode_portal_read_reply(&msg, 42),
            None,
            "another call's reply"
        );
        assert_eq!(decode_portal_read_reply(&[], 41), None);
        for cut in 0..msg.len() {
            assert_eq!(
                decode_portal_read_reply(&msg[..cut], 41),
                None,
                "prefix of {cut} bytes must not decode"
            );
        }
        let mut err = fake_return(41, None, &[]);
        err[1] = DBUS_TYPE_ERROR;
        assert_eq!(
            decode_portal_read_reply(&err, 41),
            None,
            "errors are not themes"
        );
        let wrong = fake_return(41, Some("v"), &variant_string("dark"));
        assert_eq!(
            decode_portal_read_reply(&wrong, 41),
            None,
            "a string variant is not a color-scheme"
        );
        let wrong_sig = fake_return(41, Some("s"), &variant_u32(1));
        assert_eq!(
            decode_portal_read_reply(&wrong_sig, 41),
            None,
            "a non-variant body is not a Read reply"
        );
        assert_eq!(
            decode_portal_read_reply(&fake_return(41, Some("v"), b"nope"), 41),
            None,
            "a malformed variant body"
        );
        let garbage = [0xFFu8; 64];
        assert_eq!(decode_portal_read_reply(&garbage, 41), None);
    }

    // -------------------------------------------------------- the transport

    /// No socket, no portal, no panic: the absent-daemon state minimal images
    /// boot into.
    #[test]
    fn connect_to_a_missing_socket_is_none_not_a_panic() {
        let missing =
            std::env::temp_dir().join(format!("utim-portal-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let sock = missing.join("bus.sock");
        assert!(PortalThemeConnection::connect_to(sock.to_str().unwrap()).is_none());
    }

    /// A fake bus speaking SASL + Hello + Read, so the whole session is
    /// exercised with no portal on the machine.
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
    fn start_fake_portal(tag: &str, value: u32) -> (FakeBus, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!("utim-portal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fake portal dir");
        let sock_path = dir.join("bus.sock");
        let sock = sock_path.to_str().expect("temp path is UTF-8").to_string();
        let listener = UnixListener::bind(&sock_path).expect("fake portal bind");
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
                // `Read` contains `Hello` as a substring? No -- but `Hello`
                // is a prefix risk in the other direction, so check `Read`
                // first: a Read call must not be answered as a Hello.
                let reply = if raw.windows(4).any(|w| w == b"Read") {
                    fake_read_reply(serial, value)
                } else if raw.windows(5).any(|w| w == b"Hello") {
                    fake_return(serial, None, &[])
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

    /// The full session against the fake portal: handshake then read.
    /// `connect()` honours the address override, so it is covered here rather
    /// than against whatever the host happens to run.
    #[test]
    fn connect_and_read_against_a_fake_portal() {
        let _guard = lock_env();
        let _xdg = RestoreEnv::remove("XDG_RUNTIME_DIR");
        let old_session = std::env::var(SESSION_BUS_ADDRESS_ENV).ok();
        std::env::remove_var(SESSION_BUS_ADDRESS_ENV);

        let (bus, handle) = start_fake_portal("session", 1);
        let mut conn = PortalThemeConnection::connect_to(bus.path()).expect("fake handshake");
        assert_eq!(
            conn.read_color_scheme(),
            Some(SystemTheme::Dark),
            "one read is one theme"
        );
        drop(conn);
        handle.join().expect("fake portal thread");

        let (bus2, handle2) = start_fake_portal("env", 2);
        std::env::set_var(
            SESSION_BUS_ADDRESS_ENV,
            format!("unix:path={}", bus2.path()),
        );
        let mut env_conn = PortalThemeConnection::connect().expect("env address wins");
        assert_eq!(env_conn.read_color_scheme(), Some(SystemTheme::Light));
        drop(env_conn);
        if let Some(v) = old_session {
            std::env::set_var(SESSION_BUS_ADDRESS_ENV, v);
        } else {
            std::env::remove_var(SESSION_BUS_ADDRESS_ENV);
        }
        handle2.join().expect("second fake portal thread");
    }

    /// A portal that goes away is a clean `None` on read, without a panic.
    #[test]
    fn a_portal_that_goes_away_is_a_clean_none() {
        let (bus, handle) = start_fake_portal("drop", 1);
        let mut conn = PortalThemeConnection::connect_to(bus.path()).expect("fake handshake");
        conn.stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shutdown a live socket");
        assert_eq!(conn.read_color_scheme(), None);
        handle.join().expect("fake portal thread");
    }

    /// `connect()` with no portal anywhere never panics and never hangs past
    /// its timeouts. Asserts nothing about the value: on a host *with* a bus
    /// this may legitimately succeed.
    #[test]
    fn connect_with_no_portal_never_panics_or_hangs() {
        let _guard = lock_env();
        let _s = RestoreEnv::remove(SESSION_BUS_ADDRESS_ENV);
        let _x = RestoreEnv::remove("XDG_RUNTIME_DIR");
        let _t = RestoreEnv::set("UTLC_THEME", "unknown");
        // Force the runtime fallback to a missing socket by pointing it at a
        // scratch dir: isolates this test from whatever the host runs.
        let scratch =
            std::env::temp_dir().join(format!("utim-portal-nobus-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&scratch);
        let _r = RestoreEnv::set(
            "XDG_RUNTIME_DIR",
            scratch.to_str().expect("temp path is UTF-8"),
        );
        let _ = PortalThemeConnection::connect();
    }
}
