//! Android Input Device Configuration (.idc) Parser & Calibrator.
//! Parses vendor IDC configuration files (/vendor/usr/idc/*.idc, /system/usr/idc/*.idc)
//! and applies hardware calibration factors (pressure scale, size scale, orientation)
//! to raw Linux evdev touchscreen coordinates.
//! Conforms strictly to GEMINI.md systems discipline.

use std::collections::HashMap;
use std::fs;

/// Touch Device Type (touch.deviceType)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchDeviceType {
    TouchScreen,
    TouchPad,
    Pointer,
    Default,
}

/// Touch Size Calibration Method (touch.size.calibration)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchSizeCalibration {
    None,
    Geometric,
    Diameter,
    Area,
    Default,
}

/// Touch Pressure Calibration Method (touch.pressure.calibration)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPressureCalibration {
    None,
    Physical,
    Amplitude,
    Default,
}

/// Parsed Input Device Configuration (.idc)
#[derive(Debug, Clone, PartialEq)]
pub struct InputDeviceConfig {
    pub device_type: TouchDeviceType,
    pub orientation_aware: bool,
    pub size_calibration: TouchSizeCalibration,
    pub size_scale: f32,
    pub size_bias: f32,
    pub pressure_calibration: TouchPressureCalibration,
    pub pressure_scale: f32,
    pub cursor_mode_pointer: bool,
    pub is_internal: bool,
    pub raw_properties: HashMap<String, String>,
}

impl Default for InputDeviceConfig {
    fn default() -> Self {
        Self {
            device_type: TouchDeviceType::TouchScreen,
            orientation_aware: true,
            size_calibration: TouchSizeCalibration::Geometric,
            size_scale: 1.0,
            size_bias: 0.0,
            pressure_calibration: TouchPressureCalibration::Physical,
            pressure_scale: 0.00024414, // 1.0 / 4096.0
            cursor_mode_pointer: false,
            is_internal: true,
            raw_properties: HashMap::new(),
        }
    }
}

impl InputDeviceConfig {
    /// Parse a scalar IDC float, accepting a trivial `min:max` range form.
    ///
    /// H31: vendor files occasionally carry ranges (`key = 0.0:1.5`); a
    /// scalar calibration key cannot consume a range, so the max endpoint
    /// (the sensitivity-preserving bound) is used. Plain floats keep the
    /// direct path.
    fn parse_idc_float(val: &str) -> Option<f32> {
        if let Ok(f) = val.parse::<f32>() {
            return Some(f);
        }
        if let Some((_, hi)) = val.split_once(':') {
            if let Ok(f) = hi.trim().parse::<f32>() {
                return Some(f);
            }
        }
        None
    }

    /// Parse IDC file contents from string slice.
    ///
    /// H31: trailing `#` comments are stripped before parsing
    /// (`key = value # comment`); `[section]` headers are skipped; every
    /// `key = value` pair — known or not — is retained verbatim in
    /// `raw_properties`, and unknown keys cause no behavior change.
    pub fn parse(content: &str) -> Self {
        let mut config = Self::default();
        let mut properties = HashMap::new();

        for line in content.lines() {
            // H31: strip trailing comments first; a full-line comment then
            // yields an empty line and is skipped below.
            let code = match line.find('#') {
                Some(idx) => &line[..idx],
                None => line,
            };
            let trimmed = code.trim();
            if trimmed.is_empty() {
                continue;
            }
            // H31: section headers carry no `=` and affect no behavior.
            if trimmed.starts_with('[') {
                continue;
            }

            if let Some((k, v)) = trimmed.split_once('=') {
                let key = k.trim().to_string();
                let val = v.trim().trim_matches('"').trim().to_string();

                match key.as_str() {
                    "touch.deviceType" => {
                        config.device_type = match val.as_str() {
                            "touchScreen" => TouchDeviceType::TouchScreen,
                            "touchPad" => TouchDeviceType::TouchPad,
                            "pointer" => TouchDeviceType::Pointer,
                            _ => TouchDeviceType::Default,
                        };
                    }
                    "touch.orientationAware" => {
                        config.orientation_aware = val == "1" || val.eq_ignore_ascii_case("true");
                    }
                    "touch.size.calibration" => {
                        config.size_calibration = match val.as_str() {
                            "none" => TouchSizeCalibration::None,
                            "geometric" => TouchSizeCalibration::Geometric,
                            "diameter" => TouchSizeCalibration::Diameter,
                            "area" => TouchSizeCalibration::Area,
                            _ => TouchSizeCalibration::Default,
                        };
                    }
                    "touch.size.scale" => {
                        if let Some(f) = Self::parse_idc_float(&val) {
                            config.size_scale = f;
                        }
                    }
                    "touch.size.bias" => {
                        if let Some(f) = Self::parse_idc_float(&val) {
                            config.size_bias = f;
                        }
                    }
                    "touch.pressure.calibration" => {
                        config.pressure_calibration = match val.as_str() {
                            "none" => TouchPressureCalibration::None,
                            "physical" => TouchPressureCalibration::Physical,
                            "amplitude" => TouchPressureCalibration::Amplitude,
                            _ => TouchPressureCalibration::Default,
                        };
                    }
                    "touch.pressure.scale" => {
                        if let Some(f) = Self::parse_idc_float(&val) {
                            config.pressure_scale = f;
                        }
                    }
                    "cursor.mode" => {
                        config.cursor_mode_pointer = val == "pointer";
                    }
                    "device.internal" => {
                        config.is_internal = val == "1" || val.eq_ignore_ascii_case("true");
                    }
                    _ => {}
                }

                properties.insert(key, val);
            }
        }

        config.raw_properties = properties;
        config
    }

    /// Load IDC file from disk, looking in vendor and system directories
    pub fn load_for_device(device_name: &str) -> Self {
        let safe_name = device_name.replace('/', "_");
        let search_paths = [
            format!("/vendor/usr/idc/{}.idc", safe_name),
            format!("/system/usr/idc/{}.idc", safe_name),
            format!("/odm/usr/idc/{}.idc", safe_name),
        ];

        for p in &search_paths {
            if let Ok(content) = fs::read_to_string(p) {
                return Self::parse(&content);
            }
        }

        Self::default()
    }

    /// Apply pressure calibration
    #[inline]
    pub fn calibrate_pressure(&self, raw_pressure: i32) -> f32 {
        match self.pressure_calibration {
            TouchPressureCalibration::None => 1.0,
            TouchPressureCalibration::Physical | TouchPressureCalibration::Amplitude => {
                (raw_pressure as f32 * self.pressure_scale).clamp(0.0, 1.0)
            }
            TouchPressureCalibration::Default => (raw_pressure as f32 / 4096.0).clamp(0.0, 1.0),
        }
    }

    /// Apply touch size calibration.
    ///
    /// H30 (upper clamp REJECTED): only a lower bound is applied. Unlike
    /// pressure — normalized to 0.0..1.0 — size is a physical quantity
    /// (vendor `touch.size.scale`/`bias` map raw sensor units to mm-scale
    /// diameter/area), so legitimate values exceed 1.0 (e.g. 10 * 1.25 +
    /// 2.0 = 14.5 must pass through). An upper clamp would corrupt geometric
    /// calibration; only a negative result from a negative bias is floored.
    #[inline]
    pub fn calibrate_size(&self, raw_size: i32) -> f32 {
        match self.size_calibration {
            TouchSizeCalibration::None => 1.0,
            _ => (raw_size as f32 * self.size_scale + self.size_bias).max(0.0),
        }
    }
}
