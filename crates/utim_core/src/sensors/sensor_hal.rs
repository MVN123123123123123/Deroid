//! Android Sensors HAL Interface (android.hardware.sensors@1.0-2.1 & AIDL ISensors).
//! Handles hardware sensor enumeration, activation, sampling rates, and event streaming.
//! Supports Accelerometer, Gyroscope, Ambient Light, Proximity, and Magnetometer.
//! Conforms strictly to GEMINI.md systems discipline.

/// Standard Android Sensor Types (hardware/sensors.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorType {
    Accelerometer = 1,
    MagneticField = 2,
    Orientation = 3,
    Gyroscope = 4,
    Light = 5,
    Pressure = 6,
    Proximity = 8,
    Gravity = 9,
    LinearAcceleration = 10,
    RotationVector = 11,
}

/// Android Sensor Event Payload
#[derive(Debug, Clone, PartialEq)]
pub enum SensorData {
    /// 3-axis acceleration in m/s^2 (includes gravity)
    Acceleration { x: f32, y: f32, z: f32 },
    /// 3-axis angular velocity in rad/s
    Gyroscope { x: f32, y: f32, z: f32 },
    /// Ambient light level in SI lux units
    Light { lux: f32 },
    /// Proximity distance in centimeters
    Proximity { distance_cm: f32 },
    /// 3-axis geomagnetic field in micro-Tesla (uT)
    MagneticField { x: f32, y: f32, z: f32 },
}

/// Sensor Hardware Event
#[derive(Debug, Clone, PartialEq)]
pub struct SensorEvent {
    pub sensor_type: SensorType,
    pub timestamp_ns: u64,
    pub data: SensorData,
}

/// Sensor Hardware Information
#[derive(Debug, Clone, PartialEq)]
pub struct SensorInfo {
    pub handle: i32,
    pub sensor_type: SensorType,
    pub name: String,
    pub vendor: String,
    pub max_range: f32,
    pub resolution: f32,
    pub min_delay_us: i32,
    pub max_delay_us: i32,
    pub active: bool,
    pub sampling_period_us: i32,
}

/// Android Sensors HAL Bridge
pub struct AndroidSensorsHal {
    pub sensors: Vec<SensorInfo>,
    // H37: one monotonic clock per sensor. A single shared counter made the
    // accelerometer's implied period jump whenever a light/proximity event
    // intervened, corrupting any client's sample-rate derivation.
    accel_ts: u64,
    gyro_ts: u64,
    light_ts: u64,
    prox_ts: u64,
}

impl Default for AndroidSensorsHal {
    fn default() -> Self {
        Self::new()
    }
}

impl AndroidSensorsHal {
    pub fn new() -> Self {
        let mut sensors = Vec::with_capacity(8);

        sensors.push(SensorInfo {
            handle: 1,
            sensor_type: SensorType::Accelerometer,
            name: "LSM6DSO 3-axis Accelerometer".to_string(),
            vendor: "STMicroelectronics".to_string(),
            max_range: 78.45,
            resolution: 0.00239,
            min_delay_us: 4000,   // up to 250 Hz
            max_delay_us: 200000, // 5 Hz
            active: true,
            sampling_period_us: 20000, // 50 Hz default
        });

        sensors.push(SensorInfo {
            handle: 2,
            sensor_type: SensorType::Gyroscope,
            name: "LSM6DSO Gyroscope".to_string(),
            vendor: "STMicroelectronics".to_string(),
            max_range: 34.9,
            resolution: 0.00106,
            min_delay_us: 4000,
            max_delay_us: 200000,
            active: false,
            sampling_period_us: 20000,
        });

        sensors.push(SensorInfo {
            handle: 3,
            sensor_type: SensorType::Light,
            name: "TCS3701 Ambient Light Sensor".to_string(),
            vendor: "AMS".to_string(),
            max_range: 65535.0,
            resolution: 1.0,
            min_delay_us: 50000,
            max_delay_us: 1000000,
            active: true,
            sampling_period_us: 100000, // 10 Hz
        });

        sensors.push(SensorInfo {
            handle: 4,
            sensor_type: SensorType::Proximity,
            name: "TCS3701 Proximity Sensor".to_string(),
            vendor: "AMS".to_string(),
            max_range: 5.0,
            resolution: 1.0,
            min_delay_us: 50000,
            max_delay_us: 1000000,
            active: true,
            sampling_period_us: 100000,
        });

        Self {
            sensors,
            accel_ts: 0,
            gyro_ts: 0,
            light_ts: 0,
            prox_ts: 0,
        }
    }

    /// Activate or deactivate a sensor
    pub fn activate(&mut self, sensor_type: SensorType, enable: bool) -> Result<(), &'static str> {
        let sensor = self
            .sensors
            .iter_mut()
            .find(|s| s.sensor_type == sensor_type)
            .ok_or("Sensor not found")?;
        sensor.active = enable;
        Ok(())
    }

    /// Set sampling period in microseconds
    pub fn set_sampling_period(
        &mut self,
        sensor_type: SensorType,
        period_us: i32,
    ) -> Result<(), &'static str> {
        let sensor = self
            .sensors
            .iter_mut()
            .find(|s| s.sensor_type == sensor_type)
            .ok_or("Sensor not found")?;
        sensor.sampling_period_us = period_us.clamp(sensor.min_delay_us, sensor.max_delay_us);
        Ok(())
    }

    /// Gate helper: sensor must exist and be active (H13).
    #[inline]
    fn gate(&self, t: SensorType) -> Result<(), &'static str> {
        self.sensors
            .iter()
            .find(|s| s.sensor_type == t)
            .filter(|s| s.active)
            .map(|_| ())
            .ok_or("sensor not found or deactivated")
    }

    /// Produce an accelerometer reading
    pub fn produce_accelerometer_event(
        &mut self,
        x: f32,
        y: f32,
        z: f32,
    ) -> Result<SensorEvent, &'static str> {
        self.gate(SensorType::Accelerometer)?;
        self.accel_ts += 20_000_000; // 20 ms
        Ok(SensorEvent {
            sensor_type: SensorType::Accelerometer,
            timestamp_ns: self.accel_ts,
            data: SensorData::Acceleration { x, y, z },
        })
    }

    /// Produce a light sensor reading
    pub fn produce_light_event(&mut self, lux: f32) -> Result<SensorEvent, &'static str> {
        self.gate(SensorType::Light)?;
        self.light_ts += 100_000_000;
        Ok(SensorEvent {
            sensor_type: SensorType::Light,
            timestamp_ns: self.light_ts,
            data: SensorData::Light { lux },
        })
    }

    /// Produce a gyroscope angular velocity reading
    pub fn produce_gyroscope_event(
        &mut self,
        x: f32,
        y: f32,
        z: f32,
    ) -> Result<SensorEvent, &'static str> {
        self.gate(SensorType::Gyroscope)?;
        self.gyro_ts += 20_000_000; // 20 ms (50 Hz)
        Ok(SensorEvent {
            sensor_type: SensorType::Gyroscope,
            timestamp_ns: self.gyro_ts,
            data: SensorData::Gyroscope { x, y, z },
        })
    }

    /// Produce a proximity distance reading
    pub fn produce_proximity_event(
        &mut self,
        distance_cm: f32,
    ) -> Result<SensorEvent, &'static str> {
        self.gate(SensorType::Proximity)?;
        self.prox_ts += 100_000_000; // 100 ms (10 Hz)
        Ok(SensorEvent {
            sensor_type: SensorType::Proximity,
            timestamp_ns: self.prox_ts,
            data: SensorData::Proximity { distance_cm },
        })
    }
}
