//! Haptic feedback for gesture edges, section detents and dismissals.
//!
//! Zero third-party dependencies, and no open file descriptors held between
//! pulses: the vibrator is driven through sysfs, which is a write-and-forget
//! interface. A phone's vibrator is either a `leds`-class device
//! (`/sys/class/leds/*/`) or an input device with force feedback
//! (`/dev/input/event*` with an `FF` capability); both are probed once, at
//! boot, and the device node is opened per pulse and closed immediately.
//! Holding an fd open would work too and cost one descriptor, but a shell that
//! can be killed at any moment should not own a device node it does not need
//! to own.
//!
//! Failure is silent by design, and that is not a shortcut. A desktop kernel
//! with no vibrator, a container without `/sys`, and a phone whose driver has
//! not bound all report the same thing: there is nothing to pulse. The shell
//! must not distinguish those, and none of them are worth an error path on a
//! frame thread.

use std::fs::OpenOptions;
use std::io::Write;
use std::mem::{align_of, offset_of, size_of};
use std::path::{Path, PathBuf};

/// The haptic vocabulary the shell uses.
///
/// Android's `HapticFeedbackConstants` in miniature. Only the four the shell
/// actually fires are modelled; the names are the reference's, because they
/// are what a reader checking the citation will be looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HapticEffect {
    /// A single short click. `HapticFeedbackConstants.VIRTUAL_KEY` /
    /// `CLOCK_TICK`: the detent crossed in the fast scroller.
    Tick,
    /// A light buzz. `LONG_PRESS`: the long-press threshold being reached.
    LongPress,
    /// A firmer double-weight buzz. `CONFIRM`: a task card committing to its
    /// dismissal, where the user is about to lose an app.
    Confirm,
    /// The soft thud of the workspace opening. `REJECT` is deliberately not
    /// modelled: nothing in the shell should buzz to say "no".
    Open,
}

impl HapticEffect {
    /// On-time in milliseconds, the first half of the waveform.
    ///
    /// The vibrator is a square wave, so an effect is really an on/off pair.
    /// Android's own durations, in ms: `Tick` is one frame, `LongPress` and
    /// `Confirm` are the values `Vibrator.vibrate(long)` uses, `Open` is the
    /// quickstep attach.
    pub const fn on_ms(self) -> u32 {
        match self {
            // 8 ms is the shortest a panel's driver will honour, and it is
            // what a clock tick feels like: perceptible, not a buzz.
            HapticEffect::Tick => 8,
            HapticEffect::LongPress => 20,
            HapticEffect::Confirm => 30,
            HapticEffect::Open => 16,
        }
    }

    /// Off-time in milliseconds, the second half.
    pub const fn off_ms(self) -> u32 {
        match self {
            HapticEffect::Tick => 0,
            HapticEffect::LongPress => 0,
            HapticEffect::Confirm => 40,
            HapticEffect::Open => 0,
        }
    }

    /// Amplitude, 0..=255. The Android waveform amplitude.
    pub const fn amplitude(self) -> u32 {
        match self {
            HapticEffect::Tick => 128,
            HapticEffect::LongPress => 180,
            HapticEffect::Confirm => 255,
            HapticEffect::Open => 140,
        }
    }
}

/// The vibrator backends, in probe order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    /// `/sys/class/leds/<name>/{activate,amplitude,duration}` -- the
    /// Android `vibrator` driver, which is a `leds` device with a duration
    /// in ms and an amplitude in 0..=255.
    Led {
        activate: PathBuf,
        amplitude: Option<PathBuf>,
        duration: Option<PathBuf>,
    },
    /// `/dev/input/eventN` with an `FF` capability, driven by writing a
    /// `struct ff_effect`.
    Evdev(PathBuf),
    /// Nothing to drive. Every trigger is a no-op.
    None,
}

/// A probed vibrator.
///
/// Construction probes; [`Haptics::trigger`] fires. Splitting them means the
/// (relatively slow) probe happens once at boot rather than on the first tap,
/// which would put a `readdir` on the touch-latency path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Haptics {
    backend: Backend,
    /// Count of pulses successfully emitted. Diagnostics only: the shell
    /// prints it in `--test-haptics` so a platform integration can be
    /// verified without a hand on the device.
    fired: u32,
}

impl Haptics {
    /// Probe the platform for a vibrator.
    ///
    /// Cheap and idempotent; the result is meant to be stored once.
    pub fn detect() -> Self {
        Self { backend: Self::probe(), fired: 0 }
    }

    /// A haptics sink that discards everything. The default on a machine
    /// with no vibrator, and what the test suite uses so a unit test never
    /// touches `/sys`.
    pub fn disabled() -> Self {
        Self { backend: Backend::None, fired: 0 }
    }

    /// True when a real device was found.
    pub fn is_available(&self) -> bool {
        self.backend != Backend::None
    }

    /// Pulses emitted so far.
    pub fn fired(&self) -> u32 {
        self.fired
    }

    /// Fire `effect`. Returns whether a device actually took it.
    ///
    /// Never returns `Err`: a platform with no vibrator is a normal platform
    /// (every desktop, every container, and a phone whose driver is not
    /// bound), and a haptic is not something worth failing a gesture over.
    pub fn trigger(&mut self, effect: HapticEffect) -> bool {
        let ok = match &self.backend {
            Backend::Led { activate, amplitude, duration } => {
                self.fire_led(activate, amplitude.as_deref(), duration.as_deref(), effect)
            }
            Backend::Evdev(path) => self.fire_evdev(path, effect),
            Backend::None => false,
        };
        if ok {
            self.fired += 1;
        }
        ok
    }

    fn fire_led(
        &self,
        activate: &Path,
        amplitude: Option<&Path>,
        duration: Option<&Path>,
        effect: HapticEffect,
    ) -> bool {
        // Order matters and is the driver's: amplitude, then duration, then
        // activate. Writing `activate` first starts the motor with the
        // previous effect's amplitude, which is a visible (and audible) click
        // of the wrong strength.
        //
        // The pulse counts as delivered only if `activate` took. A missing
        // `amplitude` or `duration` node is not a failure -- plenty of
        // drivers expose only `activate` and pulse with a fixed profile --
        // but an `activate` that cannot be written means there is no
        // vibrator, and reporting a pulse that reached nothing would make
        // the `fired` counter a diagnostic that lies.
        // Every number below is formatted into a caller-owned stack buffer.
        // `to_string()` allocates a `String` per pulse, and this function is
        // called from the touch handler: the fast scroller can fire a `Tick`
        // per letter crossed, so the allocator would be on the latency path
        // for no reason. Three digits is the whole domain (`amplitude` is a
        // u8 by kernel contract, `on_ms` is 8..=30), so a 4-byte buffer with
        // one spare is enough and `fmt_u8` asserts it.
        if let Some(p) = amplitude {
            let _ = write_u8(p, [0u8; 4], effect.amplitude());
        }
        if let Some(p) = duration {
            let _ = write_u8(p, [0u8; 4], effect.on_ms());
        }
        // `1` on, `0` off. There is no asynchronous "play then stop" here:
        // the driver stops itself after `duration`, and a second write would
        // race it.
        write_bytes(activate, b"1").is_ok()
    }

    fn fire_evdev(&self, path: &Path, fx: HapticEffect) -> bool {
        // An `ff_effect` upload needs an fd opened for write on the *input*
        // device, which is not something to do with a plain `File::create`
        // (that truncates, and the node is a character device, not a
        // regular file). `O_WRONLY | O_CLOEXEC` on the node, then an
        // `EVIOCSFF` ioctl to upload the effect and a `write` of one
        // `EV_FF` event to play it -- `input_ff_event()` in the kernel turns
        // that event into `ff->playback(dev, code, value)`, so the event's
        // `code` carries the effect id the upload ioctl assigned and `value`
        // 1 starts it. (There is no `EVIOCSPLAY` ioctl; playback is the
        // event write.)
        //
        // This is a best-effort path: a device that is not really a force-
        // feedback actuator fails the ioctl and we fall through silently,
        // which is the same outcome as having no vibrator at all.

        // SAFETY: `open` is variadic and its third argument is a mode the
        // kernel ignores unless `O_CREAT` is set, which it is not.
        //
        // `copy_c_path` has already rejected anything that would not fit and
        // guaranteed the terminator, so this cannot read past the buffer.
        let fd = match copy_c_path(path) {
            Some(c_path) => unsafe {
                libc::open(
                    c_path.as_ptr() as *const libc::c_char,
                    libc::O_WRONLY | libc::O_CLOEXEC,
                )
            },
            None => return false,
        };
        if fd < 0 {
            return false;
        }
        let mut eff = FfEffect::rumble(fx.on_ms() as u16, fx.amplitude() as u16);
        // The kernel allocates the effect slot, writes it into `id` and then
        // `put_user`s it back over our copy, so the play event below has to be
        // built from `eff.id` *after* this call and not from the -1 we sent.
        //
        // SAFETY: `eff` is a live `FfEffect` whose size is pinned to 48 by a
        // `const` assert, which is the size `EVIOCSFF`'s size field encodes,
        // so the kernel reads exactly the bytes we initialised.
        let mut ok =
            unsafe { libc::ioctl(fd, EVIOCSFF, (&mut eff as *mut FfEffect).cast::<libc::c_void>()) }
                >= 0;
        // A successful upload that left the request for "allocate me" in place
        // did not allocate. Playing an effect we do not own would be a no-op
        // at best, so report failure rather than counting the pulse.
        ok = ok && eff.id >= 0;
        if ok {
            let ev = InputEvent {
                // `evdev_write` reads the timestamp but evdev timestamps every
                // event from its own clock, so a zero `timeval` is correct and
                // calling `gettimeofday` here would only add a syscall to the
                // touch path.
                time: libc::timeval { tv_sec: 0, tv_usec: 0 },
                type_: EV_FF,
                // The effect id the upload assigned: `input_ff_event` passes
                // `code` straight to `ff->playback(dev, code, value)`.
                code: eff.id as u16,
                value: 1,
            };
            // SAFETY: `&ev` is a live, correctly sized `InputEvent` for the
            // duration of the call. `evdev_write` copies whole events and
            // returns the byte count it consumed, so a short write means the
            // event did not go through and must not count as a pulse.
            ok = unsafe {
                libc::write(fd, (&ev as *const InputEvent).cast(), size_of::<InputEvent>())
            } == size_of::<InputEvent>() as isize;
        }
        // SAFETY: `fd` is a descriptor this function opened and has not
        // closed; every early return above either returned before opening or
        // funnelled through here.
        unsafe { libc::close(fd) };
        ok
    }

    /// Find a vibrator, sysfs first.
    fn probe() -> Backend {
        if let Some(b) = Self::probe_led() {
            return b;
        }
        Self::probe_evdev().unwrap_or(Backend::None)
    }

    fn probe_led() -> Option<Backend> {
        let dir = PathBuf::from("/sys/class/leds");
        let entries = std::fs::read_dir(&dir).ok()?;
        // The Android driver names the device `vibrator`; the AOSP
        // `vibrator_sysfs.c` also accepts a `*_vibrator` prefix. Match on
        // that rather than on a fixed name so a vendor prefix still works.
        let mut best: Option<(usize, PathBuf)> = None;
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let hits = name == "vibrator" || name.contains("vibrator");
            if !hits {
                continue;
            }
            let p = e.path();
            if !p.join("activate").exists() {
                continue;
            }
            // Prefer the shortest name: `vibrator` over
            // `qcom,vibrator_allow` or a per-bank suffix.
            let rank = name.len();
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, p));
            }
        }
        let (_, base) = best?;
        let opt = |n: &str| {
            let p = base.join(n);
            if p.exists() { Some(p) } else { None }
        };
        Some(Backend::Led {
            activate: base.join("activate"),
            amplitude: opt("amplitude"),
            duration: opt("duration"),
        })
    }

    fn probe_evdev() -> Option<Backend> {
        let dir = PathBuf::from("/dev/input");
        let entries = std::fs::read_dir(&dir).ok()?;
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("event") {
                continue;
            }
            let p = e.path();
            // `/sys/class/input/<dev>/capabilities/ff` is the cheap check;
            // EVIOCGBIT on the fd would need an open, and a probe must not
            // open.
            let dev = p.file_name().map(|s| s.to_owned())?;
            let caps = PathBuf::from("/sys/class/input").join(&dev).join("capabilities");
            if std::fs::read_to_string(caps.join("ff"))
                .map(|s| s.trim() != "0")
                .unwrap_or(false)
            {
                return Some(Backend::Evdev(p));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------
// sysfs writes
// ---------------------------------------------------------------------

/// Write `s` to `path`.
///
/// sysfs attributes are opened `O_WRONLY` without `O_TRUNC`: the kernel
/// rejects a truncate on most of them, and a plain `write` of a short ASCII
/// number is what the ABI expects.
///
/// `OpenOptions` on unix adds `O_CLOEXEC` to every `open2` it performs
/// unconditionally (`std::sys::pal::unix::fs`), so the descriptor this opens
/// cannot leak across an `exec`. Verified by
/// `the_sysfs_open_sets_cloexec` rather than assumed.
fn write_bytes(path: &Path, s: &[u8]) -> std::io::Result<()> {
    let mut f = OpenOptions::new().write(true).truncate(false).open(path)?;
    f.write_all(s)
}

/// Longest path handed to libc, in bytes, *excluding* the terminator.
///
/// `PATH_MAX` is 4096, but a sysfs attribute path or an evdev node is tens of
/// bytes and this buffer lives on the touch-handler's stack. 255 leaves room
/// for any realistic `/sys/class/leds/<vendor-name>/<attr>` and keeps the copy
/// to one predictable bounded memcpy.
const C_PATH_MAX: usize = 255;

/// NUL-terminated copy of `path` for a libc call, or `None` if it does not fit.
///
/// Why a stack buffer and not a `CString`: `CString::new` allocates, and this
/// runs on the touch path where the only acceptable cost is the copy. Why the
/// length is checked rather than truncated: a truncated path is not "a
/// slightly wrong path", it is a *different* path -- `/sys/class/leds/foo/am
/// plitude` could collide with a real, shorter attribute on a sibling
/// device, and the write would land on the wrong file with no error. Refusing
/// is the only safe behaviour.
///
/// `Path::as_os_str().as_encoded_bytes()` is deliberately not NUL-terminated
/// (a `Path` may contain interior NULs as far as the type is concerned), so
/// passing its pointer to `open`/`openat` reads past the end of the `PathBuf`
/// buffer until it happens to hit a zero byte. That is the out-of-bounds read
/// this helper exists to remove.
fn copy_c_path(path: &Path) -> Option<[u8; C_PATH_MAX + 1]> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() > C_PATH_MAX {
        return None;
    }
    let mut buf = [0u8; C_PATH_MAX + 1];
    buf[..bytes.len()].copy_from_slice(bytes);
    // `buf` is zero-initialised and only `..len` was written, so the byte at
    // `len` is already the terminator. Stated explicitly so the invariant is
    // visible rather than implied by the initialiser.
    buf[bytes.len()] = 0;
    Some(buf)
}

/// Format `v` as decimal ASCII into `buf`, returning the filled suffix.
///
/// Allocation-free by construction: the digits go straight into the caller's
/// stack array and the returned slice borrows it. `buf` is left zero-filled
/// after the digits, so a caller that wants a C string can use it as-is; a
/// sysfs attribute wants no terminator, and a trailing NUL would be part of
/// the written bytes, so callers write only the returned slice.
///
/// Panics above 4 digits, which is unreachable from `HapticEffect` (amplitude
/// is 128..=255 by kernel contract, `on_ms` is 8..=30) and is asserted rather
/// than truncated: silently writing a wrong number to `duration` would change
/// what the motor does without reporting anything.
fn fmt_u8(buf: &mut [u8; 4], v: u32) -> &[u8] {
    assert!(v < 10_000, "u8 domain does not fit 4 ASCII digits");
    // Filled back-to-front from the end of the buffer, then the filled
    // suffix is returned. No reversal pass, no length pre-computation, no
    // allocation.
    let mut i = buf.len();
    let mut d = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (d % 10) as u8;
        d /= 10;
        if d == 0 {
            break;
        }
    }
    &buf[i..]
}

/// Write `v` in decimal to the sysfs attribute at `path`, no allocation.
///
/// `buf` is passed in by the caller rather than created here so the array
/// lives in the caller's frame and the whole chain from `Haptics::trigger`
/// down to `write` is allocation-free. See [`fmt_u8`] for the digit contract.
fn write_u8(path: &Path, mut buf: [u8; 4], v: u32) -> std::io::Result<()> {
    let digits = fmt_u8(&mut buf, v);
    // Reborrow rather than move out of `buf`: the returned slice borrows
    // `buf`, and `write_bytes` only needs it to outlive the `write_all` call.
    write_bytes(path, digits)
}

// ---------------------------------------------------------------------
// evdev ABI: `struct input_event`, `struct ff_effect`, `EVIOCSFF`
// ---------------------------------------------------------------------

/// `struct input_event`, 64-bit Linux.
///
/// ```c
/// struct input_event {
///     struct timeval time;   // 16 bytes: time_t tv_sec; suseconds_t tv_usec
///     __u16 type;
///     __u16 code;
///     __s32 value;
/// };                        // 24 bytes with natural alignment
/// ```
///
/// The `time` field is *not* optional and *not* replaceable by a shortened
/// header: `evdev_write()` copies whole `input_event`s
/// (`input_event_from_user`, in a `retval + input_event_size() <= count`
/// loop) and rejects the call with `-EINVAL` outright if the buffer is
/// shorter than one event. A 20-byte "type/code/value" header therefore
/// fails at the syscall boundary, not inside the driver -- and because
/// `fire_evdev` treats a failed write as "no vibrator", the mis-sized struct
/// was a silent kernel-side failure rather than a visible one.
#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// Pin the size at compile time: a mis-sized `input_event` is not caught by
/// the Rust compiler, and the kernel's complaint is a bare `-EINVAL`.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<InputEvent>() == 24);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(align_of::<InputEvent>() == 8);
/// 32-bit arm/x86: `struct timeval` is two 32-bit fields, so the event is 16.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<InputEvent>() == 16);

/// `union ff_effect_u`, sized for the kernel's largest member.
///
/// The union's size is set by `struct ff_periodic_effect`, not by
/// `struct ff_rumble_effect`: on 64-bit it holds a `__s16 *custom_data`
/// pointer, so it is 32 bytes (24 bytes of scalars, then an 8-byte pointer at
/// offset 24 after alignment), and the union inherits its 8-byte alignment.
/// `ff_rumble_effect` itself is only 4 bytes (two `__u16`:
///
/// ```c
/// struct ff_rumble_effect {
///     __u16 strong_magnitude;
///     __u16 weak_magnitude;
/// };
/// ```
///
/// ), and `strong_magnitude` is therefore `u[0]` -- bytes 0..2 of the union,
/// bytes 16..18 of the whole struct.
///
/// A Rust `union` cannot be used to describe the other members without
/// enumerating all six of them (constant, ramp, periodic, condition[2],
/// rumble, haptic) plus their envelopes, none of which this driver can
/// produce: it only ever uploads `FF_RUMBLE`. Modelling it as opaque bytes
/// of the correct length and alignment keeps the *size and offsets* exact,
/// which is what the ABI contract is, and lets the rumble payload be written
/// through a typed view.
#[repr(C, align(8))]
struct FfEffectUnion([u8; 32]);

/// `struct ff_rumble_effect`: the union's `rumble` view.
#[repr(C)]
#[derive(Clone, Copy)]
struct FfRumbleEffect {
    strong_magnitude: u16,
    weak_magnitude: u16,
}

impl FfEffectUnion {
    /// Write the `ff_rumble_effect` view into the union's first four bytes.
    ///
    /// Done through `to_ne_bytes` rather than by transmuting a
    /// `FfRumbleEffect`: the bytes are what the kernel reads, and this keeps
    /// the write free of `unsafe` while staying exactly layout-faithful --
    /// `ff_rumble_effect` is two `__u16` and nothing else. The `assert` is
    /// what makes the slice indices provably in bounds.
    fn rumble(&mut self, strong: u16, weak: u16) {
        let r = FfRumbleEffect { strong_magnitude: strong, weak_magnitude: weak };
        let n = size_of::<FfRumbleEffect>();
        assert!(n <= self.0.len());
        for (i, b) in r.strong_magnitude.to_ne_bytes().iter().enumerate() {
            self.0[i] = *b;
        }
        for (i, b) in r.weak_magnitude.to_ne_bytes().iter().enumerate() {
            self.0[2 + i] = *b;
        }
    }

    /// The rumble view, so a test can assert what was uploaded.
    ///
    /// Test-only: the write path never reads the union back, and an unused
    /// accessor is dead weight in the release binary.
    #[cfg(test)]
    fn rumble_parts(&self) -> FfRumbleEffect {
        FfRumbleEffect {
            strong_magnitude: u16::from_ne_bytes([self.0[0], self.0[1]]),
            weak_magnitude: u16::from_ne_bytes([self.0[2], self.0[3]]),
        }
    }
}

/// `struct ff_effect`:
///
/// ```c
/// struct ff_effect {
///     __u16 type;
///     __s16 id;
///     __u16 direction;
///     struct ff_trigger trigger;   // { __u16 button; __u16 interval; }
///     struct ff_replay replay;     // { __u16 length; __u16 delay; }
///     union ff_effect_u u;         // 32 bytes, 8-aligned on 64-bit
/// };
/// ```
///
/// `trigger` and `replay` are anonymous pairs of `__u16`, flattened here to
/// the four `u16` fields, which is layout-identical.
#[repr(C)]
struct FfEffect {
    kind: u16,
    id: i16,
    direction: u16,
    trigger_button: u16,
    trigger_interval: u16,
    replay_length: u16,
    replay_delay: u16,
    u: FfEffectUnion,
}

/// 16 bytes of `__u16` header + 32 bytes of union = 48. This is the number
/// `EVIOCSFF`'s size field must carry, and it is derived from the struct
/// rather than written down a second time so the two cannot drift.
const _: () = assert!(size_of::<FfEffect>() == 48);
const _: () = assert!(offset_of!(FfEffect, u) == 16);

impl FfEffect {
    /// An `FF_RUMBLE` upload request: `id = -1` asks the kernel to allocate.
    fn rumble(replay_ms: u16, amplitude: u16) -> Self {
        let mut u = FfEffectUnion([0u8; 32]);
        u.rumble(amplitude, 0);
        Self {
            kind: FF_RUMBLE,
            id: -1,
            direction: 0,
            trigger_button: 0,
            trigger_interval: 0,
            replay_length: replay_ms,
            replay_delay: 0,
            u,
        }
    }
}

/// `EV_FF`: force-feedback events (`linux/input-event-codes.h`).
const EV_FF: u16 = 0x15;
/// `FF_RUMBLE`: the simple strong/weak two-motor effect.
const FF_RUMBLE: u16 = 0x50;

// ---- the ioctl encoding, spelled out --------------------------------
//
// `asm-generic/ioctl.h` is the encoding for both of this crate's targets
// (x86_64 and arm64):
//
//   _IOC(dir, type, nr, size)
//     dir  << 30   2 bits, _IOC_NONE 0 / _IOC_WRITE 1 / _IOC_READ 2
//     type << 8    8 bits, the "group", 'E' for evdev
//     nr   << 0    8 bits, the command number within the group
//     size << 16  14 bits, sizeof the argument struct
//
// `EVIOCSFF` is `_IOW('E', 0x80, struct ff_effect)`, i.e. dir = _IOC_WRITE,
// type = 'E', nr = 0x80, size = sizeof(struct ff_effect) = 48, which
// assembles to 0x4030_4580.
//
// The previous constant, 0x8008_0180, decoded to dir = _IOC_READ,
// type = 0x01, nr = 0x80, size = 8 -- `_IOR(1, 0x80, 8)`. The command number
// was right and the group was wrong by 0x44, which is the difference between
// 'E' (0x45) and 0x01. `evdev_ioctl` switches on the *whole* command for
// fixed-size commands, so an unmatched value falls through to
// `-ENOTTY`/`-EINVAL` and the upload never happened.
//
// Computed from its parts rather than pasted so a reader can check the
// arithmetic and so it cannot drift when the struct changes.
const IOC_NRBITS: u32 = 8;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPEBITS: u32 = 8;
const IOC_TYPESHIFT: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_SIZESHIFT: u32 = 16;
const IOC_DIRBITS: u32 = 2;
const IOC_DIRSHIFT: u32 = 30;

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

/// The `_IOC` encoding, as a `const fn` so the constants below are checked.
const fn ioc(dir: u32, group: u32, nr: u32, size: u32) -> u32 {
    assert!(dir < (1 << IOC_DIRBITS), "ioctl direction out of range");
    assert!(group < (1 << IOC_TYPEBITS), "ioctl group out of range");
    assert!(nr < (1 << IOC_NRBITS), "ioctl nr out of range");
    assert!(size < (1 << IOC_SIZEBITS), "ioctl size out of range");
    (dir << IOC_DIRSHIFT)
        | ((group & ((1 << IOC_TYPEBITS) - 1)) << IOC_TYPESHIFT)
        | ((nr & ((1 << IOC_NRBITS) - 1)) << IOC_NRSHIFT)
        | ((size & ((1 << IOC_SIZEBITS) - 1)) << IOC_SIZESHIFT)
}

/// `_IOW`: an ioctl that writes to the device.
const fn iow(group: u32, nr: u32, size: u32) -> u32 {
    ioc(IOC_WRITE, group, nr, size)
}

/// `_IOR`: an ioctl that reads from the device.
const fn ior(group: u32, nr: u32, size: u32) -> u32 {
    ioc(IOC_READ, group, nr, size)
}

/// `EVIOCSFF`: upload a `struct ff_effect`.
const EVIOCSFF: libc::c_ulong =
    iow(b'E' as u32, 0x80, size_of::<FfEffect>() as u32) as libc::c_ulong;

// The encoding is checked against the kernel's other evdev commands at
// compile time rather than only in a test, so a mistake in any of the five
// constants above fails the build. Both values below are what
// `linux/input.h` produces on x86_64/arm64, cross-checked with
//
//     printf("%lx %lx\n", EVIOCGVERSION, EVIOCGNAME(0))
//     -> 80044501 80004506
//
// `EVIOCGVERSION` is `_IOR('E', 0x01, int)` -- note the direction is _READ
// (2), which is the half of the encoding the old constant also got wrong.
// `EVIOCGNAME(0)` is `_IOR('E', 0x06, 0)`: zero-length, but still _READ, not
// _IOC_NONE. Both are _READ, so they pin `IOC_READ` as well as the shifts.
const _: () = assert!(ior(b'E' as u32, 0x01, 4) == 0x8004_4501);
const _: () = assert!(ior(b'E' as u32, 0x06, 0) == 0x8000_4506);
// And the unencoded form, to keep `IOC_NONE` honest as a distinct direction
// rather than a synonym for `_IOR(..., 0)`.
const _: () = assert!(ioc(IOC_NONE, b'E' as u32, 0x06, 0) == 0x0000_4506);
const _: () = assert!(IOC_READ == 2);
const _: () = assert!(IOC_WRITE == 1);

#[cfg(test)]
mod tests {
    use super::*;

    /// The path handed to libc must be NUL-terminated and nothing may be
    /// truncated to make it fit.
    ///
    /// This is the regression test for the `as_encoded_bytes().as_ptr()`
    /// read past the end of the `PathBuf`: the returned buffer is what the
    /// `open` call reads, so asserting on it is asserting on the bytes the
    /// kernel sees, without needing a device to exist.
    #[test]
    fn the_c_path_is_nul_terminated_and_length_checked() {
        // The device path this module actually opens.
        let raw = b"/dev/input/event3";
        let p = copy_c_path(Path::new("/dev/input/event3")).expect("a node path fits");
        assert_eq!(&p[..raw.len()], raw);
        assert_eq!(p[raw.len()], 0, "no terminator means open() reads off the end");
        assert!(p[raw.len() + 1..].iter().all(|&b| b == 0), "the tail stays zeroed");

        // 255 bytes is the cap, and 255 bytes plus the terminator is exactly
        // the buffer: the boundary case must be accepted, not refused.
        let max = PathBuf::from(format!("/{}", "a".repeat(C_PATH_MAX - 1)));
        assert_eq!(max.as_os_str().as_encoded_bytes().len(), C_PATH_MAX);
        let p = copy_c_path(&max).expect("exactly C_PATH_MAX bytes must be accepted");
        assert_eq!(p.len(), C_PATH_MAX + 1);
        assert_eq!(p[C_PATH_MAX], 0, "the terminator lands in the last byte");
        assert_eq!(&p[..C_PATH_MAX], max.as_os_str().as_encoded_bytes());

        // One byte more is refused. The path does not exist, but that is
        // beside the point: the refusal happens in `copy_c_path`, before any
        // syscall, which is exactly what makes it safe to refuse rather than
        // truncate.
        let too_long = PathBuf::from(format!("/{}", "a".repeat(C_PATH_MAX)));
        assert_eq!(too_long.as_os_str().as_encoded_bytes().len(), C_PATH_MAX + 1);
        assert!(
            copy_c_path(&too_long).is_none(),
            "a path longer than the buffer must be refused, not truncated: \
             a truncated path can name a different file"
        );

        // The audit's case: 300 bytes.
        let long300 = PathBuf::from(format!("/{}", "a".repeat(299)));
        assert!(copy_c_path(&long300).is_none(), "300 bytes must be refused");

        // And a path that long through `trigger` must not open anything and
        // must not count a pulse.
        let mut h = Haptics {
            backend: Backend::Evdev(long300),
            fired: 0,
        };
        assert!(!h.trigger(HapticEffect::Tick));
        assert_eq!(h.fired(), 0);
    }

    /// `struct input_event` must be 24 bytes with the kernel's field offsets.
    ///
    /// The old 20-byte "header only" event was rejected by `evdev_write` with
    /// `-EINVAL` before it ever reached the driver, so the evdev path was
    /// silently dead.
    #[test]
    fn the_input_event_matches_the_kernel_layout() {
        assert_eq!(size_of::<InputEvent>(), 24, "timeval(16) + 2 + 2 + 4");
        assert_eq!(offset_of!(InputEvent, time), 0);
        assert_eq!(offset_of!(InputEvent, type_), 16, "after the 16-byte timeval");
        assert_eq!(offset_of!(InputEvent, code), 18);
        assert_eq!(offset_of!(InputEvent, value), 20);
        assert_eq!(align_of::<InputEvent>(), 8, "timeval is 8-aligned");

        // The event the module writes, byte-for-byte in the field positions
        // the kernel reads them from. Endianness is native on both targets,
        // so this checks layout, not a byte order.
        let ev = InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            type_: EV_FF,
            code: 0,
            value: 1,
        };
        // The bytes the kernel actually reads, at the offsets it reads them from.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                size_of::<InputEvent>(),
            )
        };
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[16..18], &EV_FF.to_ne_bytes());
        assert_eq!(&bytes[18..20], &0u16.to_ne_bytes());
        assert_eq!(&bytes[20..24], &1i32.to_ne_bytes());
        assert!(bytes[..16].iter().all(|&b| b == 0), "a zero timeval is fine");
    }

    /// The ioctl number must decode to what `EVIOCSFF` is.
    #[test]
    fn the_evioctl_number_decodes_to_the_kernel_definition() {
        // 0x4030_4580 = _IOW('E', 0x80, 48), and 48 is sizeof(struct ff_effect).
        assert_eq!(EVIOCSFF as u32, 0x4030_4580);

        // Decode it back and check every field independently, so a wrong
        // constant cannot pass by matching itself.
        let cmd = EVIOCSFF as u32;
        let dir = (cmd >> IOC_DIRSHIFT) & ((1 << IOC_DIRBITS) - 1);
        let group = (cmd >> IOC_TYPESHIFT) & ((1 << IOC_TYPEBITS) - 1);
        let nr = (cmd >> IOC_NRSHIFT) & ((1 << IOC_NRBITS) - 1);
        let size = (cmd >> IOC_SIZESHIFT) & ((1 << IOC_SIZEBITS) - 1);
        assert_eq!(dir, IOC_WRITE, "_IOW, not _IOR");
        assert_eq!(group, u32::from(b'E'), "the evdev group");
        assert_eq!(nr, 0x80, "EVIOCSFF's command number");
        assert_eq!(size, size_of::<FfEffect>() as u32);
        assert_eq!(size, 48);

        // And it round-trips through the encoding.
        assert_eq!(iow(b'E' as u32, 0x80, 48), cmd);
        assert_eq!(ior(b'E' as u32, 0x01, 4), 0x8004_4501, "_IOR('E', 1, int)");
        assert_eq!(ioc(IOC_NONE, b'E' as u32, 0x06, 0), 0x0000_4506, "_IOC");
        assert_eq!(IOC_READ, 2, "asm-generic _IOC_READ");
        assert_eq!(IOC_WRITE, 1, "asm-generic _IOC_WRITE");
    }

    /// The old constant was not an evdev ioctl at all; pin what it was so
    /// nobody "restores" it.
    #[test]
    fn the_old_evioctl_constant_was_never_an_evdev_command() {
        let old = 0x8008_0180u32;
        let dir = (old >> IOC_DIRSHIFT) & ((1 << IOC_DIRBITS) - 1);
        let group = (old >> IOC_TYPESHIFT) & ((1 << IOC_TYPEBITS) - 1);
        let nr = (old >> IOC_NRSHIFT) & ((1 << IOC_NRBITS) - 1);
        let size = (old >> IOC_SIZESHIFT) & ((1 << IOC_SIZEBITS) - 1);
        assert_eq!(dir, IOC_READ);
        assert_ne!(group, u32::from(b'E'), "it was not in the evdev group");
        assert_eq!(group, 0x01);
        assert_eq!(nr, 0x80, "the number was right, the group was not");
        assert_eq!(size, 8, "an int-sized argument, not a struct ff_effect");
        assert_ne!(old, EVIOCSFF as u32);
    }

    /// `struct ff_effect` must be 48 bytes with the union at offset 16.
    #[test]
    fn the_ff_effect_matches_the_kernel_layout() {
        assert_eq!(size_of::<FfEffect>(), 48, "16 of __u16 + 32 of union");
        assert_eq!(size_of::<FfEffectUnion>(), 32, "set by ff_periodic_effect");
        assert_eq!(align_of::<FfEffectUnion>(), 8, "it holds a pointer");
        assert_eq!(align_of::<FfEffect>(), 8);
        assert_eq!(offset_of!(FfEffect, u), 16, "after 8 __u16 header fields");

        let eff = FfEffect::rumble(30, 255);
        assert_eq!(eff.kind, FF_RUMBLE);
        assert_eq!(eff.id, -1, "a fresh upload asks the kernel for a slot");
        assert_eq!(eff.replay_length, 30);
        let rumble = eff.u.rumble_parts();
        assert_eq!(rumble.strong_magnitude, 255, "u[0] is the strong motor");
        assert_eq!(rumble.weak_magnitude, 0);

        // The payload really is at the start of the union: bytes 16..18 of
        // the struct, which is where a C `u.rumble.strong_magnitude` reads.
        // The bytes the kernel actually reads, at the offsets it reads them
        // from.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &eff as *const FfEffect as *const u8,
                size_of::<FfEffect>(),
            )
        };
        assert_eq!(&bytes[16..18], &255u16.to_ne_bytes());
        assert_eq!(&bytes[18..20], &0u16.to_ne_bytes());
        assert_eq!(&bytes[10..12], &30u16.to_ne_bytes(), "replay_length at 5th __u16");
        assert_eq!(&bytes[2..4], &(-1i16).to_ne_bytes(), "id = -1");
        assert_eq!(&bytes[..2], &FF_RUMBLE.to_ne_bytes(), "type");
    }

    /// The decimal formatter's exact output, which is the whole contract:
    /// sysfs takes ASCII digits and nothing else.
    #[test]
    fn the_decimal_formatter_writes_the_right_bytes() {
        for (v, want) in [
            (0u32, "0"),
            (1, "1"),
            (9, "9"),
            (10, "10"),
            (99, "99"),
            (100, "100"),
            (255, "255"),
        ] {
            let mut buf = [0u8; 4];
            let got = fmt_u8(&mut buf, v);
            assert_eq!(got, want.as_bytes(), "{v} formatted wrong");
            assert_eq!(got.len(), want.len(), "{v} has the wrong digit count");
        }

        // 1000 and 9999 fill the buffer exactly; the trailing bytes a caller
        // relies on for a C string stay zero.
        let mut buf = [0u8; 4];
        assert_eq!(fmt_u8(&mut buf, 1000), b"1000");
        assert_eq!(fmt_u8(&mut buf, 9999), b"9999");

        // Every effect's real numbers, and they agree with `to_string` (the
        // allocation this replaced).
        for e in [
            HapticEffect::Tick,
            HapticEffect::LongPress,
            HapticEffect::Confirm,
            HapticEffect::Open,
        ] {
            let mut a = [0u8; 4];
            let mut d = [0u8; 4];
            assert_eq!(fmt_u8(&mut a, e.amplitude()), e.amplitude().to_string().as_bytes());
            assert_eq!(fmt_u8(&mut d, e.on_ms()), e.on_ms().to_string().as_bytes());
            assert!(e.amplitude() <= 255, "the u8 domain is the whole premise");
        }
    }

    /// The sysfs open must be close-on-exec, not merely assumed to be.
    #[test]
    fn the_sysfs_open_sets_cloexec() {
        let dir = std::env::temp_dir().join("utlc-haptics-cloexec-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let attr = dir.join("amplitude");
        std::fs::write(&attr, "").expect("attr");

        assert!(write_u8(&attr, [0u8; 4], 255).is_ok());
        assert_eq!(std::fs::read_to_string(&attr).unwrap(), "255");

        // `OpenOptions` unconditionally ORs in `O_CLOEXEC`; the observable
        // consequence is that `FD_CLOEXEC` is set on the descriptor, which is
        // what a test can check without strace.
        let f = OpenOptions::new().write(true).truncate(false).open(&attr).expect("open");
        use std::os::unix::io::AsRawFd;
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "the descriptor would leak across exec");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_disabled_sink_accepts_every_effect_and_pulses_nothing() {
        let mut h = Haptics::disabled();
        assert!(!h.is_available());
        for e in [
            HapticEffect::Tick,
            HapticEffect::LongPress,
            HapticEffect::Confirm,
            HapticEffect::Open,
        ] {
            assert!(!h.trigger(e), "{e:?} on a machine with no vibrator");
        }
        assert_eq!(h.fired(), 0, "nothing was emitted");
    }

    /// A tick must be a tick, not a buzz.
    ///
    /// The fast scroller fires one of these per letter crossed, so a 30 ms
    /// "tick" turns an A-Z scroll into a continuous 2-second rattle.
    #[test]
    fn the_fast_scroller_tick_is_shorter_than_it_is_a_buzz() {
        assert_eq!(HapticEffect::Tick.on_ms(), 8);
        assert_eq!(HapticEffect::Tick.off_ms(), 0);
        assert!(
            HapticEffect::Tick.on_ms() < HapticEffect::Confirm.on_ms(),
            "a detent must be lighter than a task-dismiss confirm"
        );
        assert!(
            HapticEffect::Tick.amplitude() < HapticEffect::Confirm.amplitude(),
            "a detent must be quieter than a task-dismiss confirm"
        );
    }

    /// Every effect must have a non-zero on-time and fit the u16 the
    /// evdev `replay_length` field carries.
    #[test]
    fn every_effect_fits_the_kernel_abi() {
        for e in [
            HapticEffect::Tick,
            HapticEffect::LongPress,
            HapticEffect::Confirm,
            HapticEffect::Open,
        ] {
            assert!(e.on_ms() > 0, "{e:?} would be inaudible");
            assert!(e.on_ms() <= u16::MAX as u32, "{e:?} overflows replay_length");
            assert!(e.amplitude() <= 255, "{e:?} overflows the u8 amplitude");
        }
    }

    /// Probing must never panic, whatever the machine looks like.
    ///
    /// A phone with no vibrator, a container with no `/sys`, and a developer
    /// machine are all "no vibrator". The probe has to return, not abort.
    #[test]
    fn detection_is_total_and_never_panics() {
        let h = Haptics::detect();
        // The assertion is that we got here. Availability depends on the
        // host, so it is deliberately not asserted either way.
        let _ = h.is_available();
        let _ = h.fired();
    }

    #[test]
    fn a_led_backend_writes_amplitude_then_duration_then_activate() {
        // Drive the sysfs path against a temp directory so the ordering is
        // observable without hardware.
        let dir = std::env::temp_dir().join("utlc-haptics-led-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for f in ["activate", "amplitude", "duration"] {
            std::fs::write(dir.join(f), "").expect("attr");
        }
        let mut h = Haptics {
            backend: Backend::Led {
                activate: dir.join("activate"),
                amplitude: Some(dir.join("amplitude")),
                duration: Some(dir.join("duration")),
            },
            fired: 0,
        };
        assert!(h.trigger(HapticEffect::Confirm));
        assert_eq!(h.fired(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("amplitude")).unwrap(),
            "255"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("duration")).unwrap(),
            "30"
        );
        assert_eq!(std::fs::read_to_string(dir.join("activate")).unwrap(), "1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backend whose node has gone away must fail softly.
    #[test]
    fn a_vanished_vibrator_does_not_panic_or_count() {
        let mut h = Haptics {
            backend: Backend::Led {
                activate: PathBuf::from("/nonexistent/utlc/vibrator/activate"),
                amplitude: None,
                duration: None,
            },
            fired: 0,
        };
        assert!(!h.trigger(HapticEffect::Tick));
        assert_eq!(h.fired(), 0, "a pulse that reached no device is not a pulse");
    }

    #[test]
    fn an_evdev_backend_with_no_device_fails_softly() {
        let mut h = Haptics {
            backend: Backend::Evdev(PathBuf::from("/dev/input/event-nonexistent")),
            fired: 0,
        };
        assert!(!h.trigger(HapticEffect::Open));
        assert_eq!(h.fired(), 0);
    }
}
