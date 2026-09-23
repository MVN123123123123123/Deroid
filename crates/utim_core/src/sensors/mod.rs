//! Universal Treble Sensors & Location Subsystem (Phase 5 Milestone 5.3).
//! Integrates Android Sensors HAL, D-Bus net.hadess.SensorProxy (iio-sensor-proxy)
//! display auto-rotation, and GNSS HAL NMEA-0183 generation for gpsd/geoclue2.

pub mod gnss;
pub mod sensor_hal;
pub mod sensor_proxy;

pub use gnss::{
    calculate_nmea_checksum, GnssConstellation, GnssLocation, GnssService, SatelliteInfo,
};
pub use sensor_hal::{AndroidSensorsHal, SensorData, SensorEvent, SensorInfo, SensorType};
pub use sensor_proxy::{DeviceOrientation, SensorProxyService};

/// Full Phase 5 Sensors Subsystem Bring-Up Status
#[derive(Debug, Clone, PartialEq)]
pub struct SensorsBringupStatus {
    pub has_accelerometer: bool,
    pub has_gyroscope: bool,
    pub has_ambient_light: bool,
    pub has_gnss_fix: bool,
    pub auto_rotation_active: bool,
    pub current_orientation: DeviceOrientation,
}

impl SensorsBringupStatus {
    pub fn is_ready(&self) -> bool {
        self.has_accelerometer && self.auto_rotation_active
    }
}
