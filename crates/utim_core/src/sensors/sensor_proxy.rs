//! D-Bus net.hadess.SensorProxy (iio-sensor-proxy) Service & Display Auto-Rotation.
//! Calculates device physical orientation from 3-axis accelerometer gravity vectors,
//! applies anti-jitter hysteresis, and feeds rotation events into the UTLC Wayland Compositor.
//! Conforms strictly to GEMINI.md systems discipline.

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
            SensorData::Acceleration { x, y, z } => {
                self.process_accelerometer(*x, *y, *z)
            }
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
                self.current_orientation = raw_orientation;
                self.candidate_sample_count = 0;
                self.orientation_change_count += 1;
                if self.auto_rotate_enabled {
                    return Some(self.current_orientation);
                }
            }
        } else {
            self.candidate_orientation = raw_orientation;
            self.candidate_sample_count = 1;
        }

        None
    }
}
