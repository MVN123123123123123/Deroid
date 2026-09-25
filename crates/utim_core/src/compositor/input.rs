//! Zero-allocation Linux evdev input event decoder and dispatcher.
//! Conforms strictly to GEMINI.md systems discipline: no heap allocations on the hot path,
//! pure standard library POSIX evdev parsing, sub-millisecond dispatch latency.

use std::time::Instant;
use crate::compositor::gestures::{RawTouchEvent, TouchPhase};

// --- Linux Input ABI Constants (linux/input.h / linux/input-event-codes.h) ---
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0x00;

pub const BTN_LEFT: u16 = 0x110;   // 272
pub const BTN_RIGHT: u16 = 0x111;  // 273
pub const BTN_MIDDLE: u16 = 0x112; // 274
pub const BTN_TOUCH: u16 = 0x14a;  // 330

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_WHEEL: u16 = 0x08;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

// Key codes
pub const KEY_ESC: u16 = 1;
pub const KEY_1: u16 = 2;
pub const KEY_2: u16 = 3;
pub const KEY_3: u16 = 4;
pub const KEY_4: u16 = 5;
pub const KEY_5: u16 = 6;
pub const KEY_6: u16 = 7;
pub const KEY_7: u16 = 8;
pub const KEY_8: u16 = 9;
pub const KEY_9: u16 = 10;
pub const KEY_0: u16 = 11;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;
pub const KEY_E: u16 = 18;
pub const KEY_R: u16 = 19;
pub const KEY_T: u16 = 20;
pub const KEY_Y: u16 = 21;
pub const KEY_U: u16 = 22;
pub const KEY_I: u16 = 23;
pub const KEY_O: u16 = 24;
pub const KEY_P: u16 = 25;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_F: u16 = 33;
pub const KEY_G: u16 = 34;
pub const KEY_H: u16 = 35;
pub const KEY_J: u16 = 36;
pub const KEY_K: u16 = 37;
pub const KEY_L: u16 = 38;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_Z: u16 = 44;
pub const KEY_X: u16 = 45;
pub const KEY_C: u16 = 46;
pub const KEY_V: u16 = 47;
pub const KEY_B: u16 = 48;
pub const KEY_N: u16 = 49;
pub const KEY_M: u16 = 50;
pub const KEY_DOT: u16 = 52;
pub const KEY_SLASH: u16 = 53;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_SPACE: u16 = 57;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_UP: u16 = 103;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_DOWN: u16 = 108;

/// Exactly 24 bytes on 64-bit Linux architectures (aarch64, x86_64)
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct LinuxInputEvent {
    pub time_sec: u64,
    pub time_usec: u64,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl LinuxInputEvent {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    #[inline]
    pub fn from_raw_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        let ev = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const LinuxInputEvent) };
        Some(ev)
    }
}

/// Dispatched high-level action from raw evdev streams
#[derive(Debug, Clone, PartialEq)]
pub enum InputDispatchResult {
    None,
    Touch(RawTouchEvent),
    Tap { x: f32, y: f32 },
    LongPress { x: f32, y: f32 },
    PointerMove { x: f32, y: f32 },
    KeyPress { code: u16, ch: Option<char>, pressed: bool, repeat: bool, ctrl: bool },
}

/// Zero-allocation evdev dispatcher and coordinate normalizer
pub struct InputDispatcher {
    pub screen_width: f32,
    pub screen_height: f32,
    pub tablet_max_x: f32,
    pub tablet_max_y: f32,
    pub mouse_sensitivity: f32,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub is_touch_down: bool,
    pub touch_start_x: f32,
    pub touch_start_y: f32,
    pub touch_start_time: Instant,
    pub shift_active: bool,
    pub ctrl_active: bool,
    touch_id_counter: i32,
    pending_abs_x: Option<f32>,
    pending_abs_y: Option<f32>,
}

impl InputDispatcher {
    pub fn new(screen_width: f32, screen_height: f32) -> Self {
        Self {
            screen_width,
            screen_height,
            tablet_max_x: 32767.0,
            tablet_max_y: 32767.0,
            mouse_sensitivity: 0.35,
            cursor_x: screen_width / 2.0,
            cursor_y: screen_height / 2.0,
            is_touch_down: false,
            touch_start_x: 0.0,
            touch_start_y: 0.0,
            touch_start_time: Instant::now(),
            shift_active: false,
            ctrl_active: false,
            touch_id_counter: 1,
            pending_abs_x: None,
            pending_abs_y: None,
        }
    }

    /// Process a single raw Linux input event and produce high-level dispatched events
    pub fn process_event(&mut self, ev: &LinuxInputEvent) -> InputDispatchResult {
        match ev.type_ {
            EV_ABS => {
                match ev.code {
                    ABS_X | ABS_MT_POSITION_X => {
                        let normalized_x = (ev.value as f32 / self.tablet_max_x) * self.screen_width;
                        self.pending_abs_x = Some(normalized_x.clamp(0.0, self.screen_width - 1.0));
                    }
                    ABS_Y | ABS_MT_POSITION_Y => {
                        let normalized_y = (ev.value as f32 / self.tablet_max_y) * self.screen_height;
                        self.pending_abs_y = Some(normalized_y.clamp(0.0, self.screen_height - 1.0));
                    }
                    _ => {}
                }
                InputDispatchResult::None
            }
            EV_REL => {
                match ev.code {
                    REL_X => {
                        let delta = (ev.value as f32) * self.mouse_sensitivity;
                        self.cursor_x = (self.cursor_x + delta).clamp(0.0, self.screen_width - 1.0);
                    }
                    REL_Y => {
                        let delta = (ev.value as f32) * self.mouse_sensitivity;
                        self.cursor_y = (self.cursor_y + delta).clamp(0.0, self.screen_height - 1.0);
                    }
                    _ => {}
                }
                if self.is_touch_down {
                    InputDispatchResult::Touch(RawTouchEvent {
                        touch_id: self.touch_id_counter,
                        phase: TouchPhase::Move,
                        x: self.cursor_x,
                        y: self.cursor_y,
                        timestamp: Instant::now(),
                    })
                } else {
                    InputDispatchResult::PointerMove {
                        x: self.cursor_x,
                        y: self.cursor_y,
                    }
                }
            }
            EV_KEY => {
                if ev.code == BTN_LEFT || ev.code == BTN_TOUCH {
                    if let Some(x) = self.pending_abs_x.take() {
                        self.cursor_x = x;
                    }
                    if let Some(y) = self.pending_abs_y.take() {
                        self.cursor_y = y;
                    }

                    if ev.value == 1 {
                        // Button Down -> TouchPhase::Down
                        self.is_touch_down = true;
                        self.touch_start_x = self.cursor_x;
                        self.touch_start_y = self.cursor_y;
                        self.touch_start_time = Instant::now();

                        InputDispatchResult::Touch(RawTouchEvent {
                            touch_id: self.touch_id_counter,
                            phase: TouchPhase::Down,
                            x: self.cursor_x,
                            y: self.cursor_y,
                            timestamp: Instant::now(),
                        })
                    } else if ev.value == 0 {
                        // Button Up -> TouchPhase::Up
                        self.is_touch_down = false;
                        let dx = (self.cursor_x - self.touch_start_x).abs();
                        let dy = (self.cursor_y - self.touch_start_y).abs();
                        let dur = self.touch_start_time.elapsed();

                        let res = if dx < 25.0 && dy < 25.0 {
                            if dur.as_millis() >= 400 {
                                InputDispatchResult::LongPress {
                                    x: self.cursor_x,
                                    y: self.cursor_y,
                                }
                            } else {
                                InputDispatchResult::Tap {
                                    x: self.cursor_x,
                                    y: self.cursor_y,
                                }
                            }
                        } else {
                            InputDispatchResult::Touch(RawTouchEvent {
                                touch_id: self.touch_id_counter,
                                phase: TouchPhase::Up,
                                x: self.cursor_x,
                                y: self.cursor_y,
                                timestamp: Instant::now(),
                            })
                        };
                        self.touch_id_counter = self.touch_id_counter.wrapping_add(1);
                        res
                    } else {
                        InputDispatchResult::None
                    }
                } else if ev.code == 0x111 /* BTN_RIGHT */ && ev.value == 0 {
                    InputDispatchResult::LongPress {
                        x: self.cursor_x,
                        y: self.cursor_y,
                    }
                } else {
                    // Physical/virtio Keyboard key
                    let pressed = ev.value == 1 || ev.value == 2;
                    let repeat = ev.value == 2;
                    if ev.code == KEY_LEFTSHIFT || ev.code == KEY_RIGHTSHIFT {
                        self.shift_active = pressed;
                    }
                    if ev.code == KEY_LEFTCTRL || ev.code == KEY_RIGHTCTRL {
                        self.ctrl_active = pressed;
                    }
                    let ch = keycode_to_char(ev.code, self.shift_active);
                    InputDispatchResult::KeyPress {
                        code: ev.code,
                        ch,
                        pressed,
                        repeat,
                        ctrl: self.ctrl_active,
                    }
                }
            }
            EV_SYN => {
                if ev.code == SYN_REPORT {
                    let mut moved = false;
                    if let Some(x) = self.pending_abs_x.take() {
                        self.cursor_x = x;
                        moved = true;
                    }
                    if let Some(y) = self.pending_abs_y.take() {
                        self.cursor_y = y;
                        moved = true;
                    }
                    if moved {
                        if self.is_touch_down {
                            InputDispatchResult::Touch(RawTouchEvent {
                                touch_id: self.touch_id_counter,
                                phase: TouchPhase::Move,
                                x: self.cursor_x,
                                y: self.cursor_y,
                                timestamp: Instant::now(),
                            })
                        } else {
                            InputDispatchResult::PointerMove {
                                x: self.cursor_x,
                                y: self.cursor_y,
                            }
                        }
                    } else {
                        InputDispatchResult::None
                    }
                } else {
                    InputDispatchResult::None
                }
            }
            _ => InputDispatchResult::None,
        }
    }
}

/// Convert Linux keycode to ASCII character, respecting shift state
pub fn keycode_to_char(code: u16, shift: bool) -> Option<char> {
    match code {
        KEY_1 => Some(if shift { '!' } else { '1' }),
        KEY_2 => Some(if shift { '@' } else { '2' }),
        KEY_3 => Some(if shift { '#' } else { '3' }),
        KEY_4 => Some(if shift { '$' } else { '4' }),
        KEY_5 => Some(if shift { '%' } else { '5' }),
        KEY_6 => Some(if shift { '^' } else { '6' }),
        KEY_7 => Some(if shift { '&' } else { '7' }),
        KEY_8 => Some(if shift { '*' } else { '8' }),
        KEY_9 => Some(if shift { '(' } else { '9' }),
        KEY_0 => Some(if shift { ')' } else { '0' }),
        KEY_MINUS => Some(if shift { '_' } else { '-' }),
        KEY_EQUAL => Some(if shift { '+' } else { '=' }),
        KEY_Q => Some(if shift { 'Q' } else { 'q' }),
        KEY_W => Some(if shift { 'W' } else { 'w' }),
        KEY_E => Some(if shift { 'E' } else { 'e' }),
        KEY_R => Some(if shift { 'R' } else { 'r' }),
        KEY_T => Some(if shift { 'T' } else { 't' }),
        KEY_Y => Some(if shift { 'Y' } else { 'y' }),
        KEY_U => Some(if shift { 'U' } else { 'u' }),
        KEY_I => Some(if shift { 'I' } else { 'i' }),
        KEY_O => Some(if shift { 'O' } else { 'o' }),
        KEY_P => Some(if shift { 'P' } else { 'p' }),
        KEY_A => Some(if shift { 'A' } else { 'a' }),
        KEY_S => Some(if shift { 'S' } else { 's' }),
        KEY_D => Some(if shift { 'D' } else { 'd' }),
        KEY_F => Some(if shift { 'F' } else { 'f' }),
        KEY_G => Some(if shift { 'G' } else { 'g' }),
        KEY_H => Some(if shift { 'H' } else { 'h' }),
        KEY_J => Some(if shift { 'J' } else { 'j' }),
        KEY_K => Some(if shift { 'K' } else { 'k' }),
        KEY_L => Some(if shift { 'L' } else { 'l' }),
        KEY_Z => Some(if shift { 'Z' } else { 'z' }),
        KEY_X => Some(if shift { 'X' } else { 'x' }),
        KEY_C => Some(if shift { 'C' } else { 'c' }),
        KEY_V => Some(if shift { 'V' } else { 'v' }),
        KEY_B => Some(if shift { 'B' } else { 'b' }),
        KEY_N => Some(if shift { 'N' } else { 'n' }),
        KEY_M => Some(if shift { 'M' } else { 'm' }),
        KEY_DOT => Some(if shift { '>' } else { '.' }),
        KEY_SLASH => Some(if shift { '?' } else { '/' }),
        KEY_SPACE => Some(' '),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linux_input_event_size() {
        assert_eq!(LinuxInputEvent::SIZE, 24);
    }

    #[test]
    fn test_tablet_absolute_scaling_and_touch_phases() {
        let mut dispatcher = InputDispatcher::new(1080.0, 2400.0);

        // 1. Move tablet to center (16383, 16383)
        let ev_x = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_ABS,
            code: ABS_X,
            value: 16383,
        };
        assert_eq!(dispatcher.process_event(&ev_x), InputDispatchResult::None);

        let ev_y = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_ABS,
            code: ABS_Y,
            value: 16383,
        };
        assert_eq!(dispatcher.process_event(&ev_y), InputDispatchResult::None);

        let ev_syn = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };
        let res = dispatcher.process_event(&ev_syn);
        match res {
            InputDispatchResult::PointerMove { x, y } => {
                assert!((x - 540.0).abs() < 2.0);
                assert!((y - 1200.0).abs() < 2.0);
            }
            other => panic!("Expected PointerMove, got {:?}", other),
        }

        // 2. Press BTN_TOUCH / BTN_LEFT
        let ev_down = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: BTN_TOUCH,
            value: 1,
        };
        let res_down = dispatcher.process_event(&ev_down);
        match res_down {
            InputDispatchResult::Touch(t) => {
                assert_eq!(t.phase, TouchPhase::Down);
                assert!((t.x - 540.0).abs() < 2.0);
            }
            other => panic!("Expected Touch Down, got {:?}", other),
        }

        // 3. Move while pressed
        let ev_x_move = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_ABS,
            code: ABS_X,
            value: 8191,
        };
        dispatcher.process_event(&ev_x_move);
        let res_move = dispatcher.process_event(&ev_syn);
        match res_move {
            InputDispatchResult::Touch(t) => {
                assert_eq!(t.phase, TouchPhase::Move);
                assert!((t.x - 270.0).abs() < 2.0);
            }
            other => panic!("Expected Touch Move, got {:?}", other),
        }

        // 4. Release
        let ev_up = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: BTN_TOUCH,
            value: 0,
        };
        let res_up = dispatcher.process_event(&ev_up);
        match res_up {
            InputDispatchResult::Touch(t) => {
                assert_eq!(t.phase, TouchPhase::Up);
            }
            other => panic!("Expected Touch Up, got {:?}", other),
        }
    }

    #[test]
    fn test_keyboard_key_decoding() {
        let mut dispatcher = InputDispatcher::new(1080.0, 2400.0);

        let ev_a = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: KEY_A,
            value: 1,
        };
        let res = dispatcher.process_event(&ev_a);
        assert_eq!(
            res,
            InputDispatchResult::KeyPress {
                code: KEY_A,
                ch: Some('a'),
                pressed: true,
                repeat: false,
                ctrl: false,
            }
        );

        // Test Shift + A -> 'A'
        let ev_shift = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: KEY_LEFTSHIFT,
            value: 1,
        };
        dispatcher.process_event(&ev_shift);
        let res_shift_a = dispatcher.process_event(&ev_a);
        assert_eq!(
            res_shift_a,
            InputDispatchResult::KeyPress {
                code: KEY_A,
                ch: Some('A'),
                pressed: true,
                repeat: false,
                ctrl: false,
            }
        );

        // Release Shift
        let ev_shift_up = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: KEY_LEFTSHIFT,
            value: 0,
        };
        dispatcher.process_event(&ev_shift_up);

        // Test Ctrl + C
        let ev_ctrl = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: KEY_LEFTCTRL,
            value: 1,
        };
        dispatcher.process_event(&ev_ctrl);
        let ev_c = LinuxInputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: EV_KEY,
            code: KEY_C,
            value: 1,
        };
        let res_ctrl_c = dispatcher.process_event(&ev_c);
        assert_eq!(
            res_ctrl_c,
            InputDispatchResult::KeyPress {
                code: KEY_C,
                ch: Some('c'),
                pressed: true,
                repeat: false,
                ctrl: true,
            }
        );
    }
}
