//! Integrated Virtual Keyboard (IME - Gboard Style).
//! Conforms to Wayland text-input-v3 and zwp_input_method_v2 protocols.
//! Provides smooth viewport push animation, QWERTY/symbols/numeric layouts,
//! commit_string, delete_surrounding_text, and haptic feedback triggers.

use crate::compositor::launcher::{SpringConfig, SpringOscillator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardLayout {
    Qwerty,
    Symbols,
    Numeric,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImeAction {
    None,
    /// Single-character commit: zero-allocation fast path for the common
    /// ASCII case.
    CommitChar(char),
    CommitString(String),
    DeleteSurroundingText {
        before_length: u32,
        after_length: u32,
    },
    SendKey(u32), // Linux evdev keycode e.g. KEY_ENTER (28)
}

/// Gboard-Style On-Screen Virtual Keyboard
pub struct VirtualKeyboard {
    pub display_width: f32,
    pub display_height: f32,
    pub keyboard_height: f32, // e.g. 320.0 px
    pub layout: KeyboardLayout,
    pub is_shift_active: bool,
    pub is_caps_lock: bool,
    pub is_active: bool,
    pub slide_spring: SpringOscillator, // 0.0 = Hidden, 1.0 = Fully visible
    pub last_haptic_trigger: bool,
}

impl VirtualKeyboard {
    pub fn new(display_width: f32, display_height: f32) -> Self {
        let keyboard_height = 320.0;
        Self {
            display_width,
            display_height,
            keyboard_height,
            layout: KeyboardLayout::Qwerty,
            is_shift_active: false,
            is_caps_lock: false,
            is_active: false,
            slide_spring: SpringOscillator::new(
                0.0,
                SpringConfig {
                    stiffness: 260.0,
                    damping: 26.0,
                    mass: 1.0,
                },
            ),
            last_haptic_trigger: false,
        }
    }

    /// Wayland text-input-v3 focus entered: activate IME
    pub fn activate(&mut self) {
        self.is_active = true;
        self.slide_spring.target = 1.0;
    }

    /// Wayland text-input-v3 focus left: deactivate IME
    pub fn deactivate(&mut self) {
        self.is_active = false;
        self.slide_spring.target = 0.0;
    }

    pub fn toggle(&mut self) {
        if self.is_active {
            self.deactivate();
        } else {
            self.activate();
        }
    }

    /// Calculates smooth viewport push translation for active app window
    /// so the text cursor remains visible above the keyboard
    pub fn window_viewport_push_y(&self) -> f32 {
        let progress = self.slide_spring.current.clamp(0.0, 1.0);
        progress * self.keyboard_height
    }

    /// Key tap handler
    pub fn handle_key_tap(&mut self, key: &str) -> ImeAction {
        self.last_haptic_trigger = true;

        match key {
            "SHIFT" => {
                self.is_shift_active = !self.is_shift_active;
                ImeAction::None
            }
            "?123" => {
                self.layout = KeyboardLayout::Symbols;
                ImeAction::None
            }
            "ABC" => {
                self.layout = KeyboardLayout::Qwerty;
                ImeAction::None
            }
            "123" => {
                self.layout = KeyboardLayout::Numeric;
                ImeAction::None
            }
            "BACKSPACE" => ImeAction::DeleteSurroundingText {
                before_length: 1,
                after_length: 0,
            },
            "ENTER" => ImeAction::SendKey(28), // KEY_ENTER
            "SPACE" => ImeAction::CommitChar(' '),
            _ => {
                // Single-char keys without double-scanning the string.
                let mut it = key.chars();
                match (it.next(), it.next()) {
                    (Some(ch), None) => {
                        let shifted = self.is_shift_active || self.is_caps_lock;
                        // ASCII fast path: no allocation. Non-ASCII case
                        // folding is the rare path and keeps the String form.
                        let out = if ch.is_ascii_alphabetic() {
                            if shifted {
                                ch.to_ascii_uppercase()
                            } else {
                                ch.to_ascii_lowercase()
                            }
                        } else if ch.is_ascii() {
                            ch
                        } else {
                            let s = if shifted {
                                ch.to_uppercase().to_string()
                            } else {
                                ch.to_lowercase().to_string()
                            };
                            if self.is_shift_active && !self.is_caps_lock {
                                self.is_shift_active = false;
                            }
                            return ImeAction::CommitString(s);
                        };

                        // Single shift turns off after typing
                        if self.is_shift_active && !self.is_caps_lock {
                            self.is_shift_active = false;
                        }

                        ImeAction::CommitChar(out)
                    }
                    _ => ImeAction::None,
                }
            }
        }
    }

    pub fn update(&mut self, dt: f32) {
        self.slide_spring.step(dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_keyboard_animation_and_viewport_push() {
        let mut ime = VirtualKeyboard::new(1080.0, 2400.0);
        assert_eq!(ime.window_viewport_push_y(), 0.0);

        ime.activate();
        for _ in 0..60 {
            ime.update(0.016);
        }

        assert!(ime.is_active);
        assert!((ime.window_viewport_push_y() - 320.0).abs() < 1.0);

        ime.deactivate();
        for _ in 0..60 {
            ime.update(0.016);
        }
        assert!(!ime.is_active);
        assert!(ime.window_viewport_push_y() < 1.0);
    }

    #[test]
    fn test_ime_key_actions_and_shift() {
        let mut ime = VirtualKeyboard::new(1080.0, 2400.0);

        // Lowercase typing
        let act1 = ime.handle_key_tap("a");
        assert_eq!(act1, ImeAction::CommitChar('a'));

        // Shift then type
        ime.handle_key_tap("SHIFT");
        assert!(ime.is_shift_active);
        let act2 = ime.handle_key_tap("b");
        assert_eq!(act2, ImeAction::CommitChar('B'));
        assert!(!ime.is_shift_active); // Auto-reverted

        // Space and Backspace
        let act_space = ime.handle_key_tap("SPACE");
        assert_eq!(act_space, ImeAction::CommitChar(' '));

        let act_del = ime.handle_key_tap("BACKSPACE");
        assert_eq!(
            act_del,
            ImeAction::DeleteSurroundingText {
                before_length: 1,
                after_length: 0
            }
        );
    }
}
