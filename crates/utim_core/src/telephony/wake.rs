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
    /// Monotonic tick of the last acquire; the lock expires after TIMEOUT.
    pub alert_tick: u32,
    pub alert_deadline_ticks: u32,
}

impl Default for TelephonyWakeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TelephonyWakeManager {
    /// Ticks of event-loop progress before an unanswered alert expires (H17).
    pub const ALERT_TIMEOUT_TICKS: u32 = 600;

    pub fn new() -> Self {
        Self {
            wake_events_count: 0,
            last_wake_reason: None,
            wake_lock_active: false,
            alert_tick: 0,
            alert_deadline_ticks: 0,
        }
    }

    /// Triggered by kernel modem IRQ or RIL unsol packet during sleep
    pub fn on_modem_irq_event(&mut self, reason: WakeReason, mpg: &mut MobilePowerGovernor) {
        self.wake_events_count += 1;
        self.last_wake_reason = Some(reason);

        // H17: only record the lock when acquire actually succeeded.
        if mpg.acquire_wake_lock(TELEPHONY_WAKE_LOCK).is_ok() {
            self.wake_lock_active = true;
            self.alert_tick = self.alert_tick.wrapping_add(1);
            self.alert_deadline_ticks = self.alert_tick.wrapping_add(Self::ALERT_TIMEOUT_TICKS);
        }
    }

    /// Release wake lock after call answered, rejected, or notification timeout
    pub fn release_alert_lock(&mut self, mpg: &mut MobilePowerGovernor) {
        if self.wake_lock_active && mpg.release_wake_lock(TELEPHONY_WAKE_LOCK).is_ok() {
            self.wake_lock_active = false;
        }
    }

    /// Expire an unanswered alert (call poll_timeout(now_tick) from the loop).
    pub fn poll_timeout(&mut self, now_tick: u32, mpg: &mut MobilePowerGovernor) {
        if self.wake_lock_active
            && now_tick.wrapping_sub(self.alert_tick) >= Self::ALERT_TIMEOUT_TICKS
        {
            self.release_alert_lock(mpg);
            // If release failed, still clear to avoid holding forever; the
            // next IRQ will re-acquire.
            self.wake_lock_active = false;
        }
    }
}
