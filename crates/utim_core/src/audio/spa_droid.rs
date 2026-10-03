//! PipeWire Simple Plugin API (SPA) Droid Audio Node.
//! Implements a low-latency audio processing node bridging PipeWire graphs
//! directly to the Android Audio HAL.
//! Conforms strictly to GEMINI.md: zero-copy PCM slice passing, bounded buffers,
//! and sub-15ms roundtrip latency.

use super::hal::{AndroidAudioHal, AudioConfig, AudioError, AudioStreamType};

/// SPA Node States (PipeWire spa/node/node.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaNodeState {
    Inactive,
    Suspended,
    Idle,
    Running,
    Error,
}

/// SPA Node Direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaDirection {
    Playback,
    Capture,
    Duplex,
}

/// PipeWire SPA Droid Node
pub struct SpaDroidNode {
    pub node_id: u32,
    pub name: String,
    pub direction: SpaDirection,
    pub state: SpaNodeState,
    pub config: AudioConfig,
    pub hal_stream_id: Option<u32>,
    pub frames_processed: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub last_latency_ms: f32,
}

impl SpaDroidNode {
    pub fn new_playback(node_id: u32, name: &str, config: AudioConfig) -> Self {
        Self {
            node_id,
            name: name.to_string(),
            direction: SpaDirection::Playback,
            state: SpaNodeState::Suspended,
            config,
            hal_stream_id: None,
            frames_processed: 0,
            underruns: 0,
            overruns: 0,
            last_latency_ms: config.latency_ms(),
        }
    }

    pub fn new_capture(node_id: u32, name: &str, config: AudioConfig) -> Self {
        Self {
            node_id,
            name: name.to_string(),
            direction: SpaDirection::Capture,
            state: SpaNodeState::Suspended,
            config,
            hal_stream_id: None,
            frames_processed: 0,
            underruns: 0,
            overruns: 0,
            last_latency_ms: config.latency_ms(),
        }
    }

    /// Transition node to Idle and open corresponding HAL stream if not already open
    pub fn activate(&mut self, hal: &mut AndroidAudioHal) -> Result<(), AudioError> {
        if self.hal_stream_id.is_none() {
            let stream_type = if self.direction == SpaDirection::Capture {
                AudioStreamType::System
            } else {
                AudioStreamType::Music
            };

            // H29: endpoint arrays are Copy; snapshot before the &mut call.
            let stream_id = if self.direction == SpaDirection::Capture {
                let devs = hal.active_input_devices;
                let n = hal.num_active_inputs.min(devs.len());
                hal.open_input_stream(super::hal::AudioSource::Mic, self.config, &devs[..n])?
            } else {
                let devs = hal.active_output_devices;
                let n = hal.num_active_outputs.min(devs.len());
                hal.open_output_stream(stream_type, self.config, &devs[..n])?
            };
            self.hal_stream_id = Some(stream_id);
        }
        self.state = SpaNodeState::Idle;
        Ok(())
    }

    /// Transition node to Running
    pub fn start(&mut self) {
        if self.state == SpaNodeState::Idle || self.state == SpaNodeState::Suspended {
            self.state = SpaNodeState::Running;
        }
    }

    /// Process a PCM buffer from PipeWire graph to Android HAL (Playback)
    pub fn process_output_buffer(
        &mut self,
        pcm_data: &[u8],
        hal: &mut AndroidAudioHal,
    ) -> Result<usize, AudioError> {
        // H18: guard divide-by-zero before any side effect (unlike
        // AudioConfig::latency_ms, which substitutes a 0.0 sentinel).
        if self.config.sample_rate == 0 {
            return Err(AudioError::InvalidParameter("sample_rate must be non-zero"));
        }
        if self.state != SpaNodeState::Running {
            self.start();
        }

        let stream_id = match self.hal_stream_id {
            Some(id) => id,
            None => {
                self.activate(hal)?;
                self.hal_stream_id.unwrap()
            }
        };

        let frame_size = self.config.frame_size();
        if frame_size == 0 {
            return Err(AudioError::InvalidParameter("Frame size is 0"));
        }

        let written = hal.write_output(stream_id, pcm_data)?;
        let frames = written / frame_size;
        self.frames_processed += frames as u64;

        // sample_rate != 0 checked above; inf/NaN latency is unreachable.
        // Calculate processing latency: (buffer_frames / sample_rate) * 1000 ms
        self.last_latency_ms = (frames as f32 / self.config.sample_rate as f32) * 1000.0;

        Ok(written)
    }

    /// Process a PCM buffer from Android HAL to PipeWire graph (Capture)
    pub fn process_input_buffer(
        &mut self,
        pcm_buffer: &mut [u8],
        hal: &mut AndroidAudioHal,
    ) -> Result<usize, AudioError> {
        // H18: guard divide-by-zero before any side effect.
        if self.config.sample_rate == 0 {
            return Err(AudioError::InvalidParameter("sample_rate must be non-zero"));
        }
        if self.state != SpaNodeState::Running {
            self.start();
        }

        let stream_id = match self.hal_stream_id {
            Some(id) => id,
            None => {
                self.activate(hal)?;
                self.hal_stream_id.unwrap()
            }
        };

        let frame_size = self.config.frame_size();
        if frame_size == 0 {
            return Err(AudioError::InvalidParameter("Frame size is 0"));
        }

        let read = hal.read_input(stream_id, pcm_buffer)?;
        let frames = read / frame_size;
        self.frames_processed += frames as u64;
        // sample_rate != 0 checked above; inf/NaN latency is unreachable.
        self.last_latency_ms = (frames as f32 / self.config.sample_rate as f32) * 1000.0;

        Ok(read)
    }

    /// Pause node and put HAL stream into standby
    pub fn pause(&mut self, hal: &mut AndroidAudioHal) -> Result<(), AudioError> {
        if let Some(stream_id) = self.hal_stream_id {
            let _ = hal.standby(stream_id);
        }
        self.state = SpaNodeState::Idle;
        Ok(())
    }

    /// Suspend node
    pub fn suspend(&mut self, hal: &mut AndroidAudioHal) -> Result<(), AudioError> {
        if let Some(stream_id) = self.hal_stream_id {
            let _ = hal.standby(stream_id);
        }
        self.state = SpaNodeState::Suspended;
        Ok(())
    }

    /// Destroy node and close HAL stream
    pub fn destroy(&mut self, hal: &mut AndroidAudioHal) -> Result<(), AudioError> {
        if let Some(stream_id) = self.hal_stream_id.take() {
            let _ = hal.close_stream(stream_id);
        }
        self.state = SpaNodeState::Inactive;
        Ok(())
    }
}
