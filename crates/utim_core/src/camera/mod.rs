//! Universal Treble Camera Subsystem (Phase 5 Milestone 5.1).
//! Bridges Android Camera HAL3 / AIDL ICameraProvider with the Linux V4L2 loopback
//! virtual video device (/dev/v4l2loopback).
//! Enables zero-copy camera video capture for Linux desktop applications (Firefox WebRTC, Cheese).

pub mod hal3;
pub mod v4l2_bridge;

pub use hal3::{
    AeMode, AeState, AfMode, AfState, AwbMode, AwbState, Camera3aControls, CameraDeviceInfo,
    CameraFacing, CameraHal3Device, CameraPixelFormat, CameraStreamType, CapturedFrame, FlashMode,
};
pub use v4l2_bridge::{
    V4l2Buffer, V4l2Capability, V4l2Format, V4l2LoopbackBridge, V4L2_CAP_READWRITE,
    V4L2_CAP_STREAMING, V4L2_CAP_VIDEO_CAPTURE, V4L2_PIX_FMT_MJPEG, V4L2_PIX_FMT_NV12,
    V4L2_PIX_FMT_YUYV,
};

/// Full Phase 5 Camera Subsystem Bring-Up Status
#[derive(Debug, Clone, PartialEq)]
pub struct CameraBringupStatus {
    pub num_cameras: usize,
    pub active_facing: CameraFacing,
    pub stream_configured: bool,
    pub v4l2_streaming: bool,
    pub frames_delivered: u64,
}

impl CameraBringupStatus {
    pub fn is_ready(&self) -> bool {
        self.num_cameras > 0
    }
}
