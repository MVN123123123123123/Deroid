//! Dynamic audio routing policy engine for Android Treble GSI.
//! Implements automatic endpoint switching between Speaker, Earpiece,
//! 3.5mm Wired Headset, and Bluetooth (A2DP / SCO).
//! Follows GEMINI.md: zero redundant dependencies, predictable deterministic state transitions.

use super::hal::{
    AndroidAudioHal, AudioInputDevice, AudioMode, AudioOutputDevice,
};

/// Dynamic audio routing state
#[derive(Debug, Clone, PartialEq)]
pub struct AudioRoutingState {
    pub current_output: AudioOutputDevice,
    pub current_input: AudioInputDevice,
    pub wired_headset_connected: bool,
    pub wired_headset_has_mic: bool,
    pub bluetooth_a2dp_connected: bool,
    pub bluetooth_sco_connected: bool,
    pub in_call_active: bool,
    pub speakerphone_forced: bool,
}

impl Default for AudioRoutingState {
    fn default() -> Self {
        Self {
            current_output: AudioOutputDevice::Speaker,
            current_input: AudioInputDevice::BuiltinMic,
            wired_headset_connected: false,
            wired_headset_has_mic: false,
            bluetooth_a2dp_connected: false,
            bluetooth_sco_connected: false,
            in_call_active: false,
            speakerphone_forced: false,
        }
    }
}

/// Dynamic Audio Router
pub struct AudioRouter {
    pub state: AudioRoutingState,
}

impl Default for AudioRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRouter {
    pub fn new() -> Self {
        Self {
            state: AudioRoutingState::default(),
        }
    }

    /// Update wired headset connection state
    pub fn on_wired_headset_event(
        &mut self,
        connected: bool,
        has_mic: bool,
        hal: &mut AndroidAudioHal,
    ) {
        self.state.wired_headset_connected = connected;
        self.state.wired_headset_has_mic = has_mic;
        self.evaluate_and_apply_routing(hal);
    }

    /// Update Bluetooth connection state
    pub fn on_bluetooth_a2dp_event(&mut self, connected: bool, hal: &mut AndroidAudioHal) {
        self.state.bluetooth_a2dp_connected = connected;
        self.evaluate_and_apply_routing(hal);
    }

    /// Update Bluetooth SCO connection state
    pub fn on_bluetooth_sco_event(&mut self, connected: bool, hal: &mut AndroidAudioHal) {
        self.state.bluetooth_sco_connected = connected;
        self.evaluate_and_apply_routing(hal);
    }

    /// Call state change handler
    pub fn on_call_state_changed(&mut self, in_call: bool, hal: &mut AndroidAudioHal) {
        self.state.in_call_active = in_call;
        if in_call {
            hal.set_mode(AudioMode::InCall);
        } else {
            hal.set_mode(AudioMode::Normal);
            self.state.speakerphone_forced = false;
        }
        self.evaluate_and_apply_routing(hal);
    }

    /// Force speakerphone mode during active phone call
    pub fn set_speakerphone(&mut self, forced: bool, hal: &mut AndroidAudioHal) {
        self.state.speakerphone_forced = forced;
        self.evaluate_and_apply_routing(hal);
    }

    /// Re-evaluates routing based on priority hierarchy:
    /// Output Priority:
    /// 1. Speakerphone (if forced on during call) -> Speaker
    /// 2. Bluetooth SCO (if in-call & connected) -> BluetoothSco
    /// 3. Bluetooth A2DP (if media & connected) -> BluetoothA2dp
    /// 4. Wired Headset (if connected) -> WiredHeadset / WiredHeadphone
    /// 5. In-Call default -> Earpiece
    /// 6. Default media -> Speaker
    pub fn evaluate_and_apply_routing(&mut self, hal: &mut AndroidAudioHal) {
        let (output, input) = if self.state.in_call_active {
            if self.state.speakerphone_forced {
                (AudioOutputDevice::Speaker, AudioInputDevice::BuiltinMic)
            } else if self.state.bluetooth_sco_connected {
                (
                    AudioOutputDevice::BluetoothSco,
                    AudioInputDevice::BluetoothScoHeadset,
                )
            } else if self.state.wired_headset_connected {
                let in_dev = if self.state.wired_headset_has_mic {
                    AudioInputDevice::WiredHeadset
                } else {
                    AudioInputDevice::BuiltinMic
                };
                (AudioOutputDevice::WiredHeadset, in_dev)
            } else {
                (AudioOutputDevice::Earpiece, AudioInputDevice::BuiltinMic)
            }
        } else if self.state.wired_headset_connected {
            let in_dev = if self.state.wired_headset_has_mic {
                AudioInputDevice::WiredHeadset
            } else {
                AudioInputDevice::BuiltinMic
            };
            (AudioOutputDevice::WiredHeadset, in_dev)
        } else if self.state.bluetooth_a2dp_connected {
            (
                AudioOutputDevice::BluetoothA2dp,
                AudioInputDevice::BuiltinMic,
            )
        } else {
            (AudioOutputDevice::Speaker, AudioInputDevice::BuiltinMic)
        };

        self.state.current_output = output;
        self.state.current_input = input;

        hal.active_output_devices = vec![output];
        hal.active_input_devices = vec![input];

        // Apply route to all active streams in HAL
        for stream in hal.streams.iter_mut() {
            if stream.is_input {
                stream.input_devices = vec![input];
            } else {
                stream.output_devices = vec![output];
            }
        }

        // Set low-level vendor mixer parameters
        let route_str = match output {
            AudioOutputDevice::Speaker => "speaker",
            AudioOutputDevice::Earpiece => "earpiece",
            AudioOutputDevice::WiredHeadset | AudioOutputDevice::WiredHeadphone => "headset",
            AudioOutputDevice::BluetoothA2dp => "bt_a2dp",
            AudioOutputDevice::BluetoothSco => "bt_sco",
            _ => "default",
        };
        hal.set_parameters("routing", route_str);
    }
}
