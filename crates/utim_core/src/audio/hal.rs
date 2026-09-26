//! Android Audio HAL interface supporting HIDL (android.hardware.audio@2.0-7.1)
//! and AIDL (android.hardware.audio.core.IModule).
//! Adheres strictly to GEMINI.md: zero redundant dependencies, bounded buffers,
//! and minimal allocations.

use std::path::Path;

/// Android Audio HAL architecture version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioHalVersion {
    Hidl { major: u8, minor: u8 },
    Aidl { version: u32 },
}

impl AudioHalVersion {
    pub const HIDL_7_1: Self = Self::Hidl { major: 7, minor: 1 };
    pub const HIDL_2_0: Self = Self::Hidl { major: 2, minor: 0 };
    pub const AIDL_V1: Self = Self::Aidl { version: 1 };
    pub const AIDL_V3: Self = Self::Aidl { version: 3 };

    #[inline]
    pub fn is_aidl(&self) -> bool {
        matches!(self, Self::Aidl { .. })
    }

    pub fn service_name(&self) -> &'static str {
        // H21: resolve from the version fields so HIDL_2_0 does not report
        // itself as @7.0. AIDL instance names carry no version by design.
        match *self {
            Self::Hidl {
                major: 2,
                minor: 0,
            } => "android.hardware.audio@2.0::IDevicesFactory",
            Self::Hidl {
                major: 7,
                minor: 1,
            } => "android.hardware.audio@7.1::IDevicesFactory",
            Self::Hidl { .. } => "android.hardware.audio@7.0::IDevicesFactory",
            Self::Aidl { .. } => "android.hardware.audio.core.IModule/default",
        }
    }

    pub fn binder_device(&self) -> &'static str {
        match self {
            Self::Hidl { .. } => "/dev/hwbinder",
            Self::Aidl { .. } => "/dev/binder",
        }
    }
}

/// Standard Android Audio stream types (system/audio.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioStreamType {
    VoiceCall = 0,
    System = 1,
    Ring = 2,
    Music = 3,
    Alarm = 4,
    Notification = 5,
    BluetoothSco = 6,
    EnforcedAudible = 7,
    Dtmf = 8,
    Tts = 9,
    Accessibility = 10,
}

/// Audio usage modes (system/audio.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    Normal = 0,
    Ringtone = 1,
    InCall = 2,
    InCommunication = 3,
    CallScreen = 4,
}

/// Audio input capture sources (system/audio.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioSource {
    Default = 0,
    Mic = 1,
    VoiceUplink = 2,
    VoiceDownlink = 3,
    VoiceCall = 4,
    Camcorder = 5,
    VoiceRecognition = 6,
    VoiceCommunication = 7,
    RemoteSubmix = 8,
    Unprocessed = 9,
}

/// Hardware audio output device endpoints
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioOutputDevice {
    Earpiece,
    Speaker,
    WiredHeadset,
    WiredHeadphone,
    BluetoothSco,
    BluetoothA2dp,
    BluetoothA2dpHeadphones,
    BluetoothA2dpSpeaker,
    AuxDigital,
    UsbDevice,
    TelephonyTx,
}

/// Hardware audio input device endpoints
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioInputDevice {
    BuiltinMic,
    BackMic,
    BluetoothScoHeadset,
    WiredHeadset,
    AuxDigital,
    VoiceCall,
    TelephonyRx,
}

/// PCM Audio sample format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Pcm16Bit,
    Pcm8Bit,
    Pcm32Bit,
    PcmFloat,
}

impl AudioFormat {
    #[inline]
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            Self::Pcm8Bit => 1,
            Self::Pcm16Bit => 2,
            Self::Pcm32Bit | Self::PcmFloat => 4,
        }
    }
}

/// Audio channel configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioChannelMask {
    Mono = 1,
    Stereo = 2,
    Quad = 4,
    Surround51 = 6,
    Surround71 = 8,
}

impl AudioChannelMask {
    #[inline]
    pub fn channels(&self) -> u32 {
        *self as u32
    }
}

/// Output stream behavior flags
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioOutputFlags {
    pub direct: bool,
    pub primary: bool,
    pub fast: bool,
    pub deep_buffer: bool,
    pub compress_offload: bool,
    pub raw: bool,
    pub voip_rx: bool,
}

/// Hardware stream audio configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channel_mask: AudioChannelMask,
    pub format: AudioFormat,
    pub period_frames: u32,
}

impl AudioConfig {
    pub fn standard_mobile_output() -> Self {
        Self {
            sample_rate: 48000,
            channel_mask: AudioChannelMask::Stereo,
            format: AudioFormat::Pcm16Bit,
            period_frames: 240, // 5.0 ms at 48kHz
        }
    }

    pub fn low_latency_output() -> Self {
        Self {
            sample_rate: 48000,
            channel_mask: AudioChannelMask::Stereo,
            format: AudioFormat::Pcm16Bit,
            period_frames: 192, // 4.0 ms at 48kHz
        }
    }

    pub fn voice_call_config() -> Self {
        Self {
            sample_rate: 16000, // Wideband voice (AMR-WB)
            channel_mask: AudioChannelMask::Mono,
            format: AudioFormat::Pcm16Bit,
            period_frames: 160, // 10.0 ms at 16kHz
        }
    }

    pub fn frame_size(&self) -> usize {
        self.channel_mask.channels() as usize * self.format.bytes_per_sample()
    }

    pub fn buffer_size_bytes(&self) -> usize {
        self.period_frames as usize * self.frame_size()
    }

    pub fn latency_ms(&self) -> f32 {
        if self.sample_rate == 0 {
            0.0
        } else {
            (self.period_frames as f32 / self.sample_rate as f32) * 1000.0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    HalNotAvailable,
    InvalidParameter(&'static str),
    StreamNotFound(u32),
    DeviceNotSupported,
    AlreadyInStandby,
    IoFailure,
    BufferOverflow,
}

/// Number of i16 PCM samples in an [`ActiveStream`] ring (8 KiB, stack).
pub const AUDIO_RING_SAMPLES: usize = 4096;
/// Maximum device endpoints tracked per stream / HAL direction (H29: fixed
/// arrays, no `vec![]` on the routing path).
pub const AUDIO_MAX_DEVICES: usize = 4;

/// Active audio stream descriptor.
///
/// H2: the PCM data path is a bounded stack ring (`ring`, 8 KiB, no heap on
/// the audio hot path). `write_output` pushes little-endian i16 samples and,
/// on a full ring, returns a short-write `Ok(consumed)` instead of inventing
/// success; `read_input` never fabricates silence and fails fast with
/// `Err(AudioError::IoFailure)` when no capture hardware is bound.
#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub id: u32,
    pub stream_type: AudioStreamType,
    pub config: AudioConfig,
    pub output_devices: [AudioOutputDevice; AUDIO_MAX_DEVICES],
    pub num_output_devices: usize,
    pub input_devices: [AudioInputDevice; AUDIO_MAX_DEVICES],
    pub num_input_devices: usize,
    pub is_input: bool,
    pub in_standby: bool,
    pub volume_left: f32,
    pub volume_right: f32,
    pub frames_processed: u64,
    /// H2: fixed-capacity PCM ring. Producer (`write_output`) advances
    /// `ring_head`; the HW drain advances `ring_tail`.
    pub ring: [i16; AUDIO_RING_SAMPLES],
    pub ring_head: usize,
    pub ring_tail: usize,
    pub ring_fill: usize,
}

/// Android Audio Hardware Abstraction Layer
pub struct AndroidAudioHal {
    pub version: AudioHalVersion,
    pub mode: AudioMode,
    pub master_volume: f32,
    pub master_muted: bool,
    pub active_output_devices: [AudioOutputDevice; AUDIO_MAX_DEVICES],
    pub num_active_outputs: usize,
    pub active_input_devices: [AudioInputDevice; AUDIO_MAX_DEVICES],
    pub num_active_inputs: usize,
    pub streams: Vec<ActiveStream>,
    next_stream_id: u32,
    parameters: Vec<(String, String)>,
    pub aec_enabled: bool,
    pub ns_enabled: bool,
}

impl Default for AndroidAudioHal {
    fn default() -> Self {
        Self::new(AudioHalVersion::AIDL_V1)
    }
}

impl AndroidAudioHal {
    pub fn new(version: AudioHalVersion) -> Self {
        Self {
            version,
            mode: AudioMode::Normal,
            master_volume: 1.0,
            master_muted: false,
            active_output_devices: [AudioOutputDevice::Speaker; AUDIO_MAX_DEVICES],
            num_active_outputs: 1,
            active_input_devices: [AudioInputDevice::BuiltinMic; AUDIO_MAX_DEVICES],
            num_active_inputs: 1,
            streams: Vec::with_capacity(8),
            next_stream_id: 1,
            parameters: Vec::with_capacity(16),
            aec_enabled: false,
            ns_enabled: false,
        }
    }

    /// Auto-detect Android Audio HAL version from system files
    pub fn detect() -> Self {
        if Path::new("/vendor/bin/hw/android.hardware.audio.service.aidl").exists()
            || Path::new("/vendor/etc/vintf/manifest.xml").exists()
                && std::fs::read_to_string("/vendor/etc/vintf/manifest.xml")
                    .map(|s| s.contains("android.hardware.audio.core.IModule"))
                    .unwrap_or(false)
        {
            Self::new(AudioHalVersion::AIDL_V1)
        } else {
            Self::new(AudioHalVersion::HIDL_7_1)
        }
    }

    pub fn set_mode(&mut self, mode: AudioMode) {
        self.mode = mode;
        match mode {
            AudioMode::InCall | AudioMode::InCommunication => {
                self.aec_enabled = true;
                self.ns_enabled = true;
            }
            _ => {
                self.aec_enabled = false;
                self.ns_enabled = false;
            }
        }
    }

    pub fn open_output_stream(
        &mut self,
        stream_type: AudioStreamType,
        config: AudioConfig,
        devices: &[AudioOutputDevice],
    ) -> Result<u32, AudioError> {
        let id = self.next_stream_id;
        self.next_stream_id += 1;

        // H29: copy into the fixed endpoint array (truncated); no heap.
        let mut output_devices = [AudioOutputDevice::Speaker; AUDIO_MAX_DEVICES];
        let n = devices.len().min(AUDIO_MAX_DEVICES);
        output_devices[..n].copy_from_slice(&devices[..n]);

        let stream = ActiveStream {
            id,
            stream_type,
            config,
            output_devices,
            num_output_devices: n,
            input_devices: [AudioInputDevice::BuiltinMic; AUDIO_MAX_DEVICES],
            num_input_devices: 0,
            is_input: false,
            in_standby: false,
            volume_left: 1.0,
            volume_right: 1.0,
            frames_processed: 0,
            ring: [0; AUDIO_RING_SAMPLES],
            ring_head: 0,
            ring_tail: 0,
            ring_fill: 0,
        };

        self.streams.push(stream);
        Ok(id)
    }

    pub fn open_input_stream(
        &mut self,
        source: AudioSource,
        config: AudioConfig,
        devices: &[AudioInputDevice],
    ) -> Result<u32, AudioError> {
        let id = self.next_stream_id;
        self.next_stream_id += 1;

        let mut input_devices = [AudioInputDevice::BuiltinMic; AUDIO_MAX_DEVICES];
        let n = devices.len().min(AUDIO_MAX_DEVICES);
        input_devices[..n].copy_from_slice(&devices[..n]);

        let stream = ActiveStream {
            id,
            stream_type: match source {
                AudioSource::VoiceCall | AudioSource::VoiceCommunication => {
                    AudioStreamType::VoiceCall
                }
                _ => AudioStreamType::System,
            },
            config,
            output_devices: [AudioOutputDevice::Speaker; AUDIO_MAX_DEVICES],
            num_output_devices: 0,
            input_devices,
            num_input_devices: n,
            is_input: true,
            in_standby: false,
            volume_left: 1.0,
            volume_right: 1.0,
            frames_processed: 0,
            ring: [0; AUDIO_RING_SAMPLES],
            ring_head: 0,
            ring_tail: 0,
            ring_fill: 0,
        };

        self.streams.push(stream);
        Ok(id)
    }

    pub fn write_output(&mut self, stream_id: u32, data: &[u8]) -> Result<usize, AudioError> {
        let stream = self
            .streams
            .iter_mut()
            .find(|s| s.id == stream_id && !s.is_input)
            .ok_or(AudioError::StreamNotFound(stream_id))?;

        stream.in_standby = false;
        let frame_size = stream.config.frame_size();
        if frame_size == 0 {
            return Err(AudioError::InvalidParameter("Frame size cannot be 0"));
        }
        // H20: only whole frames are consumed; the non-frame-aligned tail
        // is discarded and never reported as written.
        let aligned_len = (data.len() / frame_size) * frame_size;
        let mut consumed = 0usize;
        // H2: push whole frames as LE i16 samples. On a full ring stop and
        // report a short-write Ok(consumed) so the caller drains/retries;
        // never claim bytes that were not stored.
        for frame in data[..aligned_len].chunks_exact(frame_size) {
            let slots_needed = frame_size.div_ceil(2);
            if stream.ring_fill + slots_needed > AUDIO_RING_SAMPLES {
                break;
            }
            for chunk in frame.as_chunks::<2>().0 {
                stream.ring[stream.ring_head] = i16::from_le_bytes([chunk[0], chunk[1]]);
                stream.ring_head = (stream.ring_head + 1) % AUDIO_RING_SAMPLES;
                stream.ring_fill += 1;
            }
            if frame_size % 2 == 1 {
                // Odd frame size (e.g. 8-bit mono): pad the trailing byte.
                stream.ring[stream.ring_head] = frame[frame_size - 1] as i16;
                stream.ring_head = (stream.ring_head + 1) % AUDIO_RING_SAMPLES;
                stream.ring_fill += 1;
            }
            consumed += frame_size;
        }
        stream.frames_processed += (consumed / frame_size) as u64;
        Ok(consumed)
    }

    pub fn read_input(&mut self, stream_id: u32, _buf: &mut [u8]) -> Result<usize, AudioError> {
        self.streams
            .iter()
            .find(|s| s.id == stream_id && s.is_input)
            .ok_or(AudioError::StreamNotFound(stream_id))?;
        // H2: no capture hardware is bound here; fabricating silence would
        // lie to AEC/NS and the SPA graph. Fail fast.
        Err(AudioError::IoFailure)
    }

    pub fn set_stream_volume(
        &mut self,
        stream_id: u32,
        left: f32,
        right: f32,
    ) -> Result<(), AudioError> {
        // H19: clamp does not reject NaN; fail fast on non-finite input.
        if !left.is_finite() || !right.is_finite() {
            return Err(AudioError::InvalidParameter("volume must be finite"));
        }
        let stream = self
            .streams
            .iter_mut()
            .find(|s| s.id == stream_id)
            .ok_or(AudioError::StreamNotFound(stream_id))?;
        stream.volume_left = left.clamp(0.0, 1.0);
        stream.volume_right = right.clamp(0.0, 1.0);
        Ok(())
    }

    pub fn standby(&mut self, stream_id: u32) -> Result<(), AudioError> {
        let stream = self
            .streams
            .iter_mut()
            .find(|s| s.id == stream_id)
            .ok_or(AudioError::StreamNotFound(stream_id))?;
        stream.in_standby = true;
        Ok(())
    }

    pub fn close_stream(&mut self, stream_id: u32) -> Result<(), AudioError> {
        let pos = self
            .streams
            .iter()
            .position(|s| s.id == stream_id)
            .ok_or(AudioError::StreamNotFound(stream_id))?;
        self.streams.swap_remove(pos);
        Ok(())
    }

    pub fn set_parameters(&mut self, key: &str, val: &str) {
        if let Some(pos) = self.parameters.iter().position(|(k, _)| k == key) {
            self.parameters[pos].1 = val.to_string();
        } else {
            self.parameters.push((key.to_string(), val.to_string()));
        }
    }

    pub fn get_parameter(&self, key: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}
