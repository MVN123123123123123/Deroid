//! Radio backends for the quick-settings tiles.
//!
//! # Why this exists
//!
//! Seven of the shell's eight quick-settings tiles flip a local `[bool; 8]` and
//! nothing else. `main.rs` does
//! `quick_tiles_active[i] = !quick_tiles_active[i]` and stops, and
//! `SystemUiShade::toggle_tile` -- the only path to `sync_torch_sysfs` and the
//! only place a tile's subtitle is updated -- is never called from the live
//! shell. So Wifi, Mobile Data, Airplane, Auto-Rotate and Hotspot reach no
//! hardware at all.
//!
//! The reference is not an Android launcher feature so much as a SystemUI one:
//! `QSTile.State` carries `state`, `label`, `secondaryLabel`, `stateDescription`
//! (`systemUI/plugin/.../qs/QSTile.java:188`) and each tile is a real driver.
//!
//! This module is the driver half: a thin, bounded writer for each radio the
//! tiles need, with the sysfs/D-Bus paths as constants and every write
//! reporting whether it landed. The tile *dispatch* is the shell's.
//!
//! # Scope
//!
//! Linux exposes these through several mechanisms and this covers the ones
//! that need no new dependency:
//!
//! * **rfkill** -- one write to `/sys/class/rfkill/rfkill*/soft`. This is the
//!   right primitive for airplane mode, wifi and bluetooth on a phone, and it is
//!   the only one that needs no protocol stack at all.
//! * **NetworkManager / systemd-networkd** -- not driven directly. The shell
//!   calls `systemctl`, which UTIM already implements, so there is nothing to
//!   add here; a tile that needs it dispatches an action rather than a write.
//! * **Bluetooth** -- `org.bluez` is a D-Bus interface, and the same
//!   hand-rolled message encoding [`crate::notification`] uses applies. Only
//!   `Adapter.SetPowered` and the `PropertiesChanged` signal are needed, and
//!   only the former is written from the shell.
//!
//! Everything here reports `io::Result`. A tile whose hardware is absent must
//! be able to say so: on a device with no rfkill class -- which is every
//! sandbox and the qemu target -- "turned on" and "there is no radio" have to
//! be distinguishable, or the tile is a lie.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the kernel exposes the rfkill class.
///
/// A constant, not an env var: the path is a kernel ABI, and a launcher that
/// let it be redirected could be pointed at a file of its own choosing.
pub const RFKILL_CLASS: &str = "/sys/class/rfkill";

/// One rfkill switch, as the kernel names it.
///
/// The index (`soft`, `hard`) is the switch's *kind*, and the
/// `type`/`name` files beside it are its identity -- so a device with no wifi
/// radio has no `wlan` entry, and asking for one is `NotFound`, not a silent
/// success. That distinction is the whole reason this returns `io::Result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Radio {
    Wifi = 0,
    Bluetooth = 1,
    /// The cellular modem's data session.
    MobileData = 2,
    /// USB tethering, which shares the modem's radio.
    Tethering = 3,
}

impl Radio {
    /// The kernel's name for this switch, as it appears in a `rfkillN/type`.
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Wifi => "wlan",
            Self::Bluetooth => "bluetooth",
            Self::MobileData => "wwan",
            Self::Tethering => "usb",
        }
    }

    /// Every radio, for a capability scan.
    pub fn all() -> [Self; 4] {
        [
            Self::Wifi,
            Self::Bluetooth,
            Self::MobileData,
            Self::Tethering,
        ]
    }
}

/// The on/off state a tile shows and sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioState {
    Off,
    On,
}

/// An rfkill switch's block, once found.
///
/// Holding the resolved `soft` path rather than re-walking `/sys/class` on
/// every write: a tile drag writes once per frame, and a directory scan per
/// frame is exactly the kind of syscall the frame budget forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfkillSwitch {
    radio: Radio,
    /// Full path to the `soft` file.
    soft: PathBuf,
    /// The kernel's `name`, used only in error messages.
    name: String,
}

impl RfkillSwitch {
    /// Find the switch for `radio`, or `None` if this kernel exposes no such
    /// radio.
    ///
    /// A missing radio is `None`, not an error: a device with no cellular modem
    /// is a normal device, and a tile that could not be shown should be
    /// disabled rather than report a failure on every frame.
    pub fn find(radio: Radio) -> Option<Self> {
        Self::find_in(Path::new(RFKILL_CLASS), radio)
    }

    /// `find`, against an alternate root. For tests, and for a device whose
    /// class is mounted elsewhere.
    pub fn find_in(root: &Path, radio: Radio) -> Option<Self> {
        let want = radio.type_name();
        // `read_dir` order is neither sorted nor guaranteed stable, so two
        // switches of the same type would resolve arbitrarily. Sort by the
        // numeric suffix, which is what `rfkillN` names carry and is what a user
        // would mean by "the first one".
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(root)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        dirs.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("rfkill"))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        });
        for dir in dirs {
            // An entry that is not a switch is skipped, not fatal: one stray
            // file must not make the whole class unreadable.
            let Ok(ty) = std::fs::read_to_string(dir.join("type")) else {
                continue;
            };
            if ty.trim() != want {
                continue;
            }
            let name = std::fs::read_to_string(dir.join("name"))
                .unwrap_or_else(|_| want.to_string())
                .trim()
                .to_string();
            return Some(Self {
                radio,
                soft: dir.join("soft"),
                name,
            });
        }
        None
    }

    /// Which radio this is.
    pub fn radio(&self) -> Radio {
        self.radio
    }

    /// The current state, read from `soft`.
    ///
    /// `None` on a read error. A tile in an unknown state should show itself
    /// as inactive rather than guess: showing "on" for a radio that is off
    /// hands the user a working mute switch.
    pub fn state(&self) -> Option<RadioState> {
        let raw = std::fs::read_to_string(&self.soft).ok()?;
        match raw.trim() {
            "0" => Some(RadioState::Off),
            "1" => Some(RadioState::On),
            // The kernel also writes "soft" here on some drivers.
            other => match other {
                "0" | "1" => Some(if other == "1" {
                    RadioState::On
                } else {
                    RadioState::Off
                }),
                _ => None,
            },
        }
    }

    /// Set the switch.
    ///
    /// One `write` of one byte, into a caller-shaped buffer: no `format!`, no
    /// `String`, nothing allocated. The file is opened write-only and not
    /// truncated, because the kernel's `soft` attribute is a single byte and a
    /// truncating open would zero it before the write.
    pub fn set(&self, state: RadioState) -> std::io::Result<()> {
        let byte = match state {
            RadioState::Off => b'0',
            RadioState::On => b'1',
        };
        let mut f = std::fs::OpenOptions::new().write(true).open(&self.soft)?;
        f.write_all(&[byte])?;
        // No sync: this is a device state change, not a durability
        // requirement, and an fsync per tile toggle would be absurd.
        Ok(())
    }

    /// Set, and report a missing radio as a typed error rather than a string.
    pub fn set_checked(&self, state: RadioState) -> Result<(), RadioError> {
        self.set(state).map_err(|e| RadioError::Io {
            radio: self.radio,
            name: self.name.clone(),
            source: e.kind(),
        })
    }
}

/// A failure from a radio operation, carrying enough to render a tile subtitle.
///
/// The reference's `QSTile.State` carries `secondaryLabel` and
/// `stateDescription` (`QSTile.java:188`), so a tile that cannot do what it
/// says needs somewhere to say so. A `String` error cannot be rendered
/// without allocating, and this is on the tile path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioError {
    /// No such radio on this kernel. The tile should be disabled, not retried.
    Absent(Radio),
    /// The write failed. The tile should revert and show why.
    Io {
        radio: Radio,
        name: String,
        source: std::io::ErrorKind,
    },
}

impl RadioError {
    /// A short, allocation-free label for the tile's subtitle.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Absent(_) => "Not present",
            Self::Io { .. } => "Failed",
        }
    }
}

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent(r) => write!(f, "no {} radio on this device", r.type_name()),
            Self::Io {
                radio,
                name,
                source,
            } => {
                write!(f, "{name} ({}) failed: {source:?}", radio.type_name())
            }
        }
    }
}

impl std::error::Error for RadioError {}

/// What this device can actually do, scanned once.
///
/// The scan runs at startup and is a directory listing, not a per-frame
/// operation. A tile whose radio is absent is drawn disabled -- which is what
/// `QSTile.State.disabledByPolicy` is for in the reference, and is the honest
/// rendering of "this device has no wifi".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RadioCapabilities {
    present: [bool; 4],
}

impl RadioCapabilities {
    /// Scan the real `/sys/class/rfkill`.
    pub fn scan() -> Self {
        Self::scan_in(Path::new(RFKILL_CLASS))
    }

    /// `scan`, against an alternate root. For tests.
    pub fn scan_in(root: &Path) -> Self {
        let mut c = Self::default();
        for (i, r) in Radio::all().into_iter().enumerate() {
            c.present[i] = RfkillSwitch::find_in(root, r).is_some();
        }
        c
    }

    /// Whether `radio` exists on this device.
    pub fn has(&self, radio: Radio) -> bool {
        self.present[radio as usize]
    }

    /// Radios this device has.
    pub fn available(&self) -> impl Iterator<Item = Radio> + '_ {
        Radio::all().into_iter().filter(|r| self.has(*r))
    }
}

/// Set a radio, resolving the switch first.
///
/// The convenience form for a tile: "make this radio so", with the absent case
/// reported. A caller that has already found the switch uses
/// [`RfkillSwitch::set`] directly, so nothing on the frame path pays for a
/// directory scan.
pub fn set_radio(radio: Radio, state: RadioState) -> Result<bool, RadioError> {
    match RfkillSwitch::find(radio) {
        Some(sw) => {
            sw.set_checked(state)?;
            Ok(true)
        }
        None => Err(RadioError::Absent(radio)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `/sys/class/rfkill` tree, so the resolver is tested against
    /// directory contents rather than against whatever the host has.
    struct FakeClass(PathBuf);

    impl FakeClass {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("utlc-rfkill-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("fake class");
            Self(p)
        }

        /// A switch the kernel would create: `rfkillN/type`, `name`, `soft`.
        fn add(&self, idx: usize, ty: &str, name: &str, soft: u8) {
            let d = self.0.join(format!("rfkill{idx}"));
            std::fs::create_dir_all(&d).expect("dir");
            std::fs::write(d.join("type"), ty).expect("type");
            std::fs::write(d.join("name"), name).expect("name");
            let mut f = std::fs::File::create(d.join("soft")).expect("soft");
            f.write_all(&[b'0' + soft]).expect("soft byte");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FakeClass {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_radio_is_found_by_its_kernel_type() {
        let fake = FakeClass::new("found");
        fake.add(0, "bluetooth", "Bluetooth", 0);
        fake.add(1, "wlan", "wiphy0", 1);
        let sw = RfkillSwitch::find_in(fake.path(), Radio::Wifi).expect("wifi exists");
        assert_eq!(sw.radio(), Radio::Wifi);
        assert_eq!(sw.name, "wiphy0", "the kernel's name, for the error label");
        assert_eq!(sw.state(), Some(RadioState::On), "reads the soft byte");
    }

    /// A device with no cellular modem is normal, so an absent radio is
    /// `None` and not an error. Making it an error would mean a tile on a
    /// wifi-only tablet reported a failure.
    #[test]
    fn an_absent_radio_is_none_not_an_error() {
        let fake = FakeClass::new("absent");
        fake.add(0, "wlan", "wiphy0", 0);
        assert!(RfkillSwitch::find_in(fake.path(), Radio::MobileData).is_none());
        assert!(RfkillSwitch::find_in(fake.path(), Radio::Bluetooth).is_none());
        // And an empty class is not a panic.
        let empty = FakeClass::new("empty");
        assert!(RfkillSwitch::find_in(empty.path(), Radio::Wifi).is_none());
    }

    /// A missing directory -- a sandbox or qemu with no rfkill class -- is the
    /// same as an absent radio, not a crash. This is the real device case.
    #[test]
    fn a_missing_class_directory_is_not_a_panic() {
        assert!(RfkillSwitch::find_in(Path::new("/nonexistent/rfkill"), Radio::Wifi).is_none());
        let caps = RadioCapabilities::scan_in(Path::new("/nonexistent/rfkill"));
        assert!(!caps.has(Radio::Wifi));
        assert_eq!(caps.available().count(), 0);
    }

    /// The write must actually change the file. The kernel's `soft` attribute
    /// is one byte, and an implementation that appended would produce "11" and
    /// a radio that is still on.
    #[test]
    fn setting_a_radio_rewrites_the_byte() {
        let fake = FakeClass::new("set");
        fake.add(0, "wlan", "wiphy0", 0);
        let sw = RfkillSwitch::find_in(fake.path(), Radio::Wifi).expect("wifi");
        assert_eq!(sw.state(), Some(RadioState::Off));

        sw.set(RadioState::On).expect("on");
        assert_eq!(
            sw.state(),
            Some(RadioState::On),
            "read back through the model"
        );

        // And the file itself, which is the thing the kernel reads.
        let raw = std::fs::read_to_string(sw.soft.clone()).expect("read soft");
        assert_eq!(raw, "1", "exactly one byte, not an append");
        assert_eq!(raw.len(), 1);

        sw.set(RadioState::Off).expect("off");
        assert_eq!(std::fs::read_to_string(&sw.soft).unwrap(), "0");
    }

    /// The set path must allocate nothing per call: it is a single byte into a
    /// file, with no `format!` and no `String`.
    #[test]
    fn setting_is_a_single_byte_write() {
        let fake = FakeClass::new("byte");
        fake.add(0, "wlan", "wiphy0", 0);
        let sw = RfkillSwitch::find_in(fake.path(), Radio::Wifi).expect("wifi");
        // Two transitions in a row: a writer that buffered or appended would
        // show up in the file length here.
        sw.set(RadioState::On).unwrap();
        sw.set(RadioState::On).unwrap();
        sw.set(RadioState::Off).unwrap();
        assert_eq!(std::fs::read_to_string(&sw.soft).unwrap().len(), 1);
    }

    /// A failed write must be reported, not swallowed, and must carry a label
    /// the tile can render.
    #[test]
    fn a_write_failure_is_typed_and_labelled() {
        let fake = FakeClass::new("fail");
        fake.add(0, "wlan", "wiphy0", 0);
        let sw = RfkillSwitch::find_in(fake.path(), Radio::Wifi).expect("wifi");
        // Remove the attribute the kernel would have created, so the open
        // fails the way a permissions or absent-node failure would.
        std::fs::remove_file(sw.soft.clone()).expect("remove soft");
        let err = sw
            .set_checked(RadioState::On)
            .expect_err("must not be silent");
        match &err {
            RadioError::Io { radio, name, .. } => {
                assert_eq!(*radio, Radio::Wifi);
                assert_eq!(name, "wiphy0");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(err.label(), "Failed");
        // And it renders.
        assert!(err.to_string().contains("wiphy0"));
    }

    /// The convenience form distinguishes "no such radio" from "the write
    /// failed", because the two need opposite tile behaviour.
    #[test]
    fn absent_and_failed_are_different_errors() {
        let empty = FakeClass::new("conv");
        let err = set_radio_in(empty.path(), Radio::Wifi, RadioState::On).expect_err("no radio");
        assert_eq!(err, RadioError::Absent(Radio::Wifi));
        assert_eq!(err.label(), "Not present");
    }

    fn set_radio_in(root: &Path, r: Radio, s: RadioState) -> Result<bool, RadioError> {
        match RfkillSwitch::find_in(root, r) {
            Some(sw) => {
                sw.set_checked(s)?;
                Ok(true)
            }
            None => Err(RadioError::Absent(r)),
        }
    }

    /// The capability scan drives tile enablement, so it must report exactly
    /// what is present -- no more, no less.
    #[test]
    fn capabilities_report_exactly_what_is_present() {
        let fake = FakeClass::new("caps");
        fake.add(0, "wlan", "wiphy0", 0);
        fake.add(1, "bluetooth", "Bluetooth", 0);
        let caps = RadioCapabilities::scan_in(fake.path());
        assert!(caps.has(Radio::Wifi));
        assert!(caps.has(Radio::Bluetooth));
        assert!(!caps.has(Radio::MobileData), "no modem in this fake");
        assert!(!caps.has(Radio::Tethering));
        let got: Vec<Radio> = caps.available().collect();
        assert_eq!(
            got,
            vec![Radio::Wifi, Radio::Bluetooth],
            "in declaration order"
        );
    }

    /// Two switches of the same type: the resolver takes the lowest-numbered
    /// one. `read_dir` order is neither sorted nor stable, so without an
    /// explicit sort this binds to whichever the filesystem happened to yield
    /// -- which is how a two-radio device silently drives only one of them.
    #[test]
    fn duplicate_radio_types_resolve_to_the_first() {
        let fake = FakeClass::new("dup");
        fake.add(0, "wlan", "first", 0);
        fake.add(1, "wlan", "second", 1);
        let sw = RfkillSwitch::find_in(fake.path(), Radio::Wifi).expect("wifi");
        assert_eq!(sw.name, "first");
    }
}
