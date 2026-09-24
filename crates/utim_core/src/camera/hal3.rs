//! Android Camera HAL3 & AIDL ICameraProvider Abstraction.
//! Interfaces with android.hardware.camera.provider@2.4-2.7 and AIDL CameraProvider.
//! Supports multi-stream capture (Preview, Still Capture, Video), 3A controls (AF, AE, AWB),
//! flash modes, and front/rear sensor switching.
//! Conforms strictly to GEMINI.md systems discipline.

/// Camera facing direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFacing {
    Back = 0,
    Front = 1,
    External = 2,
}

/// Camera HAL3 Stream Configuration Type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraStreamType {
    Preview,
    StillCapture,
    VideoRecord,
}

/// Camera Pixel Format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPixelFormat {
    Nv12,
    Yuv420Planar,
    Yuyv,
    JpegBlob,
    RawSensor,
}

impl CameraPixelFormat {
    pub fn bits_per_pixel(&self) -> u32 {
        match self {
            Self::Nv12 | Self::Yuv420Planar => 12,
            Self::Yuyv => 16,
            Self::JpegBlob => 8, // compressed
            Self::RawSensor => 16,
        }
    }
}

/// Auto-Focus (AF) Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfMode {
    Off,
    Auto,
    Macro,
    ContinuousVideo,
    ContinuousPicture,
}

/// Auto-Focus (AF) State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfState {
    Inactive,
    PassiveScan,
    PassiveFocused,
    ActiveScan,
    FocusedLocked,
    NotFocusedLocked,
}

/// Auto-Exposure (AE) Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeMode {
    Off,
    On,
    OnAutoFlash,
    OnAlwaysFlash,
    OnAutoFlashRedEye,
}

/// Auto-Exposure (AE) State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeState {
    Inactive,
    Searching,
    Converged,
    Locked,
    FlashRequired,
    Precapture,
}

/// Auto-White-Balance (AWB) Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwbMode {
    Off,
    Auto,
    Incandescent,
    Fluorescent,
    Daylight,
    CloudyDaylight,
    Shade,
}

/// Auto-White-Balance (AWB) State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwbState {
    Inactive,
    Searching,
    Converged,
    Locked,
}

/// Flash Control Mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMode {
    Off,
    Single,
    Torch,
}

/// 3A Control State and Settings
#[derive(Debug, Clone, PartialEq)]
pub struct Camera3aControls {
    pub af_mode: AfMode,
    pub af_state: AfState,
    pub ae_mode: AeMode,
    pub ae_state: AeState,
    pub ae_compensation: i32, // EV steps (-4..+4)
    pub awb_mode: AwbMode,
    pub awb_state: AwbState,
    pub flash_mode: FlashMode,
    pub zoom_ratio: f32, // 1.0..10.0
}

impl Default for Camera3aControls {
    fn default() -> Self {
        Self {
            af_mode: AfMode::ContinuousPicture,
            af_state: AfState::FocusedLocked,
            ae_mode: AeMode::On,
            ae_state: AeState::Converged,
            ae_compensation: 0,
            awb_mode: AwbMode::Auto,
            awb_state: AwbState::Converged,
            flash_mode: FlashMode::Off,
            zoom_ratio: 1.0,
        }
    }
}

/// Camera Sensor Characteristics
#[derive(Debug, Clone, PartialEq)]
pub struct CameraDeviceInfo {
    pub id: u32,
    pub facing: CameraFacing,
    pub sensor_orientation: u32, // 0, 90, 180, 270 degrees
    pub max_width: u32,
    pub max_height: u32,
    pub supported_fps_ranges: Vec<(u32, u32)>,
    pub has_flash: bool,
}

impl CameraDeviceInfo {
    pub fn back_camera(id: u32, max_w: u32, max_h: u32) -> Self {
        Self {
            id,
            facing: CameraFacing::Back,
            sensor_orientation: 90,
            max_width: max_w,
            max_height: max_h,
            supported_fps_ranges: vec![(15, 30), (30, 30), (30, 60)],
            has_flash: true,
        }
    }

    pub fn front_camera(id: u32, max_w: u32, max_h: u32) -> Self {
        Self {
            id,
            facing: CameraFacing::Front,
            sensor_orientation: 270,
            max_width: max_w,
            max_height: max_h,
            supported_fps_ranges: vec![(15, 30), (30, 30)],
            has_flash: false,
        }
    }
}

/// Captured Frame Buffer Descriptor
#[derive(Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    pub frame_number: u64,
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub format: CameraPixelFormat,
    pub stride: u32,
    pub buffer_size: usize,
    pub dmabuf_fd: i32,
}

impl Clone for CapturedFrame {
    fn clone(&self) -> Self {
        let dup_fd = if self.dmabuf_fd >= 0 {
            let fd = unsafe { libc::fcntl(self.dmabuf_fd, libc::F_DUPFD_CLOEXEC, 0) };
            if fd >= 0 {
                fd
            } else {
                -1
            }
        } else {
            self.dmabuf_fd
        };
        Self {
            frame_number: self.frame_number,
            timestamp_ns: self.timestamp_ns,
            width: self.width,
            height: self.height,
            format: self.format,
            stride: self.stride,
            buffer_size: self.buffer_size,
            dmabuf_fd: dup_fd,
        }
    }
}

impl Drop for CapturedFrame {
    fn drop(&mut self) {
        if self.dmabuf_fd >= 0 {
            unsafe {
                libc::close(self.dmabuf_fd);
            }
            self.dmabuf_fd = -1;
        }
    }
}

/// Camera HAL3 Device Handle
pub struct CameraHal3Device {
    pub info: CameraDeviceInfo,
    pub controls: Camera3aControls,
    pub active_stream: Option<CameraStreamType>,
    pub stream_width: u32,
    pub stream_height: u32,
    pub stream_format: CameraPixelFormat,
    pub is_streaming: bool,
    frame_counter: u64,
}

impl CameraHal3Device {
    pub fn new(info: CameraDeviceInfo) -> Self {
        Self {
            info,
            controls: Camera3aControls::default(),
            active_stream: None,
            stream_width: 1920,
            stream_height: 1080,
            stream_format: CameraPixelFormat::Nv12,
            is_streaming: false,
            frame_counter: 0,
        }
    }

    /// Configure capture stream
    pub fn configure_stream(
        &mut self,
        stream_type: CameraStreamType,
        width: u32,
        height: u32,
        format: CameraPixelFormat,
    ) -> Result<(), &'static str> {
        if self.is_streaming {
            return Err("Cannot reconfigure stream while streaming is active");
        }
        if width > self.info.max_width || height > self.info.max_height {
            return Err("Requested resolution exceeds sensor capabilities");
        }
        self.active_stream = Some(stream_type);
        self.stream_width = width;
        self.stream_height = height;
        self.stream_format = format;
        Ok(())
    }

    pub fn start_stream(&mut self) -> Result<(), &'static str> {
        if self.active_stream.is_none() {
            return Err("No stream configured");
        }
        self.is_streaming = true;
        Ok(())
    }

    pub fn stop_stream(&mut self) {
        self.is_streaming = false;
    }

    pub fn set_zoom(&mut self, ratio: f32) {
        self.controls.zoom_ratio = ratio.clamp(1.0, 10.0);
    }

    pub fn set_flash(&mut self, mode: FlashMode) -> Result<(), &'static str> {
        if !self.info.has_flash && mode != FlashMode::Off {
            return Err("Device does not possess a flash unit");
        }
        self.controls.flash_mode = mode;
        Ok(())
    }

    /// Produce next frame from ISP pipeline
    pub fn produce_frame(&mut self) -> Result<CapturedFrame, &'static str> {
        if !self.is_streaming {
            return Err("Camera is not streaming");
        }

        self.frame_counter += 1;
        let stride = self.stream_width;
        let buffer_size = match self.stream_format {
            CameraPixelFormat::Nv12 | CameraPixelFormat::Yuv420Planar => {
                (self.stream_width * self.stream_height * 3 / 2) as usize
            }
            CameraPixelFormat::Yuyv => (self.stream_width * self.stream_height * 2) as usize,
            CameraPixelFormat::JpegBlob => (self.stream_width * self.stream_height / 4) as usize,
            CameraPixelFormat::RawSensor => (self.stream_width * self.stream_height * 2) as usize,
        };

        Ok(CapturedFrame {
            frame_number: self.frame_counter,
            timestamp_ns: self.frame_counter * 33_333_333, // ~30 fps
            width: self.stream_width,
            height: self.stream_height,
            format: self.stream_format,
            stride,
            buffer_size,
            dmabuf_fd: -1, // Mock or DMA-BUF fd
        })
    }
}
