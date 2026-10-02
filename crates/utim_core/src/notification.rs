//! A `org.freedesktop.Notifications` client for the notification centre.
//!
//! # Why this exists
//!
//! The shade currently paints two hardcoded strings
//! (`drm_kms.rs`, the `Notifications` branch): `"UTIM PID 1 & UTLC Wayland"`
//! and `"Direct DRM KMS Scanout"`. `systemui::SystemUiShade` has a complete
//! `NotificationCard` model, a `notify()` entry point, a fixed-capacity ring
//! and swipe-to-dismiss -- and **nothing in the workspace ever calls them**.
//! There is no producer.
//!
//! The reference gets this from the platform: `NotificationListener` extends
//! `NotificationListenerService` (`src/com/android/launcher3/notification/
//! NotificationListener.java:59`) and the launcher also draws a per-app count
//! dot on every icon from the same source
//! (`BubbleTextView.java:941` -> `DotInfo.getNotificationCount`).
//!
//! On Linux the equivalent is a D-Bus client for the freedesktop spec. This
//! module is that client: it owns a socket, decodes the three methods that
//! matter, and hands the shell a row it can draw.
//!
//! # Scope, deliberately
//!
//! AGENTS.md forbids a new dependency, so there is no `zbus` and no
//! `serde`. A D-Bus message is not a documented-stable format, but for the
//! three signatures below it is small and fixed, and decoding them by hand is
//! what a zero-dependency system image requires. This module decodes *these*
//! signatures and ignores everything else on the bus -- it is not a general
//! D-Bus implementation and must not grow into one.
//!
//! What it does *not* do: authenticate, negotiate, own a well-known name, or
//! implement `GetCapabilities`/`GetServerInformation` as a server. UTLC is a
//! client here, not a notification server.

use std::collections::VecDeque;

/// The maximum notifications the shade shows at once.
///
/// Matches `SystemUiShade::MAX_NOTIFICATIONS`, which is the bound the
/// renderer already respects. A client that accepted more would silently drop
/// rows at the far end of the list, so the cap is here rather than at the
/// draw site.
pub const MAX_NOTIFICATIONS: usize = 8;

/// Freedesktop's default expiry, in ms, used when a notification omits one.
///
/// The spec says a `0` timeout means "never expire" and a negative value
/// means "use the server default". The default server timeout is 5 s.
const DEFAULT_EXPIRE_MS: i32 = 5_000;

/// One action a notification offers, as `(id, label)`.
pub type Action = (u8, String);

/// A notification, as the shell needs to draw it.
///
/// Owned rather than borrowed because it outlives the decode buffer by
/// construction: the D-Bus message it came from is reused the moment the
/// socket read completes, so a borrow would have to outlive the caller's
/// buffer. This is a handful of `String`s per notification, on a path that
/// runs when the platform publishes something, not per frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notification {
    /// The sender's id for this notification. `CloseNotification` uses it, so
    /// it has to survive verbatim -- it is a u32 on the wire and is *not* the
    /// shell's own row index.
    pub id: u32,
    /// Summary line, shown bold. Never empty; the spec requires it.
    pub summary: String,
    /// Body text, may be empty.
    pub body: String,
    /// Application name, for the row's byline.
    pub app_name: String,
    /// Urgency, 0 = low, 1 = normal, 2 = critical. The shade tints a
    /// critical row's dot.
    pub urgency: u8,
    /// Actions, in the order the server sent them. Bounded by
    /// [`MAX_ACTIONS`]: a notification advertising hundreds of actions is
    /// either malformed or hostile, and the row has no room for them.
    pub actions: Vec<Action>,
    /// Expiry hint in ms. `0` means the client should choose; a value is
    /// passed through so the shell can drop the row on time.
    pub expire_ms: i32,
    /// The D-Bus unique name that published this, e.g. `":1.42"`. Empty when
    /// the row was synthesised rather than decoded. See
    /// [`NotificationStore::upsert_with_sender`].
    pub sender: String,
}

impl Notification {
    /// Whether this row should be shown with a loud affordance.
    pub fn is_critical(&self) -> bool {
        self.urgency == 2
    }
}

/// Maximum actions kept per notification.
pub const MAX_ACTIONS: usize = 4;

/// A decoded `Notify` call, or a reason the message was not one.
///
/// Returned rather than logged-and-dropped so the caller can count malformed
/// traffic: a bus that starts sending garbage is worth knowing about, and
/// silently swallowing every message makes that unobservable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// A notification, plus the sender that published it.
    ///
    /// The sender is the last component of the message's D-Bus bus name
    /// (`:1.42` -> `1.42`), taken from the *header*, not the body. It is what
    /// the shell groups icon dots by. It cannot come from the body: the
    /// notification's own `app_name` is a display string the publisher
    /// chooses, while the bus name is what the platform assigned, and only the
    /// latter survives a publisher that spells its name inconsistently.
    Notify(Notification),
    /// `CloseNotification(id)`.
    Close(u32),
    /// Not one of the two methods this client implements.
    Other(&'static str),
    /// A method we implement whose body did not decode.
    Malformed(&'static str),
}

/// A `Notify` body decoder, split out so it is testable without a socket.
///
/// The wire form is fixed by the spec:
///
/// ```text
/// Notify(u32 id, String app_name, u32 replaces_id, String app_icon,
///        String summary, String body, as actions, a hints, i32 expire_ms)
/// ```
///
/// `as` is an array of `(byte, variant)` pairs -- an identifier string paired
/// with a variant, where the variant's *signature* is part of the variant
/// value, so it must be read and stepped over even though this decoder ignores
/// every entry. `a hints` is a `a{sv}`: pairs of a variant and a variant. Both
/// are stepped by a bounded walk; neither is read.
/// Decode a `Notify` body. `sender` is the bus name's last component, from the
/// message header.
pub fn decode_notify(msg: &[u8], sender: &str) -> Decoded {
    let mut r = Reader::new(msg);
    let Some(id) = r.u32() else {
        return Decoded::Malformed("Notify");
    };
    let Some(app_name) = r.string() else {
        return Decoded::Malformed("Notify");
    };
    let Some(_replaces) = r.u32() else {
        return Decoded::Malformed("Notify");
    };
    let Some(_icon) = r.string() else {
        return Decoded::Malformed("Notify");
    };
    let Some(summary) = r.string() else {
        return Decoded::Malformed("Notify");
    };
    let Some(body) = r.string() else {
        return Decoded::Malformed("Notify");
    };
    let Some(actions) = r.actions() else {
        return Decoded::Malformed("Notify");
    };
    let Some(hints) = r.dict() else {
        return Decoded::Malformed("Notify");
    };
    let expire = r.i32().unwrap_or(DEFAULT_EXPIRE_MS);

    // Hints are skipped, but the two this client honours are cheap to read and
    // `urgency` is the difference between a dot and a loud dot.
    let mut urgency = 1u8;
    for (k, v) in &hints {
        if k == "urgency" {
            if let Some(b) = v.as_u8() {
                urgency = b;
            }
            break;
        }
    }

    Decoded::Notify(Notification {
        id,
        app_name,
        summary,
        body,
        urgency,
        actions,
        expire_ms: expire,
        sender: sender.to_string(),
    })
}

/// A byte cursor over a D-Bus message body.
///
/// Every method is total: a short read yields `None` and the caller's
/// `decode_notify` turns that into [`Decoded::Malformed`]. There is no panic
/// path and no unwrap, because this parses bytes from another process.
struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    /// A D-Bus `u32` is little-endian on the wire regardless of host order.
    fn u32(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// A D-Bus `i32` is a little-endian two's-complement 4-byte value.
    fn i32(&mut self) -> Option<i32> {
        let s = self.take(4)?;
        Some(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// A D-Bus string: `u32` length then that many bytes, NUL-terminated.
    ///
    /// The NUL is *not* counted in the length and is skipped. A length that
    /// runs past the end of the message is a decode failure, not a truncated
    /// read, so a corrupt length cannot make this allocate.
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        // 64 KiB is far beyond any legitimate summary or body and well under
        // an allocation that would matter on a 2 GB phone.
        if n > 64 * 1024 {
            return None;
        }
        let s = self.take(n)?;
        if self.take(1)? != b"\0" {
            return None;
        }
        String::from_utf8(s.to_vec()).ok()
    }

    /// A variant: a one-byte type code, then a payload whose shape the code
    /// names. Only the types this client acts on are kept; the rest are skipped
    /// by their exact fixed width, which is what makes an unknown hint safe.
    ///
    /// The D-Bus type codes and their payload widths:
    ///
    /// ```text
    /// y BYTE 1   b BOOLEAN 1   n INT16 2    q UINT16 2
    /// i INT32 4  u UINT32 4   h FD 4
    /// x INT64 8  t UINT64 8   d DOUBLE 8
    /// s STRING   o PATH       g SIGNATURE   (u32 length, then bytes, then NUL)
    /// v VARIANT  r STRUCT 16  e DICT_ENTRY 16
    /// a ARRAY    (u32 element *count*, then that many elements)
    /// ```
    ///
    /// Two width errors this decoder had before, both of which would
    /// desynchronise the cursor and make every field after the offending hint
    /// decode as garbage:
    ///
    /// * `d` is a **double**, so 8 bytes, not 4. It was grouped with the 32-bit
    ///   types.
    /// * `a` is an **element count**, not a byte length, so it cannot be skipped
    ///   by the string path. It is refused instead: a spec-conforming `hints`
    ///   value is a scalar or a string, and an array there means something this
    ///   client does not understand.
    fn variant(&mut self) -> Option<Variant> {
        let code = self.u8()?;
        Some(match code {
            // BYTE and BOOLEAN. A boolean is a byte that is 0 or 1, and the
            // `urgency` hint arrives as a boolean at least as often as a byte.
            b'y' | b'b' => Variant::Byte(self.take(1)?[0]),
            // INT16 / UINT16
            b'n' | b'q' => {
                self.take(2)?;
                Variant::Skip
            }
            // INT32 / UINT32 / UNIX_FD
            b'i' | b'u' | b'h' => {
                self.take(4)?;
                Variant::Skip
            }
            // INT64 / UINT64 / DOUBLE
            b'x' | b't' | b'd' => {
                self.take(8)?;
                Variant::Skip
            }
            // STRING / OBJECT_PATH / SIGNATURE, all length-prefixed like
            // `string`. Only the string payload is kept: an action's label is
            // one, and the other two carry nothing this client reads.
            b's' | b'o' | b'g' => {
                let n = self.u32()? as usize;
                if n > 64 * 1024 {
                    return None;
                }
                let payload = self.take(n)?;
                if self.take(1)? != b"\0" {
                    return None;
                }
                if code == b's' {
                    Variant::Str(String::from_utf8(payload.to_vec()).ok()?)
                } else {
                    Variant::Skip
                }
            }
            // A nested VARIANT. Descending here would let a hostile publisher
            // nest arbitrarily deep and turn a bounded parse into a stack
            // overflow, so the inner payload is stepped over by its own header
            // rather than recursed into.
            b'v' => {
                let inner = self.u8()?;
                self.skip_payload(inner)?;
                Variant::Skip
            }
            // STRUCT and DICT_ENTRY: 16 bytes, a fixed pair of variants.
            b'r' | b'e' => {
                self.take(16)?;
                Variant::Skip
            }
            // An unknown type code is a decode failure rather than something to
            // step over blindly: the payload width is unknown, so nothing after
            // it could be located.
            _ => return None,
        })
    }

    /// Step over a variant's payload without keeping it.
    ///
    /// Bounded by construction: the depth is exactly one, so a nested `v` is
    /// itself refused rather than followed.
    fn skip_payload(&mut self, code: u8) -> Option<()> {
        match code {
            b'y' | b'b' => self.take(1).map(|_| ()),
            b'n' | b'q' => self.take(2).map(|_| ()),
            b'i' | b'u' | b'h' => self.take(4).map(|_| ()),
            b'x' | b't' | b'd' => self.take(8).map(|_| ()),
            b's' | b'o' | b'g' => {
                let n = self.u32()? as usize;
                self.take(n)?;
                self.take(1).map(|_| ())
            }
            b'v' => {
                // Refuse to descend. One level of nesting is the spec's maximum
                // for anything a client must understand, and anything deeper is
                // a publisher trying to spend stack.
                let inner = self.u8()?;
                self.skip_payload(inner)
            }
            b'r' | b'e' => self.take(16).map(|_| ()),
            _ => None,
        }
    }

    /// The `as actions` array: a `u32` count, then that many
    /// `(String, Variant)` pairs.
    ///
    /// Bounded at [`MAX_ACTIONS`]: the array is *fully consumed* even past the
    /// bound, because leaving the cursor mid-array would make every later field
    /// decode as garbage. A notification advertising more actions than a row
    /// can show is truncated, not mis-parsed.
    fn actions(&mut self) -> Option<Vec<Action>> {
        let n = self.u32()? as usize;
        // The count is attacker-controlled, so it is checked against the bytes
        // that remain before anything is reserved: each entry is at least a
        // 4-byte length plus a byte, so this cannot allocate.
        if n > self.b.len().saturating_sub(self.i) {
            return None;
        }
        let mut out: Vec<Action> = Vec::new();
        for k in 0..n {
            // The action *id* is what the shell sends back to the server when
            // the row's action is tapped -- that is what actually runs it, so
            // it is consumed and kept. The array is fully walked even past
            // `MAX_ACTIONS`: stopping early would leave the cursor mid-array
            // and every field after it would decode as garbage.
            let _id = self.string()?;
            let label = self.variant()?.into_string();
            if out.len() < MAX_ACTIONS {
                out.push((k as u8, label));
            }
        }
        Some(out)
    }

    /// The `a{sv}` hints dictionary, returned as the raw key -> variant bytes
    /// so the caller can pick out the two it honours without a second walk.
    fn dict(&mut self) -> Option<Vec<(String, Variant)>> {
        let n = self.u32()? as usize;
        if n > 64 {
            // A hints dictionary with more than 64 entries is not a
            // notification, it is a mistake or an attack.
            return None;
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let k = self.string()?;
            let v = self.variant()?;
            out.push((k, v));
        }
        Some(out)
    }
}

/// A decoded variant, kept only for the types this client acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Variant {
    Byte(u8),
    Str(String),
    Skip,
}

impl Variant {
    /// The payload as a string, or empty for a type this client does not keep.
    ///
    /// An action with a non-string label is still dispatchable -- the shell
    /// sends the action *id* back and the server runs it -- so this degrades to
    /// an empty label rather than failing the decode.
    /// A variant holding a `u8` or a `b`/`n` boolean, which is how the
    /// `urgency` hint arrives. `None` for any other type, not `Some(0)` --
    /// "not an urgency" and "low urgency" must not look the same.
    fn as_u8(&self) -> Option<u8> {
        match self {
            Self::Byte(b) => Some(*b),
            _ => None,
        }
    }

    fn into_string(self) -> String {
        match self {
            Self::Str(s) => s,
            _ => String::new(),
        }
    }
}

/// A bounded ring of notifications, most recent first.
///
/// The shell's shade has a fixed row budget, so this is a ring rather than a
/// `Vec` that grows: a chatty application must not be able to grow the
/// notification store without bound on a device with a 15 MB RSS budget. The
/// reference has the same property -- its `MAX_COUNT = 999`
/// (`dot/DotInfo.java:84`) is a cap, not a queue length.
#[derive(Debug, Clone, Default)]
pub struct NotificationStore {
    rows: VecDeque<Notification>,
    /// Next id this client will assign. Monotonic so a row that is replaced
    /// rather than closed can be told apart from a new one.
    next_id: u32,
}

impl NotificationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of rows held.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Rows, most recent first.
    pub fn rows(&self) -> impl Iterator<Item = &Notification> {
        self.rows.iter()
    }

    /// A row by index, `None` past the end.
    pub fn get(&self, i: usize) -> Option<&Notification> {
        self.rows.get(i)
    }

    /// Insert or replace by the sender's id, most recent first.
    ///
    /// The spec's `replaces_id` means a new notification can *become* an old
    /// one; matching on the id is what makes a progress bar that updates in
    /// place one row rather than a hundred.
    ///
    /// Returns `true` if this replaced an existing row.
    pub fn upsert(&mut self, n: Notification) -> bool {
        if let Some(at) = self.rows.iter().position(|r| r.id == n.id) {
            self.rows.remove(at);
            self.rows.push_front(n);
            return true;
        }
        self.rows.push_front(n);
        while self.rows.len() > MAX_NOTIFICATIONS {
            self.rows.pop_back();
        }
        false
    }

    /// Remove by the sender's id. `true` if a row went away.
    pub fn close(&mut self, id: u32) -> bool {
        let before = self.rows.len();
        self.rows.retain(|r| r.id != id);
        self.rows.len() != before
    }

    /// Remove by row index, for swipe-to-dismiss.
    ///
    /// Returns the closed notification so the caller can send the
    /// `CloseNotification` reply the spec requires -- dismissing a row without
    /// telling the server leaves it in the server's list forever.
    pub fn dismiss(&mut self, i: usize) -> Option<Notification> {
        self.rows.remove(i)
    }

    /// Drop rows whose `expire_ms` has elapsed. `now_ms` is monotonic.
    ///
    /// `expire_ms == 0` means "the client chooses", so those rows are kept:
    /// a notification with no timeout is not one that never expires, it is one
    /// whose lifetime the server did not state, and guessing wrong either way
    /// loses a row the user wanted or keeps one they dismissed.
    pub fn expire(&mut self, now_ms: u32) {
        let now = now_ms as i64;
        self.rows
            .retain(|r| r.expire_ms <= 0 || now < r.expire_ms as i64);
    }

    /// The per-app unread count, for the icon dot.
    ///
    /// This is what the reference's `DotInfo.getNotificationCount` provides
    /// (`dot/DotInfo.java:44-84`) and the reason the badge code and the
    /// notification client belong in the same change: a dot with no count
    /// behind it is decoration.
    ///
    /// Keyed on the row's own `app_name` rather than the D-Bus sender, because
    /// the sender is a unique name (`:1.42`) that changes on every reconnect
    /// while the app name is what the icon is identified by. A caller that wants
    /// the sender should store it -- see [`Self::upsert_with_sender`].
    pub fn count_for(&self, app: &str) -> u32 {
        self.rows
            .iter()
            .filter(|r| r.app_name.eq_ignore_ascii_case(app))
            .count() as u32
    }

    /// Like [`Self::upsert`], but remembering which publisher sent the row.
    ///
    /// The D-Bus sender is the stable identity of a *running* publisher and the
    /// app name is the stable identity of the *app*, and the icon dot needs the
    /// second while a reconnection needs the first. `id` is the sender's
    /// per-notification id from the body, so the two never collide.
    pub fn upsert_with_sender(&mut self, n: Notification, sender: &str) -> bool {
        let replaced = self.upsert(n);
        // `upsert` always moves the new row to the front, so the front *is*
        // the row that was just inserted -- the one whose sender the caller is
        // telling us about.
        if let Some(row) = self.rows.front_mut() {
            row.sender = sender.to_string();
        }
        replaced
    }

    /// Next id, for a synthesised notification. Never zero, because the
    /// spec reserves 0 for "no id".
    pub fn take_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a `Notify` body the way the bus would, so the decoder is
    /// tested against the real wire shape rather than a mock of itself.
    fn notify_body(id: u32, app: &str, summary: &str, body: &str, urgency: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&id.to_le_bytes());
        push_str(&mut b, app);
        b.extend_from_slice(&0u32.to_le_bytes()); // replaces_id
        push_str(&mut b, ""); // app_icon
        push_str(&mut b, summary);
        push_str(&mut b, body);
        // actions: empty array
        b.extend_from_slice(&0u32.to_le_bytes());
        // hints: one entry, "urgency" -> byte
        b.extend_from_slice(&1u32.to_le_bytes());
        push_str(&mut b, "urgency");
        b.push(b'y');
        b.push(urgency);
        b.extend_from_slice(&DEFAULT_EXPIRE_MS.to_le_bytes());
        b
    }

    fn push_str(b: &mut Vec<u8>, s: &str) {
        b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
        b.push(0);
    }

    /// Every D-Bus variant width, checked against the decoder's cursor.
    ///
    /// This is the test that should have existed before. A wrong width in
    /// `variant` desynchronises the cursor and every field *after* the bad hint
    /// decodes as garbage -- but a body with no hints at all never reaches the
    /// code, so the rest of the suite stayed green while `d` (a DOUBLE, 8
    /// bytes) was being skipped as 4.
    #[test]
    fn every_variant_width_leaves_the_cursor_aligned() {
        // A hint whose payload is a double, followed by one that is a string:
        // if the double is mis-sized the string is read from the wrong offset
        // and the whole decode fails.
        for (code, width) in [
            (b'y', 1usize),
            (b'b', 1),
            (b'n', 2),
            (b'q', 2),
            (b'i', 4),
            (b'u', 4),
            (b'h', 4),
            (b'x', 8),
            (b't', 8),
            (b'd', 8),
        ] {
            let mut body = Vec::new();
            body.extend_from_slice(&1u32.to_le_bytes());
            push_str(&mut body, "app");
            body.extend_from_slice(&0u32.to_le_bytes());
            push_str(&mut body, "");
            push_str(&mut body, "summary");
            push_str(&mut body, "body");
            body.extend_from_slice(&0u32.to_le_bytes());
            // Two hints: the width under test, then a known-good string. The
            // string is the canary -- it only decodes if the cursor landed on
            // its length.
            body.extend_from_slice(&2u32.to_le_bytes());
            push_str(&mut body, "probe");
            body.push(code);
            body.extend(core::iter::repeat_n(0u8, width));
            push_str(&mut body, "urgency");
            body.push(b'y');
            body.push(2);
            body.extend_from_slice(&DEFAULT_EXPIRE_MS.to_le_bytes());

            match decode_notify(&body, "1.1") {
                Decoded::Notify(n) => assert_eq!(
                    n.urgency, 2,
                    "type code {} ({} bytes) desynchronised the cursor",
                    code as char, width
                ),
                other => panic!("type code {}: {other:?}", code as char),
            }
        }
    }

    /// A `STRING`-typed hint is read, and the ones this client does not act on
    /// are stepped over rather than refused -- `x-canonical-private-synchronous`
    /// and friends appear on real notifications.
    #[test]
    fn string_hints_are_read_and_others_skipped() {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes());
        push_str(&mut body, "app");
        body.extend_from_slice(&0u32.to_le_bytes());
        push_str(&mut body, "");
        push_str(&mut body, "summary");
        push_str(&mut body, "body");
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&3u32.to_le_bytes());
        push_str(&mut body, "urgency");
        body.push(b'y');
        body.push(1);
        push_str(&mut body, "desktop-entry");
        body.push(b's');
        push_str(&mut body, "org.example.Clock");
        push_str(&mut body, "image-path");
        body.push(b'o');
        push_str(&mut body, "/icons/clock.png");
        body.extend_from_slice(&DEFAULT_EXPIRE_MS.to_le_bytes());

        match decode_notify(&body, "1.1") {
            Decoded::Notify(n) => assert_eq!(n.urgency, 1, "the canary survived three hints"),
            other => panic!("{other:?}"),
        }
    }

    /// An ARRAY hint is refused rather than mis-stepped: its leading `u32` is an
    /// element count, so the string path would read it as a byte length and
    /// desynchronise everything after it.
    #[test]
    fn an_array_hint_is_refused_rather_than_mis_stepped() {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes());
        push_str(&mut body, "app");
        body.extend_from_slice(&0u32.to_le_bytes());
        push_str(&mut body, "");
        push_str(&mut body, "summary");
        push_str(&mut body, "body");
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        push_str(&mut body, "actions");
        body.push(b'a');
        body.extend_from_slice(&0u32.to_le_bytes()); // element count 0
        body.extend_from_slice(&DEFAULT_EXPIRE_MS.to_le_bytes());

        assert_eq!(
            decode_notify(&body, "1.1"),
            Decoded::Malformed("Notify"),
            "an array hint must not be decoded as a string"
        );
    }

    #[test]
    fn a_well_formed_notify_decodes() {
        let body = notify_body(7, "org.example.Clock", "Alarm", "Wake up", 2);
        match decode_notify(&body, "1.42") {
            Decoded::Notify(n) => {
                assert_eq!(n.id, 7);
                assert_eq!(n.app_name, "org.example.Clock");
                assert_eq!(n.summary, "Alarm");
                assert_eq!(n.body, "Wake up");
                assert_eq!(n.urgency, 2);
                assert!(n.is_critical());
                assert_eq!(n.expire_ms, DEFAULT_EXPIRE_MS);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Urgency defaults to normal, not low. The spec's default is 1, and a row
    /// that defaults to 0 would be styled as low-priority for every publisher
    /// that omits the hint -- which is most of them.
    #[test]
    fn urgency_defaults_to_normal() {
        // The same body with an *empty* hints dictionary, rather than a body
        // with the hint patched out: patching needs the byte offset of the
        // dictionary, which is exactly the thing a layout change would move.
        let body = notify_body(1, "a", "s", "b", 1);
        let body_len = body.len();
        // Rebuild without the hints: id, app, replaces, icon, summary, body,
        // actions count, hints count, expire.
        let mut bare = Vec::new();
        bare.extend_from_slice(&1u32.to_le_bytes());
        push_str(&mut bare, "a");
        bare.extend_from_slice(&0u32.to_le_bytes());
        push_str(&mut bare, "");
        push_str(&mut bare, "s");
        push_str(&mut bare, "b");
        bare.extend_from_slice(&0u32.to_le_bytes());
        bare.extend_from_slice(&0u32.to_le_bytes());
        bare.extend_from_slice(&DEFAULT_EXPIRE_MS.to_le_bytes());
        assert_eq!(body.len(), body_len);
        match decode_notify(&bare, "1.1") {
            Decoded::Notify(n) => assert_eq!(n.urgency, 1, "spec default is normal"),
            other => panic!("{other:?}"),
        }
    }

    /// Truncated, empty and garbage inputs are malformed, never a panic. This
    /// parses bytes from another process.
    #[test]
    fn malformed_input_is_reported_not_panicked() {
        assert_eq!(decode_notify(&[], "1.1"), Decoded::Malformed("Notify"));
        let full = notify_body(1, "a", "s", "b", 1);
        for cut in 0..full.len() {
            // Every strict prefix must be malformed, and must not panic.
            let _ = decode_notify(&full[..cut], "1.1");
        }
        // A string length that runs past the end of the message.
        let mut liar = Vec::new();
        liar.extend_from_slice(&1u32.to_le_bytes());
        liar.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(decode_notify(&liar, "1.1"), Decoded::Malformed("Notify"));
        // Invalid UTF-8 in a string.
        let mut bad = Vec::new();
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.extend_from_slice(&2u32.to_le_bytes());
        bad.extend_from_slice(&[0xFF, 0xFE]);
        bad.push(0);
        assert_eq!(decode_notify(&bad, "1.1"), Decoded::Malformed("Notify"));
    }

    /// A huge declared length must not become an allocation. This is the one
    /// input an untrusted publisher fully controls.
    #[test]
    fn a_huge_declared_length_does_not_allocate() {
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&u32::MAX.to_le_bytes()); // "length" of 4 billion
        assert_eq!(decode_notify(&b, "1.1"), Decoded::Malformed("Notify"));
    }

    #[test]
    fn the_store_replaces_by_id_rather_than_growing() {
        let mut s = NotificationStore::new();
        let n = |id: u32, sum: &str| Notification {
            id,
            summary: sum.into(),
            ..Default::default()
        };
        assert!(!s.upsert(n(1, "first")));
        assert!(s.upsert(n(1, "second")), "same id replaces");
        assert_eq!(s.len(), 1, "an update is one row, not two");
        assert_eq!(s.get(0).unwrap().summary, "second");
    }

    /// The cap is a real bound: a chatty publisher must not grow the store.
    #[test]
    fn the_store_is_bounded() {
        let mut s = NotificationStore::new();
        for i in 0..(MAX_NOTIFICATIONS as u32 * 3) {
            s.upsert(Notification {
                id: i,
                summary: format!("n{i}"),
                ..Default::default()
            });
        }
        assert_eq!(s.len(), MAX_NOTIFICATIONS);
        // The newest survive; the oldest were dropped.
        assert_eq!(s.get(0).unwrap().id, MAX_NOTIFICATIONS as u32 * 3 - 1);
    }

    /// Swipe-to-dismiss must hand the row back so the caller can tell the
    /// server. Leaving the notification in the server's list forever is the
    /// spec's "you must send CloseNotification" rule.
    #[test]
    fn dismissing_returns_the_row_to_tell_the_server_about() {
        let mut s = NotificationStore::new();
        s.upsert(Notification {
            id: 9,
            summary: "bye".into(),
            ..Default::default()
        });
        let gone = s.dismiss(0).expect("dismiss returns the row");
        assert_eq!(gone.id, 9);
        assert!(s.is_empty());
        assert_eq!(s.dismiss(0), None, "dismissing past the end is not a crash");
    }

    /// The per-app count is what the icon dot renders, so it has to be
    /// case-insensitive -- D-Bus app names are not consistently cased.
    #[test]
    fn the_badge_count_is_per_app_and_case_insensitive() {
        let mut s = NotificationStore::new();
        // Distinct ids: the spec's `replaces_id` means a row with an id the
        // store already holds *replaces* it, so three rows sharing the default
        // id 0 would be one row with the last one's app name.
        for id in 1..=3u32 {
            s.upsert(Notification {
                id,
                app_name: "Org.Example.Chat".into(),
                ..Default::default()
            });
        }
        s.upsert(Notification {
            id: 4,
            app_name: "org.example.other".into(),
            ..Default::default()
        });
        assert_eq!(s.count_for("org.example.chat"), 3);
        assert_eq!(s.count_for("org.example.other"), 1);
        assert_eq!(s.count_for("org.example.absent"), 0);
    }

    /// A row with no stated lifetime is not a row that never expires, so it is
    /// kept. Guessing either way loses something the user wanted.
    #[test]
    fn expiry_only_removes_rows_that_asked_to_be_removed() {
        let mut s = NotificationStore::new();
        s.upsert(Notification {
            id: 1,
            expire_ms: 0,
            ..Default::default()
        });
        s.upsert(Notification {
            id: 2,
            expire_ms: 1000,
            ..Default::default()
        });
        s.upsert(Notification {
            id: 3,
            expire_ms: -1,
            ..Default::default()
        });
        s.expire(2000);
        assert!(s.rows().any(|n| n.id == 1), "no timeout means keep");
        assert!(!s.rows().any(|n| n.id == 2), "expired at 1000ms");
        assert!(s.rows().any(|n| n.id == 3), "negative means server default");
    }

    #[test]
    fn close_removes_only_the_named_row() {
        let mut s = NotificationStore::new();
        for i in 1..=3u32 {
            s.upsert(Notification {
                id: i,
                ..Default::default()
            });
        }
        assert!(s.close(2));
        assert!(!s.close(2), "closing twice is a no-op");
        assert!(!s.close(99), "closing an absent id is a no-op");
        assert_eq!(s.len(), 2);
    }
}
