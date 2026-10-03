//! Universal Treble Input Subsystem (Phase 5 Milestone 5.2).
//! Integrates Android Input Device Configuration (.idc) file parsing,
//! coordinate/pressure calibration, and Active Stylus (S-Pen / USI digitizer)
//! mapping to Wayland Tablet protocol (zwp_tablet_manager_v2) with palm rejection.

pub mod idc;
pub mod stylus;

pub use idc::{InputDeviceConfig, TouchDeviceType, TouchPressureCalibration, TouchSizeCalibration};
pub use stylus::{
    StylusHandler, TabletEvent, TabletToolType, ABS_DISTANCE, ABS_PRESSURE, ABS_TILT_X, ABS_TILT_Y,
    BTN_STYLUS, BTN_STYLUS2, BTN_TOOL_PEN, BTN_TOOL_RUBBER,
};

/// Full Phase 5 Input Subsystem Bring-Up Status
#[derive(Debug, Clone, PartialEq)]
pub struct InputBringupStatus {
    pub idc_parsed: bool,
    pub stylus_supported: bool,
    pub palm_rejection_active: bool,
}

impl InputBringupStatus {
    pub fn is_ready(&self) -> bool {
        self.idc_parsed && self.stylus_supported
    }
}
