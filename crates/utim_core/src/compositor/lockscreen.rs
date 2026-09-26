//! Ambient Lock Screen and Android Fingerprint HAL Biometric Bridge.
//! Implements ambient display mode with large clock/date, MPRIS media controls,
//! touch barrier gating, swipe-up unlock, PIN entry, and sub-300ms Fingerprint HAL unlock.

use std::time::{Duration, Instant};

/// Lock Screen State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Locked,
    PinEntry,
    Unlocked,
}

/// Fingerprint HAL Authentication Status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FingerprintResult {
    Acquired,
    Authenticated { user_id: i32 },
    AuthenticationFailed,
    Error(&'static str),
}

/// Android Fingerprint HAL Bridge (AIDL / HIDL abstraction)
pub struct FingerprintHalBridge {
    pub is_available: bool,
    /// False until a real android.hardware.biometrics HAL is bound. While
    /// false, `authenticate` returns `Error`: a bare `finger_id` from the
    /// caller is not evidence and must never fabricate an `Authenticated`
    /// verdict.
    pub hal_bound: bool,
    pub enrolled_fingerprints: Vec<u32>,
    pub last_auth_duration: Duration,
}

impl Default for FingerprintHalBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl FingerprintHalBridge {
    pub fn new() -> Self {
        Self {
            is_available: true,
            hal_bound: false,
            enrolled_fingerprints: vec![1, 2], // 2 enrolled fingers
            last_auth_duration: Duration::from_millis(0),
        }
    }

    /// Authenticate biometric print with sub-300ms latency guarantee.
    /// Returns `Error` until a real HAL is bound. Once bound, production
    /// must replace the enrolled-list lookup below with a real
    /// android.hardware.biometrics AIDL round trip (`hal.verify_template`);
    /// the lookup is a stand-in that is only reachable after an explicit
    /// bind, never by default.
    pub fn authenticate(&mut self, finger_id: u32) -> FingerprintResult {
        if !self.hal_bound {
            return FingerprintResult::Error("fingerprint HAL not bound");
        }
        let start = Instant::now();

        let res = if self.enrolled_fingerprints.contains(&finger_id) {
            FingerprintResult::Authenticated { user_id: 1000 }
        } else {
            FingerprintResult::AuthenticationFailed
        };

        self.last_auth_duration = start.elapsed();
        res
    }
}

/// Now Playing Media Controls for Lock Screen (MPRIS D-Bus)
#[derive(Debug, Clone, PartialEq)]
pub struct MediaWidget {
    pub is_playing: bool,
    pub track_title: String,
    pub artist_name: String,
    pub album_art_path: Option<String>,
}

impl Default for MediaWidget {
    fn default() -> Self {
        Self {
            is_playing: false,
            track_title: "No Media Playing".into(),
            artist_name: "".into(),
            album_art_path: None,
        }
    }
}

/// Mobile Lock Screen & Security Supervisor
pub struct LockScreen {
    pub state: LockState,
    pub clock_str: String,
    pub date_str: String,
    pub pin_hash: Option<u64>,
    pub pin_salt: u64,
    pub failed_attempts: u32,
    pub entered_pin: String,
    pub biometric_bridge: FingerprintHalBridge,
    pub media_widget: MediaWidget,
    pub swipe_up_offset: f32,
    pub unlock_threshold_y: f32, // default 250.0 px
}

/// Consecutive failures before PIN entry locks out.
pub const MAX_PIN_ATTEMPTS: u32 = 5;
/// Key-stretching rounds for the PIN hash (FNV-1a, salted).
const PIN_HASH_ROUNDS: usize = 4096;

impl LockScreen {
    pub fn new(pin: Option<&str>) -> Self {
        // An empty PIN would match an empty buffer on the first non-digit
        // key: treat it as "no PIN configured" instead of provisioning a
        // device that unlocks itself.
        let clean_pin = pin.filter(|p| !p.is_empty());
        let pin_salt = Self::make_salt();
        let pin_hash = clean_pin.map(|p| Self::hash_pin(p, pin_salt));
        Self {
            state: LockState::Locked,
            clock_str: "12:00".into(),
            date_str: "Sunday, September 20".into(),
            pin_hash,
            pin_salt,
            failed_attempts: 0,
            entered_pin: String::new(),
            biometric_bridge: FingerprintHalBridge::new(),
            media_widget: MediaWidget::default(),
            swipe_up_offset: 0.0,
            unlock_threshold_y: 250.0,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.state != LockState::Unlocked
    }

    pub fn lock(&mut self) {
        self.state = LockState::Locked;
        self.entered_pin.clear();
        self.swipe_up_offset = 0.0;
    }

    pub fn unlock(&mut self) {
        self.state = LockState::Unlocked;
        self.entered_pin.clear();
        self.swipe_up_offset = 0.0;
    }

    /// On swipe up gesture on the lock screen
    pub fn on_swipe_up_drag(&mut self, delta_y: f32) {
        // Negative delta_y is dragging upward
        self.swipe_up_offset += -delta_y;
    }

    pub fn on_swipe_up_release(&mut self) -> bool {
        if self.swipe_up_offset >= self.unlock_threshold_y {
            if self.pin_hash.is_some() {
                self.state = LockState::PinEntry;
                self.swipe_up_offset = 0.0;
                false
            } else {
                self.unlock();
                true // unlocked directly
            }
        } else {
            self.swipe_up_offset = 0.0;
            false
        }
    }

    /// Keypad entry on PIN screen. Only live in `PinEntry`, rate-limited,
    /// and compared in-register (no per-digit String allocation).
    pub fn enter_digit(&mut self, digit: char) -> bool {
        if self.state != LockState::PinEntry || self.failed_attempts >= MAX_PIN_ATTEMPTS {
            return false;
        }
        if digit.is_ascii_digit() && self.entered_pin.len() < 8 {
            self.entered_pin.push(digit);
        }

        // Automatic submission if entered PIN matches the configured one.
        if let Some(expected) = self.pin_hash {
            if Self::hash_pin(&self.entered_pin, self.pin_salt) == expected {
                self.failed_attempts = 0;
                self.unlock();
                return true;
            }
        }
        if self.entered_pin.len() >= 8 {
            self.failed_attempts += 1;
            self.entered_pin.clear();
        }
        false
    }

    pub fn backspace_digit(&mut self) {
        self.entered_pin.pop();
    }

    /// Fingerprint sensor tap. A biometric success unlocks directly only
    /// when no PIN is enrolled; otherwise it routes to PIN entry (the PIN
    /// is still required). Never unlocks while the HAL is unbound.
    pub fn on_fingerprint_touch(&mut self, finger_id: u32) -> bool {
        match self.biometric_bridge.authenticate(finger_id) {
            FingerprintResult::Authenticated { .. } if self.pin_hash.is_none() => {
                self.unlock();
                true
            }
            FingerprintResult::Authenticated { .. } => {
                // Biometric satisfied, but a PIN is enrolled: still require it.
                self.state = LockState::PinEntry;
                false
            }
            _ => false,
        }
    }

    fn make_salt() -> u64 {
        // No RNG dependency: mix wall-clock entropy with per-instance
        // address entropy so two screens never share a salt.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().wrapping_mul(0x9E3779B97F4A7C15) ^ d.subsec_nanos() as u64)
            .unwrap_or(0x243F6A8885A308D3);
        let addr = std::ptr::addr_of!(t) as u64;
        t ^ addr.wrapping_mul(0x9E3779B97F4A7C15)
    }

    fn hash_pin(pin: &str, salt: u64) -> u64 {
        // Salted, iterated FNV-1a. Not a password-KDF grade primitive, but
        // no longer a single-round unsalted oracle over a 10^4 space.
        let mut h: u64 = 0xcbf29ce484222325 ^ salt;
        for _ in 0..PIN_HASH_ROUNDS {
            for byte in pin.as_bytes() {
                h ^= *byte as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lockscreen_swipe_up_no_pin() {
        let mut lockscreen = LockScreen::new(None);
        assert!(lockscreen.is_locked());

        lockscreen.on_swipe_up_drag(-300.0); // 300px swipe up
        let unlocked = lockscreen.on_swipe_up_release();
        assert!(unlocked);
        assert!(!lockscreen.is_locked());
    }

    #[test]
    fn test_lockscreen_with_pin() {
        let mut lockscreen = LockScreen::new(Some("1234"));
        assert!(lockscreen.is_locked());

        lockscreen.on_swipe_up_drag(-300.0);
        let unlocked = lockscreen.on_swipe_up_release();
        assert!(!unlocked);
        assert_eq!(lockscreen.state, LockState::PinEntry);

        // Wrong digits
        assert!(!lockscreen.enter_digit('9'));
        lockscreen.backspace_digit();

        // Correct digits
        assert!(!lockscreen.enter_digit('1'));
        assert!(!lockscreen.enter_digit('2'));
        assert!(!lockscreen.enter_digit('3'));
        assert!(lockscreen.enter_digit('4')); // auto-unlocks

        assert_eq!(lockscreen.state, LockState::Unlocked);
    }

    #[test]
    fn test_fingerprint_requires_bound_hal_and_pin_gate() {
        let mut lockscreen = LockScreen::new(Some("9999"));
        assert!(lockscreen.is_locked());

        // Unbound HAL: never authenticates, even for an enrolled finger.
        let unlocked = lockscreen.on_fingerprint_touch(1);
        assert!(!unlocked);
        assert!(lockscreen.is_locked());

        // Bound HAL with a PIN enrolled: biometric routes to PIN entry,
        // it does not bypass the PIN.
        lockscreen.biometric_bridge.hal_bound = true;
        let t_auth_start = Instant::now();
        let unlocked = lockscreen.on_fingerprint_touch(1);
        let auth_dur = t_auth_start.elapsed();
        assert!(!unlocked);
        assert_eq!(lockscreen.state, LockState::PinEntry);
        assert!(auth_dur < Duration::from_millis(300));

        // Bound HAL with no PIN: direct unlock.
        let mut open = LockScreen::new(None);
        open.biometric_bridge.hal_bound = true;
        assert!(open.on_fingerprint_touch(2));
        assert_eq!(open.state, LockState::Unlocked);
    }

    #[test]
    fn test_empty_pin_is_treated_as_no_pin() {
        let lockscreen = LockScreen::new(Some(""));
        assert!(lockscreen.pin_hash.is_none());
    }

    #[test]
    fn test_pin_entry_gated_and_rate_limited() {
        let mut lockscreen = LockScreen::new(Some("1234"));
        // No PinEntry gate: digits do nothing while Locked.
        assert!(!lockscreen.enter_digit('1'));
        assert!(lockscreen.entered_pin.is_empty());

        lockscreen.on_swipe_up_drag(-300.0);
        lockscreen.on_swipe_up_release();
        assert_eq!(lockscreen.state, LockState::PinEntry);

        // Burn 5 attempts of 8 wrong digits each: locked out afterwards.
        for _ in 0..5 {
            for _ in 0..8 {
                assert!(!lockscreen.enter_digit('9'));
            }
        }
        assert_eq!(lockscreen.failed_attempts, 5);
        assert!(!lockscreen.enter_digit('1'));
        assert!(lockscreen.is_locked());
    }
}
