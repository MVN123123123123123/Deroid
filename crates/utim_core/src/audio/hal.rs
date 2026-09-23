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
        match self {
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

/// Active audio stream descriptor
#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub id: u32,
    pub stream_type: AudioStreamType,
    pub config: AudioConfig,
    pub output_devices: Vec<AudioOutputDevice>,
    pub input_devices: Vec<AudioInputDevice>,
    pub is_input: bool,
    pub in_standby: bool,
    pub volume_left: f32,
    pub volume_right: f32,
    pub frames_processed: u64,
}

/// Android Audio Hardware Abstraction Layer
pub struct AndroidAudioHal {
    pub version: AudioHalVersion,
    pub mode: AudioMode,
    pub master_volume: f32,
    pub master_muted: bool,
    pub active_output_devices: Vec<AudioOutputDevice>,
    pub active_input_devices: Vec<AudioInputDevice>,
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
            active_output_devices: vec![AudioOutputDevice::Speaker],
            active_input_devices: vec![AudioInputDevice::BuiltinMic],
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
        devices: Vec<AudioOutputDevice>,
    ) -> Result<u32, AudioError> {
        let id = self.next_stream_id;
        self.next_stream_id += 1;

        let stream = ActiveStream {
            id,
            stream_type,
            config,
            output_devices: devices,
            input_devices: Vec::new(),
            is_input: false,
            in_standby: false,
            volume_left: 1.0,
            volume_right: 1.0,
            frames_processed: 0,
        };

        self.streams.push(stream);
        Ok(id)
    }

    pub fn open_input_stream(
        &mut self,
        source: AudioSource,
        config: AudioConfig,
        devices: Vec<AudioInputDevice>,
    ) -> Result<u32, AudioError> {
        let id = self.next_stream_id;
        self.next_stream_id += 1;

        let stream = ActiveStream {
            id,
            stream_type: match source {
                AudioSource::VoiceCall | AudioSource::VoiceCommunication => {
                    AudioStreamType::VoiceCall
                }
                _ => AudioStreamType::System,
            },
            config,
            output_devices: Vec::new(),
            input_devices: devices,
            is_input: true,
            in_standby: false,
            volume_left: 1.0,
            volume_right: 1.0,
            frames_processed: 0,
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
        let frames = data.len() / frame_size;
        stream.frames_processed += frames as u64;
        Ok(data.len())
    }

    pub fn read_input(&mut self, stream_id: u32, buf: &mut [u8]) -> Result<usize, AudioError> {
        let stream = self
            .streams
            .iter_mut()
            .find(|s| s.id == stream_id && s.is_input)
            .ok_or(AudioError::StreamNotFound(stream_id))?;

        stream.in_standby = false;
        let frame_size = stream.config.frame_size();
        if frame_size == 0 {
            return Err(AudioError::InvalidParameter("Frame size cannot be 0"));
        }
        let frames = buf.len() / frame_size;
        stream.frames_processed += frames as u64;
        // Fill with dummy PCM noise or silence
        for b in buf.iter_mut() {
            *b = 0;
        }
        Ok(buf.len())
    }

    pub fn set_stream_volume(
        &mut self,
        stream_id: u32,
        left: f32,
        right: f32,
    ) -> Result<(), AudioError> {
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
