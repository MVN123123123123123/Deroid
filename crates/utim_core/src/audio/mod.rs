//! Universal Treble Real-Time Audio Subsystem (Phase 4 Milestone 4.1).
//! Integrates Android Audio HAL (HIDL and AIDL), PipeWire SPA Droid node,
//! dynamic routing engine (Speaker, Earpiece, Headset, Bluetooth A2DP/SCO),
//! Mobile Power Governor screen-off playback synchronization, and self-healing crash watchdog.

pub mod hal;
pub mod power_integration;
pub mod routing;
pub mod spa_droid;

pub use hal::{
    ActiveStream, AndroidAudioHal, AudioChannelMask, AudioConfig, AudioError, AudioFormat,
    AudioHalVersion, AudioInputDevice, AudioMode, AudioOutputDevice, AudioOutputFlags,
    AudioSource, AudioStreamType,
};
pub use power_integration::{AudioPowerManager, AUDIO_WAKELOCK_NAME};
pub use routing::{AudioRouter, AudioRoutingState};
pub use spa_droid::{SpaDirection, SpaDroidNode, SpaNodeState};

/// Full Phase 4 Audio Subsystem Bring-Up Status
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBringupStatus {
    pub hal_version: AudioHalVersion,
    pub routing_state: AudioRoutingState,
    pub active_nodes: usize,
    pub roundtrip_latency_ms: f32,
    pub mpg_wakelock_active: bool,
    pub aec_active: bool,
    pub ns_active: bool,
}

impl AudioBringupStatus {
    pub fn is_ready(&self) -> bool {
        self.roundtrip_latency_ms <= 15.0
    }
}
