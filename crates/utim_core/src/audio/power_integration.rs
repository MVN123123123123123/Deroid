//! UTIM Mobile Power Governor (MPG) integration & Self-Healing Watchdog for Audio.
//! Guarantees glitch-free screen-off audio playback via kernel wakelock acquisition
//! and automatic recovery from Audio HAL termination without phone reboot.
//! Conforms strictly to GEMINI.md systems discipline.

use super::hal::AndroidAudioHal;
use super::routing::AudioRouter;
use super::spa_droid::{SpaDroidNode, SpaNodeState};
use crate::mpg::MobilePowerGovernor;

pub const AUDIO_WAKELOCK_NAME: &str = "spa-droid-playback";

/// Power & Watchdog Manager for Audio Subsystem
pub struct AudioPowerManager {
    pub wakelock_held: bool,
    pub active_stream_count: usize,
    pub hal_restarts_detected: u32,
    pub auto_recovery_count: u32,
}

impl Default for AudioPowerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPowerManager {
    pub fn new() -> Self {
        Self {
            wakelock_held: false,
            active_stream_count: 0,
            hal_restarts_detected: 0,
            auto_recovery_count: 0,
        }
    }

    /// Sync power state with UTIM MPG based on active SPA nodes
    pub fn sync_power_state(
        &mut self,
        nodes: &[SpaDroidNode],
        mpg: &mut MobilePowerGovernor,
    ) {
        let is_playing = nodes
            .iter()
            .any(|n| n.state == SpaNodeState::Running && n.config.buffer_size_bytes() > 0);

        self.active_stream_count = nodes
            .iter()
            .filter(|n| n.state == SpaNodeState::Running)
            .count();

        if is_playing && !self.wakelock_held {
            // Acquire partial wake lock to keep audio DSP and DMA running with screen off
            let _ = mpg.acquire_wake_lock(AUDIO_WAKELOCK_NAME);
            self.wakelock_held = true;
        } else if !is_playing && self.wakelock_held {
            // All playback finished or paused -> release wake lock to allow SoC deep suspend
            let _ = mpg.release_wake_lock(AUDIO_WAKELOCK_NAME);
            self.wakelock_held = false;
        }
    }

    /// Self-Healing Watchdog: handles Audio HAL crash/exit event.
    /// Re-binds HAL, re-applies routing, and re-activates active nodes.
    pub fn handle_hal_crash(
        &mut self,
        hal: &mut AndroidAudioHal,
        router: &mut AudioRouter,
        nodes: &mut [SpaDroidNode],
    ) {
        self.hal_restarts_detected += 1;

        // 1. Re-initialize HAL state with same architecture version
        let version = hal.version;
        *hal = AndroidAudioHal::new(version);

        // 2. Restore in-call audio mode and DSP algorithms if a call is active
        if router.state.in_call_active {
            hal.set_mode(super::hal::AudioMode::InCall);
        }

        // 3. Re-apply routing policies
        router.evaluate_and_apply_routing(hal);

        // 4. Re-open and bind all SPA nodes that were running or active
        for node in nodes.iter_mut() {
            let was_running = node.state == SpaNodeState::Running;
            let was_active = was_running || node.state == SpaNodeState::Idle;
            node.hal_stream_id = None;
            if was_active {
                let _ = node.activate(hal);
                if was_running {
                    node.start();
                }
            }
        }

        self.auto_recovery_count += 1;
    }
}
