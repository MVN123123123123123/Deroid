//! Active Stylus (Samsung S-Pen, USI Digitizer) Handler & Palm Rejection Engine.
//! Translates Linux evdev digitizer events to Wayland Tablet Protocol (zwp_tablet_manager_v2)
//! and provides palm rejection to suppress accidental touches during drawing.
//! Conforms strictly to GEMINI.md systems discipline.

use crate::compositor::input::LinuxInputEvent;

// --- Linux Stylus / Digitizer Evdev Constants ---
pub const BTN_TOOL_PEN: u16 = 0x140;    // 320
pub const BTN_TOOL_RUBBER: u16 = 0x141; // 321
pub const BTN_STYLUS: u16 = 0x14b;     // 331 (Primary barrel button)
pub const BTN_STYLUS2: u16 = 0x14c;    // 332 (Secondary barrel button)

pub const ABS_PRESSURE: u16 = 0x18;    // 24
pub const ABS_DISTANCE: u16 = 0x19;    // 25
pub const ABS_TILT_X: u16 = 0x1a;      // 26
pub const ABS_TILT_Y: u16 = 0x1b;      // 27

/// Wayland Tablet Tool Types (zwp_tablet_tool_v2.type)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabletToolType {
    Pen = 0x8000,
    Eraser = 0x8001,
    Brush = 0x8002,
    Pencil = 0x8003,
    Airbrush = 0x8004,
    Finger = 0x8005,
    Mouse = 0x8006,
    Lens = 0x8007,
}

/// Dispatched Wayland Tablet Protocol Event (zwp_tablet_tool_v2)
#[derive(Debug, Clone, PartialEq)]
pub enum TabletEvent {
    ProximityIn {
        tool_type: TabletToolType,
        x: f32,
        y: f32,
    },
    ProximityOut,
    Down,
    Up,
    Motion {
        x: f32,
        y: f32,
    },
    Pressure {
        normalized: f32, // 0.0 .. 1.0
    },
    Tilt {
        tilt_x: f32, // -90.0 .. +90.0 degrees
        tilt_y: f32,
    },
    Distance {
        distance: u32,
    },
    Button {
        button: u32,
        pressed: bool,
    },
}

/// Stylus Digitizer State & Palm Rejection
pub struct StylusHandler {
    pub screen_width: f32,
    pub screen_height: f32,
    pub digitizer_max_x: f32,
    pub digitizer_max_y: f32,
    pub max_pressure: f32,
    pub in_proximity: bool,
    pub is_down: bool,
    pub current_tool: TabletToolType,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub distance: u32,
    pub barrel_button_pressed: bool,
    pub eraser_button_pressed: bool,
    pub palm_rejection_margin_px: f32,
}

impl Default for StylusHandler {
    fn default() -> Self {
        Self::new(1080.0, 2400.0)
    }
}

impl StylusHandler {
    pub fn new(screen_width: f32, screen_height: f32) -> Self {
        Self {
            screen_width,
            screen_height,
            digitizer_max_x: 32767.0,
            digitizer_max_y: 32767.0,
            max_pressure: 4096.0,
            in_proximity: false,
            is_down: false,
            current_tool: TabletToolType::Pen,
            cursor_x: screen_width / 2.0,
            cursor_y: screen_height / 2.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            distance: 0,
            barrel_button_pressed: false,
            eraser_button_pressed: false,
            palm_rejection_margin_px: 120.0,
        }
    }

    /// Process a raw evdev event from the digitizer device
    pub fn process_event(&mut self, ev: &LinuxInputEvent) -> Option<TabletEvent> {
        match ev.type_ {
            crate::compositor::input::EV_ABS => match ev.code {
                crate::compositor::input::ABS_X => {
                    self.cursor_x = (ev.value as f32 / self.digitizer_max_x.max(1.0)) * self.screen_width;
                    Some(TabletEvent::Motion {
                        x: self.cursor_x,
                        y: self.cursor_y,
                    })
                }
                crate::compositor::input::ABS_Y => {
                    self.cursor_y = (ev.value as f32 / self.digitizer_max_y.max(1.0)) * self.screen_height;
                    Some(TabletEvent::Motion {
                        x: self.cursor_x,
                        y: self.cursor_y,
                    })
                }
                ABS_PRESSURE => {
                    self.pressure = (ev.value as f32 / self.max_pressure.max(1.0)).clamp(0.0, 1.0);
                    Some(TabletEvent::Pressure {
                        normalized: self.pressure,
                    })
                }
                ABS_DISTANCE => {
                    self.distance = ev.value.max(0) as u32;
                    Some(TabletEvent::Distance {
                        distance: self.distance,
                    })
                }
                ABS_TILT_X => {
                    // evdev units are 0.01 degrees; Wayland tilt is degrees.
                    self.tilt_x = (ev.value as f32 * 0.01).clamp(-90.0, 90.0);
                    Some(TabletEvent::Tilt {
                        tilt_x: self.tilt_x,
                        tilt_y: self.tilt_y,
                    })
                }
                ABS_TILT_Y => {
                    self.tilt_y = (ev.value as f32 * 0.01).clamp(-90.0, 90.0);
                    Some(TabletEvent::Tilt {
                        tilt_x: self.tilt_x,
                        tilt_y: self.tilt_y,
                    })
                }
                _ => None,
            },
            crate::compositor::input::EV_KEY => match ev.code {
                BTN_TOOL_PEN => {
                    if ev.value == 1 {
                        self.in_proximity = true;
                        self.current_tool = TabletToolType::Pen;
                        Some(TabletEvent::ProximityIn {
                            tool_type: TabletToolType::Pen,
                            x: self.cursor_x,
                            y: self.cursor_y,
                        })
                    } else {
                        self.in_proximity = false;
                        self.is_down = false;
                        Some(TabletEvent::ProximityOut)
                    }
                }
                BTN_TOOL_RUBBER => {
                    if ev.value == 1 {
                        self.in_proximity = true;
                        self.current_tool = TabletToolType::Eraser;
                        Some(TabletEvent::ProximityIn {
                            tool_type: TabletToolType::Eraser,
                            x: self.cursor_x,
                            y: self.cursor_y,
                        })
                    } else {
                        self.in_proximity = false;
                        self.is_down = false;
                        Some(TabletEvent::ProximityOut)
                    }
                }
                crate::compositor::input::BTN_TOUCH => {
                    if ev.value == 1 {
                        self.is_down = true;
                        Some(TabletEvent::Down)
                    } else {
                        self.is_down = false;
                        Some(TabletEvent::Up)
                    }
                }
                BTN_STYLUS => {
                    self.barrel_button_pressed = ev.value == 1;
                    Some(TabletEvent::Button {
                        button: 1,
                        pressed: self.barrel_button_pressed,
                    })
                }
                BTN_STYLUS2 => {
                    self.eraser_button_pressed = ev.value == 1;
                    Some(TabletEvent::Button {
                        button: 2,
                        pressed: self.eraser_button_pressed,
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Palm Rejection Filter: returns true if a touch event at (x, y) should be rejected
    /// because the user's hand/palm is resting on the screen while the pen is in proximity.
    #[inline]
    pub fn should_reject_touch(&self, touch_x: f32, touch_y: f32) -> bool {
        if !self.in_proximity {
            return false;
        }

        // Calculate Euclidean distance from stylus tip to touch contact
        let dx = touch_x - self.cursor_x;
        let dy = touch_y - self.cursor_y;
        let dist = (dx * dx + dy * dy).sqrt();

        // A palm is the broad contact away from the tip; keep the pen's own
        // contact and reject touches at/inside the margin. Never gate on
        // is_down (that would disable all multi-touch for every stroke).
        dist <= self.palm_rejection_margin_px
    }
}
