//! Modem Suspend & Deep Sleep Wake Alert Manager.
//! Wakes up the system from deep suspend (MPG sleep) upon incoming
//! cellular calls or SMS messages via modem IRQ emulation.
//! Conforms strictly to GEMINI.md systems discipline.

use crate::mpg::MobilePowerGovernor;

pub const TELEPHONY_WAKE_LOCK: &str = "telephony-incoming-alert";

/// Telephony Wake Alert Reason
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    IncomingCall,
    IncomingSms,
    EmergencyAlert,
}

/// Telephony Wake Manager
pub struct TelephonyWakeManager {
    pub wake_events_count: u32,
    pub last_wake_reason: Option<WakeReason>,
    pub wake_lock_active: bool,
}

impl Default for TelephonyWakeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TelephonyWakeManager {
    pub fn new() -> Self {
        Self {
            wake_events_count: 0,
            last_wake_reason: None,
            wake_lock_active: false,
        }
    }

    /// Triggered by kernel modem IRQ or RIL unsol packet during sleep
    pub fn on_modem_irq_event(
        &mut self,
        reason: WakeReason,
        mpg: &mut MobilePowerGovernor,
    ) {
        self.wake_events_count += 1;
        self.last_wake_reason = Some(reason);

        // Acquire wake lock to keep phone awake while ringing or notifying
        let _ = mpg.acquire_wake_lock(TELEPHONY_WAKE_LOCK);
        self.wake_lock_active = true;
    }

    /// Release wake lock after call answered, rejected, or notification timeout
    pub fn release_alert_lock(&mut self, mpg: &mut MobilePowerGovernor) {
        if self.wake_lock_active {
            let _ = mpg.release_wake_lock(TELEPHONY_WAKE_LOCK);
            self.wake_lock_active = false;
        }
    }
}
