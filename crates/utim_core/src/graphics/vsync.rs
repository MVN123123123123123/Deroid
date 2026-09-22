//! Hardware VSYNC interrupt handling, timing synchronization, and tear-free verification.
//! Supports 60Hz, 90Hz, 120Hz, 144Hz high-refresh-rate displays and LTPO dynamic refresh rates.

use std::fmt;

pub const STANDARD_REFRESH_RATES: [f64; 4] = [60.0, 90.0, 120.0, 144.0];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VsyncConfig {
    pub refresh_rate_hz: f64,
    pub period_ns: u64,
}

impl VsyncConfig {
    pub fn new(refresh_rate_hz: f64) -> Result<Self, VsyncError> {
        if !refresh_rate_hz.is_finite() || refresh_rate_hz <= 0.0 || refresh_rate_hz > 480.0 {
            return Err(VsyncError::InvalidRefreshRate(refresh_rate_hz));
        }
        let period_ns = (1_000_000_000.0 / refresh_rate_hz).round() as u64;
        if period_ns == 0 {
            return Err(VsyncError::InvalidRefreshRate(refresh_rate_hz));
        }
        Ok(Self {
            refresh_rate_hz,
            period_ns,
        })
    }

    pub fn for_rate_60hz() -> Self {
        Self::new(60.0).unwrap()
    }

    pub fn for_rate_90hz() -> Self {
        Self::new(90.0).unwrap()
    }

    pub fn for_rate_120hz() -> Self {
        Self::new(120.0).unwrap()
    }

    pub fn for_rate_144hz() -> Self {
        Self::new(144.0).unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsyncEvent {
    pub display_id: u32,
    pub timestamp_ns: u64,
    pub sequence: u64,
    pub period_ns: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VsyncError {
    InvalidRefreshRate(f64),
    DisplayNotEnabled(u32),
    MissedVsync {
        expected_ns: u64,
        actual_ns: u64,
        delta_ns: i64,
    },
    ScreenTearingDetected {
        frame_seq: u64,
        skew_ns: i64,
        max_allowed_skew_ns: i64,
    },
}

impl fmt::Display for VsyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VsyncError::InvalidRefreshRate(r) => write!(f, "Invalid refresh rate: {} Hz", r),
            VsyncError::DisplayNotEnabled(id) => write!(f, "Display {} VSYNC is not enabled", id),
            VsyncError::MissedVsync {
                expected_ns,
                actual_ns,
                delta_ns,
            } => write!(
                f,
                "Missed VSYNC: expected {} ns, actual {} ns (delta {} ns)",
                expected_ns, actual_ns, delta_ns
            ),
            VsyncError::ScreenTearingDetected {
                frame_seq,
                skew_ns,
                max_allowed_skew_ns,
            } => write!(
                f,
                "Screen tearing risk detected at frame {}: presentation skew {} ns exceeds tolerance {} ns",
                frame_seq, skew_ns, max_allowed_skew_ns
            ),
        }
    }
}

impl std::error::Error for VsyncError {}

/// VSYNC controller coordinating hardware VSYNC interrupts and presentation timing.
pub struct VsyncController {
    display_id: u32,
    enabled: bool,
    config: VsyncConfig,
    current_sequence: u64,
    last_vsync_ns: u64,
}

impl VsyncController {
    pub fn new(display_id: u32, refresh_rate_hz: f64) -> Result<Self, VsyncError> {
        let config = VsyncConfig::new(refresh_rate_hz)?;
        Ok(Self {
            display_id,
            enabled: false,
            config,
            current_sequence: 0,
            last_vsync_ns: 0,
        })
    }

    pub fn display_id(&self) -> u32 {
        self.display_id
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.last_vsync_ns = 0;
        }
    }

    pub fn config(&self) -> VsyncConfig {
        self.config
    }

    /// Dynamically adjust refresh rate (e.g. for LTPO 60 <-> 90 <-> 120 Hz transition).
    pub fn set_refresh_rate(&mut self, refresh_rate_hz: f64) -> Result<(), VsyncError> {
        self.config = VsyncConfig::new(refresh_rate_hz)?;
        Ok(())
    }

    /// Simulate or record incoming hardware VSYNC interrupt from panel.
    pub fn on_hardware_vsync(&mut self, timestamp_ns: u64) -> Option<VsyncEvent> {
        if !self.enabled {
            return None;
        }

        self.current_sequence = self.current_sequence.wrapping_add(1);
        self.last_vsync_ns = timestamp_ns;

        Some(VsyncEvent {
            display_id: self.display_id,
            timestamp_ns,
            sequence: self.current_sequence,
            period_ns: self.config.period_ns,
        })
    }

    /// Predict next VSYNC pulse given a current monotonic timestamp.
    pub fn next_vsync_timestamp(&self, now_ns: u64) -> u64 {
        if self.last_vsync_ns == 0 || now_ns <= self.last_vsync_ns {
            now_ns + self.config.period_ns
        } else {
            let elapsed = now_ns - self.last_vsync_ns;
            let count = (elapsed / self.config.period_ns) + 1;
            self.last_vsync_ns + (count * self.config.period_ns)
        }
    }
}

/// Validator verifying that frame presentation timestamps align with VSYNC without tearing or jitter.
#[derive(Debug, Clone)]
pub struct VsyncPresentationValidator {
    config: VsyncConfig,
    last_present_ns: u64,
    presented_frames: u64,
    max_jitter_ns: u64,
    total_jitter_ns: u64,
}

impl VsyncPresentationValidator {
    pub fn new(config: VsyncConfig) -> Self {
        Self {
            config,
            last_present_ns: 0,
            presented_frames: 0,
            max_jitter_ns: 0,
            total_jitter_ns: 0,
        }
    }

    pub fn config(&self) -> VsyncConfig {
        self.config
    }

    pub fn set_config(&mut self, config: VsyncConfig) {
        self.config = config;
    }

    pub fn set_refresh_rate(&mut self, refresh_rate_hz: f64) -> Result<(), VsyncError> {
        self.config = VsyncConfig::new(refresh_rate_hz)?;
        Ok(())
    }

    /// Validate presentation of a new frame.
    /// Returns Ok(()) if the frame was delivered within the safe VBLANK window.
    pub fn validate_frame_presentation(
        &mut self,
        vsync_timestamp_ns: u64,
        present_timestamp_ns: u64,
    ) -> Result<(), VsyncError> {
        // Tolerable presentation delay relative to VSYNC pulse:
        // Frame must be presented within 15% of the VSYNC period
        let max_tolerable_skew_ns = (self.config.period_ns as f64 * 0.15) as i64;
        let diff = (present_timestamp_ns as i128) - (vsync_timestamp_ns as i128);

        if diff.abs() > (max_tolerable_skew_ns as i128) {
            let clamped_skew = diff.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            return Err(VsyncError::ScreenTearingDetected {
                frame_seq: self.presented_frames + 1,
                skew_ns: clamped_skew,
                max_allowed_skew_ns: max_tolerable_skew_ns,
            });
        }

        if self.last_present_ns > 0 {
            if present_timestamp_ns < self.last_present_ns {
                let clamped_skew = ((present_timestamp_ns as i128) - (self.last_present_ns as i128))
                    .clamp(i64::MIN as i128, i64::MAX as i128)
                    as i64;
                return Err(VsyncError::ScreenTearingDetected {
                    frame_seq: self.presented_frames + 1,
                    skew_ns: clamped_skew,
                    max_allowed_skew_ns: max_tolerable_skew_ns,
                });
            }
            let delta = present_timestamp_ns - self.last_present_ns;
            let expected = self.config.period_ns;
            let jitter = delta.abs_diff(expected);

            if jitter > self.max_jitter_ns {
                self.max_jitter_ns = jitter;
            }
            self.total_jitter_ns += jitter;
        }

        self.last_present_ns = present_timestamp_ns;
        self.presented_frames += 1;
        Ok(())
    }

    pub fn average_jitter_ns(&self) -> u64 {
        if self.presented_frames <= 1 {
            0
        } else {
            self.total_jitter_ns / (self.presented_frames - 1)
        }
    }

    pub fn max_jitter_ns(&self) -> u64 {
        self.max_jitter_ns
    }

    pub fn presented_frames(&self) -> u64 {
        self.presented_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vsync_config_periods() {
        let c60 = VsyncConfig::for_rate_60hz();
        assert_eq!(c60.period_ns, 16_666_667); // ~16.66 ms

        let c90 = VsyncConfig::for_rate_90hz();
        assert_eq!(c90.period_ns, 11_111_111); // ~11.11 ms

        let c120 = VsyncConfig::for_rate_120hz();
        assert_eq!(c120.period_ns, 8_333_333); // ~8.33 ms

        let c144 = VsyncConfig::for_rate_144hz();
        assert_eq!(c144.period_ns, 6_944_444); // ~6.94 ms
    }

    #[test]
    fn test_vsync_controller_events() {
        let mut ctrl = VsyncController::new(0, 120.0).expect("Create 120Hz VsyncController");
        assert!(!ctrl.is_enabled());

        // Disabled -> no events
        assert!(ctrl.on_hardware_vsync(1_000_000).is_none());

        ctrl.set_enabled(true);
        assert!(ctrl.is_enabled());

        let evt1 = ctrl.on_hardware_vsync(1_000_000).expect("Event 1");
        assert_eq!(evt1.display_id, 0);
        assert_eq!(evt1.sequence, 1);
        assert_eq!(evt1.timestamp_ns, 1_000_000);
        assert_eq!(evt1.period_ns, 8_333_333);

        let evt2 = ctrl
            .on_hardware_vsync(1_000_000 + 8_333_333)
            .expect("Event 2");
        assert_eq!(evt2.sequence, 2);
    }

    #[test]
    fn test_tear_free_presentation_verification_all_rates() {
        for &rate in &STANDARD_REFRESH_RATES {
            let config = VsyncConfig::new(rate).unwrap();
            let mut validator = VsyncPresentationValidator::new(config);

            let mut current_vsync = 1_000_000_000u64; // 1 second
            for _ in 0..120 {
                // Perfect presentation aligned within 50 microseconds
                let presentation = current_vsync + 20_000;
                validator
                    .validate_frame_presentation(current_vsync, presentation)
                    .expect("Tear-free frame presentation");
                current_vsync += config.period_ns;
            }

            assert_eq!(validator.presented_frames(), 120);
            assert!(validator.max_jitter_ns() <= 50_000);
        }
    }

    #[test]
    fn test_tearing_detection_on_excessive_skew() {
        let config = VsyncConfig::for_rate_60hz();
        let mut validator = VsyncPresentationValidator::new(config);

        let vsync_ns = 1_000_000_000u64;
        // Exceed 15% tolerance (period is 16.66ms, 15% is 2.5ms; skew by 4ms)
        let presentation = vsync_ns + 4_000_000;

        let err = validator
            .validate_frame_presentation(vsync_ns, presentation)
            .unwrap_err();
        match err {
            VsyncError::ScreenTearingDetected { skew_ns, .. } => {
                assert_eq!(skew_ns, 4_000_000);
            }
            _ => panic!("Expected ScreenTearingDetected error"),
        }
    }

    #[test]
    fn test_vsync_large_timestamp_skew_and_monotonicity() {
        let config = VsyncConfig::for_rate_60hz();
        let mut validator = VsyncPresentationValidator::new(config);

        // Test extreme skew (u64::MAX) - must NOT overflow into a small number and must be rejected!
        let err_extreme = validator.validate_frame_presentation(0, u64::MAX);
        assert!(err_extreme.is_err());

        // Validate a good frame at 1s
        validator
            .validate_frame_presentation(1_000_000_000, 1_000_010_000)
            .unwrap();

        // Second frame presented with timestamp LESS than previous (monotonicity violation)
        let err_backwards = validator.validate_frame_presentation(1_016_666_667, 900_000_000);
        assert!(
            err_backwards.is_err(),
            "Out-of-order frame presentation must be rejected"
        );
    }
}
