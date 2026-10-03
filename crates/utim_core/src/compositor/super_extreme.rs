//! Android Recovery-Style Super Extreme Power Saver TTY Shell.
//! Zero dynamic allocation on hot paths, event-driven, touch-screen compatible.
//! Adheres strictly to GEMINI.md systems discipline.

use crate::camera::ascii_video::AsciiCameraPreview;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Active Screen inside the Super Extreme TTY Shell
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperExtremeScreen {
    Lock,
    Password,
    EmergencyDialer,
    CameraPreview,
    Home,
    AppAlarm,
    AppPhone,
    AppSms,
    AppSettings,
    PowerMenu,
}

/// Key on the on-screen TTY keyboard
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyKey {
    Char(char),
    Backspace,
    Enter,
    Cancel,
}

/// Volume HUD overlay state
#[derive(Debug, Clone)]
pub struct VolumeHud {
    pub volume_percent: u8,
    pub visible_until: Option<Instant>,
}

impl Default for VolumeHud {
    fn default() -> Self {
        Self {
            volume_percent: 50,
            visible_until: None,
        }
    }
}

impl VolumeHud {
    pub fn trigger(&mut self, new_vol: u8) {
        self.volume_percent = new_vol.min(100);
        self.visible_until = Some(Instant::now() + Duration::from_millis(2000));
    }

    pub fn is_visible(&self) -> bool {
        self.visible_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// Format 1-line ASCII volume bar for row 0.
    pub fn format_bar(&self, width_chars: usize) -> String {
        let pct = self.volume_percent as usize;
        let prefix = "VOL [";
        let suffix = format!("] {}%", pct);
        let bar_width = width_chars
            .saturating_sub(prefix.len() + suffix.len())
            .max(4);
        let filled = (pct * bar_width) / 100;
        let empty = bar_width.saturating_sub(filled);

        let mut out = String::with_capacity(width_chars);
        out.push_str(prefix);
        for _ in 0..filled {
            out.push('=');
        }
        for _ in 0..empty {
            out.push(' ');
        }
        out.push_str(&suffix);
        out
    }
}

/// Power menu recovery options
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMenuAction {
    ReturnToNormal,
    RebootSystem,
    RebootBootloader,
    RebootRecovery,
    PowerOff,
    Cancel,
}

/// Alarm entry
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmItem {
    pub time_str: &'static str,
    pub label: &'static str,
    pub enabled: bool,
}

/// SMS message entry
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsItem {
    pub sender: &'static str,
    pub snippet: &'static str,
    pub time: &'static str,
}

/// Super Extreme Recovery Shell State Machine
pub struct SuperExtremeState {
    pub active_screen: SuperExtremeScreen,
    pub prev_screen: SuperExtremeScreen,
    pub volume_hud: VolumeHud,
    pub password_input: String,
    pub password_error: bool,
    pub emergency_input: String,
    pub phone_input: String,
    pub home_selected_index: usize,
    pub power_menu_selected: usize,
    pub camera_preview: AsciiCameraPreview,
    pub power_press_start: Option<Instant>,
    pub alarms: [AlarmItem; 3],
    pub sms_messages: [SmsItem; 3],
    pub request_exit_to_normal: bool,
    pub request_reboot: bool,
    pub request_poweroff: bool,
    pub last_action_message: Option<String>,
}

impl Default for SuperExtremeState {
    fn default() -> Self {
        Self::new()
    }
}

impl SuperExtremeState {
    pub fn new() -> Self {
        Self {
            active_screen: SuperExtremeScreen::Lock,
            prev_screen: SuperExtremeScreen::Lock,
            volume_hud: VolumeHud::default(),
            password_input: String::with_capacity(16),
            password_error: false,
            emergency_input: String::with_capacity(16),
            phone_input: String::with_capacity(16),
            home_selected_index: 0,
            power_menu_selected: 0,
            camera_preview: AsciiCameraPreview::new(),
            power_press_start: None,
            alarms: [
                AlarmItem {
                    time_str: "06:30",
                    label: "Work Alarm",
                    enabled: true,
                },
                AlarmItem {
                    time_str: "07:15",
                    label: "Exercise",
                    enabled: false,
                },
                AlarmItem {
                    time_str: "22:00",
                    label: "Sleep",
                    enabled: true,
                },
            ],
            sms_messages: [
                SmsItem {
                    sender: "ICE Emergency",
                    snippet: "Status OK",
                    time: "10:15",
                },
                SmsItem {
                    sender: "Doctor",
                    snippet: "Appointment confirmed",
                    time: "Yesterday",
                },
                SmsItem {
                    sender: "Operator",
                    snippet: "Emergency broadcast test",
                    time: "Sep 20",
                },
            ],
            request_exit_to_normal: false,
            request_reboot: false,
            request_poweroff: false,
            last_action_message: None,
        }
    }

    /// Reset state upon entering Super Extreme Mode
    pub fn enter_super_extreme(&mut self) {
        self.active_screen = SuperExtremeScreen::Lock;
        self.prev_screen = SuperExtremeScreen::Lock;
        self.password_input.clear();
        self.password_error = false;
        self.emergency_input.clear();
        self.phone_input.clear();
        self.request_exit_to_normal = false;
        self.request_reboot = false;
        self.request_poweroff = false;
        self.last_action_message = None;
    }

    /// Volume adjustment
    pub fn volume_up(&mut self) {
        let new_vol = self.volume_hud.volume_percent.saturating_add(10).min(100);
        self.volume_hud.trigger(new_vol);
    }

    pub fn volume_down(&mut self) {
        let new_vol = self.volume_hud.volume_percent.saturating_sub(10);
        self.volume_hud.trigger(new_vol);
    }

    /// Power button press and hold tracking
    pub fn on_power_button_press(&mut self) {
        if self.power_press_start.is_none() {
            self.power_press_start = Some(Instant::now());
        }
    }

    pub fn on_power_button_release(&mut self) -> bool {
        let held = self.power_press_start.take().is_some_and(|start| {
            Instant::now().duration_since(start) >= Duration::from_millis(1000)
        });
        if held {
            self.prev_screen = self.active_screen;
            self.active_screen = SuperExtremeScreen::PowerMenu;
            self.power_menu_selected = 0;
            true
        } else {
            false
        }
    }

    /// Check if power button is currently held >= 1.0s
    pub fn check_power_button_hold(&mut self) -> bool {
        if let Some(start) = self.power_press_start {
            if Instant::now().duration_since(start) >= Duration::from_millis(1000) {
                self.power_press_start = None;
                self.prev_screen = self.active_screen;
                self.active_screen = SuperExtremeScreen::PowerMenu;
                self.power_menu_selected = 0;
                return true;
            }
        }
        false
    }

    /// Lock screen action: Emergency call
    pub fn trigger_emergency_call(&mut self) {
        self.prev_screen = self.active_screen;
        self.active_screen = SuperExtremeScreen::EmergencyDialer;
        self.emergency_input.clear();
    }

    /// Lock screen action: Snap photo with Front Camera
    pub fn trigger_camera_preview(&mut self) {
        self.prev_screen = self.active_screen;
        self.active_screen = SuperExtremeScreen::CameraPreview;
        let _ = self.camera_preview.start();
    }

    /// Lock screen action: Swipe up to unlock
    pub fn on_swipe_up(&mut self) {
        if self.active_screen == SuperExtremeScreen::Lock {
            self.active_screen = SuperExtremeScreen::Password;
            self.password_input.clear();
            self.password_error = false;
        }
    }

    /// Password screen: enter character via on-screen TTY keyboard or physical key
    pub fn enter_password_char(
        &mut self,
        c: char,
        expected_pin_hash: Option<u64>,
        pin_salt: u64,
    ) -> bool {
        if self.active_screen != SuperExtremeScreen::Password {
            return false;
        }
        if self.password_input.len() < 16 {
            self.password_input.push(c);
        }
        self.check_password_submission(expected_pin_hash, pin_salt)
    }

    pub fn password_backspace(&mut self) {
        if self.active_screen == SuperExtremeScreen::Password {
            self.password_input.pop();
            self.password_error = false;
        }
    }

    pub fn submit_password(&mut self, expected_pin_hash: Option<u64>, pin_salt: u64) -> bool {
        self.check_password_submission(expected_pin_hash, pin_salt)
    }

    fn check_password_submission(&mut self, expected_pin_hash: Option<u64>, pin_salt: u64) -> bool {
        if let Some(expected) = expected_pin_hash {
            let hash = hash_pin(&self.password_input, pin_salt);
            if hash == expected {
                self.active_screen = SuperExtremeScreen::Home;
                self.password_input.clear();
                self.password_error = false;
                return true;
            } else if self.password_input.len() >= 8 {
                self.password_error = true;
                self.password_input.clear();
            }
        } else {
            // No PIN enrolled: any non-empty input or enter unlocks
            if !self.password_input.is_empty() || self.active_screen == SuperExtremeScreen::Password
            {
                self.active_screen = SuperExtremeScreen::Home;
                self.password_input.clear();
                return true;
            }
        }
        false
    }

    /// Snap photo while in camera preview screen
    pub fn snap_photo(&mut self) -> Option<PathBuf> {
        if self.active_screen == SuperExtremeScreen::CameraPreview {
            if let Ok(path) = self.camera_preview.snap_photo() {
                self.last_action_message = Some(format!(
                    "Snapped photo: {:?}",
                    path.file_name().unwrap_or_default()
                ));
                return Some(path);
            }
        }
        None
    }

    /// Handle Home screen app launch
    pub fn launch_home_app(&mut self, app_index: usize) {
        match app_index {
            0 => self.active_screen = SuperExtremeScreen::AppAlarm,
            1 => self.active_screen = SuperExtremeScreen::AppPhone,
            2 => self.active_screen = SuperExtremeScreen::AppSms,
            3 => self.active_screen = SuperExtremeScreen::AppSettings,
            _ => {}
        }
    }

    /// Handle back button on sub-screens
    pub fn handle_back(&mut self) {
        match self.active_screen {
            SuperExtremeScreen::Password => self.active_screen = SuperExtremeScreen::Lock,
            SuperExtremeScreen::EmergencyDialer => self.active_screen = SuperExtremeScreen::Lock,
            SuperExtremeScreen::CameraPreview => {
                self.camera_preview.stop();
                self.active_screen = SuperExtremeScreen::Lock;
            }
            SuperExtremeScreen::AppAlarm
            | SuperExtremeScreen::AppPhone
            | SuperExtremeScreen::AppSms
            | SuperExtremeScreen::AppSettings => self.active_screen = SuperExtremeScreen::Home,
            SuperExtremeScreen::PowerMenu => self.active_screen = self.prev_screen,
            SuperExtremeScreen::Lock | SuperExtremeScreen::Home => {}
        }
    }

    /// Handle touch tap coordinate translation to UI actions
    pub fn handle_touch_tap(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        expected_pin_hash: Option<u64>,
        pin_salt: u64,
    ) -> bool {
        match self.active_screen {
            SuperExtremeScreen::Lock => {
                // Bottom left: Emergency Call (y in 0.82..0.90, x in 0.05..0.48)
                if y >= h * 0.80 && y <= h * 0.89 {
                    if x <= w * 0.48 {
                        self.trigger_emergency_call();
                        return true;
                    } else if x >= w * 0.52 {
                        self.trigger_camera_preview();
                        return true;
                    }
                }
                // Center bottom: Swipe / tap to unlock (y >= 0.90)
                if y >= h * 0.90 {
                    self.on_swipe_up();
                    return true;
                }
            }
            SuperExtremeScreen::Password => {
                // Check on-screen TTY keyboard at bottom half of screen
                if let Some(key) = hit_test_tty_keyboard(x, y, w, h) {
                    match key {
                        TtyKey::Char(c) => {
                            self.enter_password_char(c, expected_pin_hash, pin_salt);
                        }
                        TtyKey::Backspace => self.password_backspace(),
                        TtyKey::Enter => {
                            self.submit_password(expected_pin_hash, pin_salt);
                        }
                        TtyKey::Cancel => self.handle_back(),
                    }
                    return true;
                }
            }
            SuperExtremeScreen::CameraPreview => {
                // Bottom button: [ SNAP PHOTO ] (y >= 0.85)
                if y >= h * 0.85 && y <= h * 0.93 {
                    self.snap_photo();
                    return true;
                } else if y > h * 0.93 {
                    self.handle_back();
                    return true;
                }
            }
            SuperExtremeScreen::EmergencyDialer => {
                // Keypad hits: numbers 0..9, Call, Back
                if let Some(key) = hit_test_numeric_keypad(x, y, w, h) {
                    match key {
                        TtyKey::Char(c) => {
                            if self.emergency_input.len() < 12 {
                                self.emergency_input.push(c);
                            }
                        }
                        TtyKey::Backspace => {
                            self.emergency_input.pop();
                        }
                        TtyKey::Enter => {
                            self.last_action_message =
                                Some(format!("Emergency call placed: {}", self.emergency_input));
                        }
                        TtyKey::Cancel => self.handle_back(),
                    }
                    return true;
                }
            }
            SuperExtremeScreen::Home => {
                // 4 App entries located between y = 0.35 and y = 0.75
                let row_h = h * 0.09;
                let start_y = h * 0.35;
                for i in 0..4 {
                    let y0 = start_y + (i as f32 * row_h);
                    let y1 = y0 + row_h * 0.85;
                    if y >= y0 && y <= y1 && x >= w * 0.08 && x <= w * 0.92 {
                        self.launch_home_app(i);
                        return true;
                    }
                }
                // Bottom Power menu button (y >= 0.88)
                if y >= h * 0.88 {
                    self.prev_screen = self.active_screen;
                    self.active_screen = SuperExtremeScreen::PowerMenu;
                    return true;
                }
            }
            SuperExtremeScreen::AppAlarm => {
                // Tapping an alarm toggles it
                let start_y = h * 0.30;
                let row_h = h * 0.10;
                for i in 0..self.alarms.len() {
                    let y0 = start_y + (i as f32 * row_h);
                    let y1 = y0 + row_h * 0.85;
                    if y >= y0 && y <= y1 {
                        self.alarms[i].enabled = !self.alarms[i].enabled;
                        return true;
                    }
                }
                if y >= h * 0.88 {
                    self.handle_back();
                    return true;
                }
            }
            SuperExtremeScreen::AppPhone => {
                if let Some(key) = hit_test_numeric_keypad(x, y, w, h) {
                    match key {
                        TtyKey::Char(c) => {
                            if self.phone_input.len() < 15 {
                                self.phone_input.push(c);
                            }
                        }
                        TtyKey::Backspace => {
                            self.phone_input.pop();
                        }
                        TtyKey::Enter => {
                            self.last_action_message =
                                Some(format!("Calling {}", self.phone_input));
                        }
                        TtyKey::Cancel => self.handle_back(),
                    }
                    return true;
                }
            }
            SuperExtremeScreen::AppSms => {
                if y >= h * 0.88 {
                    self.handle_back();
                    return true;
                }
            }
            SuperExtremeScreen::AppSettings => {
                // Exit to normal mode button (y in 0.50..0.60)
                if y >= h * 0.50 && y <= h * 0.60 && x >= w * 0.10 && x <= w * 0.90 {
                    self.request_exit_to_normal = true;
                    return true;
                }
                if y >= h * 0.88 {
                    self.handle_back();
                    return true;
                }
            }
            SuperExtremeScreen::PowerMenu => {
                // Power options: 6 items between y = 0.30 and y = 0.78
                let row_h = h * 0.08;
                let start_y = h * 0.30;
                for i in 0..6 {
                    let y0 = start_y + (i as f32 * row_h);
                    let y1 = y0 + row_h * 0.85;
                    if y >= y0 && y <= y1 && x >= w * 0.10 && x <= w * 0.90 {
                        match i {
                            0 => self.request_exit_to_normal = true,
                            1 => self.request_reboot = true,
                            2 | 3 => self.request_reboot = true,
                            4 => self.request_poweroff = true,
                            5 => self.handle_back(),
                            _ => {}
                        }
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Hit test on-screen touchable TTY keyboard (occupies bottom 40% of screen).
pub fn hit_test_tty_keyboard(x: f32, y: f32, w: f32, h: f32) -> Option<TtyKey> {
    let kb_top = h * 0.60;
    let kb_h = h * 0.38;
    if y < kb_top || y > kb_top + kb_h {
        return None;
    }

    let rel_y = y - kb_top;
    let row_h = kb_h / 4.0;
    let row = (rel_y / row_h) as usize;

    match row {
        0 => {
            // Row 0: Digits 1..0 (10 keys)
            let col_w = w / 10.0;
            let col = (x / col_w).min(9.0) as usize;
            let digits = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
            Some(TtyKey::Char(digits[col]))
        }
        1 => {
            // Row 1: Q W E R T Y U I O P (10 keys)
            let col_w = w / 10.0;
            let col = (x / col_w).min(9.0) as usize;
            let chars = ['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P'];
            Some(TtyKey::Char(chars[col]))
        }
        2 => {
            // Row 2: A S D F G H J K L (9 keys, inset)
            let pad = w * 0.05;
            let width = w * 0.90;
            if x >= pad && x <= pad + width {
                let col_w = width / 9.0;
                let col = ((x - pad) / col_w).min(8.0) as usize;
                let chars = ['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L'];
                Some(TtyKey::Char(chars[col]))
            } else {
                None
            }
        }
        3 => {
            // Row 3: [CANCEL] Z X C V B N M [⌫] [ENTER]
            let col_w = w / 10.0;
            let col = (x / col_w).min(9.0) as usize;
            match col {
                0 => Some(TtyKey::Cancel),
                1 => Some(TtyKey::Char('Z')),
                2 => Some(TtyKey::Char('X')),
                3 => Some(TtyKey::Char('C')),
                4 => Some(TtyKey::Char('V')),
                5 => Some(TtyKey::Char('B')),
                6 => Some(TtyKey::Char('N')),
                7 => Some(TtyKey::Char('M')),
                8 => Some(TtyKey::Backspace),
                9 => Some(TtyKey::Enter),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Hit test a 3x4 numeric keypad (e.g. for emergency dialer or phone dialer).
pub fn hit_test_numeric_keypad(x: f32, y: f32, w: f32, h: f32) -> Option<TtyKey> {
    let pad_top = h * 0.45;
    let pad_h = h * 0.42;
    if y < pad_top || y > pad_top + pad_h {
        if y > h * 0.90 {
            return Some(TtyKey::Cancel);
        }
        return None;
    }

    let rel_y = y - pad_top;
    let row_h = pad_h / 4.0;
    let row = (rel_y / row_h) as usize;

    let pad_w = w * 0.80;
    let pad_x = w * 0.10;
    if x < pad_x || x > pad_x + pad_w {
        return None;
    }

    let col_w = pad_w / 3.0;
    let col = ((x - pad_x) / col_w).min(2.0) as usize;

    match (row, col) {
        (0, 0) => Some(TtyKey::Char('1')),
        (0, 1) => Some(TtyKey::Char('2')),
        (0, 2) => Some(TtyKey::Char('3')),
        (1, 0) => Some(TtyKey::Char('4')),
        (1, 1) => Some(TtyKey::Char('5')),
        (1, 2) => Some(TtyKey::Char('6')),
        (2, 0) => Some(TtyKey::Char('7')),
        (2, 1) => Some(TtyKey::Char('8')),
        (2, 2) => Some(TtyKey::Char('9')),
        (3, 0) => Some(TtyKey::Backspace),
        (3, 1) => Some(TtyKey::Char('0')),
        (3, 2) => Some(TtyKey::Enter),
        _ => None,
    }
}

fn hash_pin(pin: &str, salt: u64) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325 ^ salt;
    for _ in 0..4096 {
        for byte in pin.as_bytes() {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_super_extreme_state_transitions() {
        let mut state = SuperExtremeState::new();
        assert_eq!(state.active_screen, SuperExtremeScreen::Lock);

        // Emergency call
        state.trigger_emergency_call();
        assert_eq!(state.active_screen, SuperExtremeScreen::EmergencyDialer);
        state.handle_back();
        assert_eq!(state.active_screen, SuperExtremeScreen::Lock);

        // Camera preview
        state.trigger_camera_preview();
        assert_eq!(state.active_screen, SuperExtremeScreen::CameraPreview);
        state.handle_back();
        assert_eq!(state.active_screen, SuperExtremeScreen::Lock);

        // Swipe up to password
        state.on_swipe_up();
        assert_eq!(state.active_screen, SuperExtremeScreen::Password);

        // Enter password and unlock
        let unlocked = state.enter_password_char('1', None, 0);
        assert!(unlocked);
        assert_eq!(state.active_screen, SuperExtremeScreen::Home);

        // Launch Alarm app
        state.launch_home_app(0);
        assert_eq!(state.active_screen, SuperExtremeScreen::AppAlarm);
        state.handle_back();
        assert_eq!(state.active_screen, SuperExtremeScreen::Home);
    }

    #[test]
    fn test_volume_hud_and_power_hold() {
        let mut state = SuperExtremeState::new();
        state.volume_up();
        assert_eq!(state.volume_hud.volume_percent, 60);
        assert!(state.volume_hud.is_visible());
        let bar = state.volume_hud.format_bar(30);
        assert!(bar.starts_with("VOL ["));
        assert!(bar.contains("60%"));

        // Power button press and release hold
        state.power_press_start = Some(Instant::now() - Duration::from_millis(1500));
        let opened = state.on_power_button_release();
        assert!(opened);
        assert_eq!(state.active_screen, SuperExtremeScreen::PowerMenu);
    }

    #[test]
    fn test_tty_keyboard_hit_testing() {
        let w = 1080.0;
        let h = 2400.0;

        // Hit key '1' on row 0
        let k1 = hit_test_tty_keyboard(50.0, h * 0.65, w, h);
        assert_eq!(k1, Some(TtyKey::Char('1')));

        // Hit key 'Q' on row 1
        let kq = hit_test_tty_keyboard(50.0, h * 0.72, w, h);
        assert_eq!(kq, Some(TtyKey::Char('Q')));

        // Hit key 'Enter' on row 3
        let k_ent = hit_test_tty_keyboard(w * 0.95, h * 0.92, w, h);
        assert_eq!(k_ent, Some(TtyKey::Enter));
    }
}
