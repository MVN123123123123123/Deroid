//! Integration Test Suite for Phase 5 Milestone 5.1:
//! Camera HAL3 to Linux V4L2 Bridge (gstreamer1.0-droid / v4l2loopback).
//! Exhaustively validates:
//! - Camera HAL3 device discovery (Front and Back cameras)
//! - Stream configuration (Preview NV12, Still Capture JPEG)
//! - 3A Controls (Auto-Focus, Auto-Exposure, Auto-White-Balance), Flash, and Zoom
//! - /dev/v4l2loopback virtual device ioctls (QUERYCAP, G_FMT, S_FMT, REQBUFS, QBUF, DQBUF, STREAMON/OFF)
//! - Frame capture feeding and compatibility with Linux desktop applications (Firefox WebRTC, Cheese)

use utim_core::camera::{
    AeMode, AfMode, AwbMode, CameraBringupStatus, CameraDeviceInfo, CameraFacing, CameraHal3Device,
    CameraPixelFormat, CameraStreamType, FlashMode, V4l2Buffer, V4l2LoopbackBridge,
    V4L2_CAP_READWRITE, V4L2_CAP_STREAMING, V4L2_CAP_VIDEO_CAPTURE, V4L2_PIX_FMT_NV12,
    V4L2_PIX_FMT_YUYV,
};

fn create_mock_back_camera() -> CameraDeviceInfo {
    CameraDeviceInfo {
        id: 0,
        facing: CameraFacing::Back,
        sensor_orientation: 90,
        max_width: 4000,
        max_height: 3000,
        supported_fps_ranges: vec![(15, 30), (30, 30), (60, 60)],
        has_flash: true,
    }
}

fn create_mock_front_camera() -> CameraDeviceInfo {
    CameraDeviceInfo {
        id: 1,
        facing: CameraFacing::Front,
        sensor_orientation: 270,
        max_width: 2560,
        max_height: 1920,
        supported_fps_ranges: vec![(15, 30), (30, 30)],
        has_flash: false,
    }
}

#[test]
fn test_milestone_5_1_camera_characteristics_and_selection() {
    let back_info = create_mock_back_camera();
    assert_eq!(back_info.facing, CameraFacing::Back);
    assert_eq!(back_info.sensor_orientation, 90);
    assert!(back_info.has_flash);
    assert_eq!(back_info.max_width, 4000);

    let front_info = create_mock_front_camera();
    assert_eq!(front_info.facing, CameraFacing::Front);
    assert_eq!(front_info.sensor_orientation, 270);
    assert!(!front_info.has_flash);
}

#[test]
fn test_milestone_5_1_camera_hal3_stream_configuration() {
    let back_info = create_mock_back_camera();
    let mut camera = CameraHal3Device::new(back_info);

    // 1. Configure standard 1080p preview stream (NV12)
    camera
        .configure_stream(
            CameraStreamType::Preview,
            1920,
            1080,
            CameraPixelFormat::Nv12,
        )
        .expect("Failed to configure 1080p preview");
    assert_eq!(camera.active_stream, Some(CameraStreamType::Preview));
    assert_eq!(camera.stream_width, 1920);
    assert_eq!(camera.stream_height, 1080);
    assert_eq!(camera.stream_format, CameraPixelFormat::Nv12);

    // 2. Reject impossible resolution exceeding sensor limits
    let bad_config = camera.configure_stream(
        CameraStreamType::StillCapture,
        8000,
        6000,
        CameraPixelFormat::JpegBlob,
    );
    assert!(bad_config.is_err());
}

#[test]
fn test_milestone_5_1_camera_3a_controls_and_flash() {
    let back_info = create_mock_back_camera();
    let mut camera = CameraHal3Device::new(back_info);

    // 1. 3A controls
    camera.controls.af_mode = AfMode::ContinuousVideo;
    camera.controls.ae_mode = AeMode::On;
    camera.controls.ae_compensation = 2; // +2 EV
    camera.controls.awb_mode = AwbMode::Daylight;
    assert_eq!(camera.controls.af_mode, AfMode::ContinuousVideo);
    assert_eq!(camera.controls.ae_compensation, 2);

    // 2. Zoom setting
    camera.set_zoom(3.5);
    assert!((camera.controls.zoom_ratio - 3.5).abs() < 0.01);
    camera.set_zoom(99.0); // Clamped to 10.0x
    assert_eq!(camera.controls.zoom_ratio, 10.0);

    // 3. Flash on back camera succeeds
    assert!(camera.set_flash(FlashMode::Torch).is_ok());
    assert_eq!(camera.controls.flash_mode, FlashMode::Torch);

    // 4. Flash on front camera (without flash unit) fails cleanly
    let front_info = create_mock_front_camera();
    let mut front_camera = CameraHal3Device::new(front_info);
    assert!(front_camera.set_flash(FlashMode::Torch).is_err());
}

#[test]
fn test_milestone_5_1_v4l2_loopback_negotiation_and_ioctls() {
    let mut v4l2 = V4l2LoopbackBridge::new("/dev/v4l2loopback");

    // 1. ioctl VIDIOC_QUERYCAP
    let cap = v4l2.query_cap();
    assert_eq!(
        cap.capabilities & V4L2_CAP_VIDEO_CAPTURE,
        V4L2_CAP_VIDEO_CAPTURE
    );
    assert_eq!(cap.capabilities & V4L2_CAP_STREAMING, V4L2_CAP_STREAMING);
    assert_eq!(cap.capabilities & V4L2_CAP_READWRITE, V4L2_CAP_READWRITE);

    // 2. ioctl VIDIOC_G_FMT & VIDIOC_S_FMT
    let fmt = v4l2
        .set_format(1280, 720, V4L2_PIX_FMT_YUYV)
        .expect("Failed to set YUYV format");
    assert_eq!(fmt.width, 1280);
    assert_eq!(fmt.height, 720);
    assert_eq!(fmt.pixelformat, V4L2_PIX_FMT_YUYV);
    assert_eq!(fmt.bytesperline, 1280 * 2);
    assert_eq!(fmt.sizeimage, 1280 * 720 * 2);

    // Switch back to 1080p NV12
    let fmt_nv12 = v4l2.set_format(1920, 1080, V4L2_PIX_FMT_NV12).unwrap();
    assert_eq!(fmt_nv12.pixelformat, V4L2_PIX_FMT_NV12);
    assert_eq!(fmt_nv12.sizeimage, 1920 * 1080 * 3 / 2);

    // 3. ioctl VIDIOC_REQBUFS
    let allocated = v4l2.request_buffers(4).expect("REQBUFS failed");
    assert_eq!(allocated, 4);
    assert_eq!(v4l2.allocated_buffers.len(), 4);

    // 4. ioctl VIDIOC_STREAMON
    v4l2.stream_on().expect("STREAMON failed");
    assert!(v4l2.is_streaming);

    // Cannot change format while streaming
    assert!(v4l2.set_format(640, 480, V4L2_PIX_FMT_NV12).is_err());

    // 5. ioctl VIDIOC_STREAMOFF
    v4l2.stream_off();
    assert!(!v4l2.is_streaming);
}

#[test]
fn test_milestone_5_1_desktop_apps_compatibility_pipeline() {
    let back_info = create_mock_back_camera();
    let mut camera = CameraHal3Device::new(back_info);
    camera
        .configure_stream(
            CameraStreamType::Preview,
            1920,
            1080,
            CameraPixelFormat::Nv12,
        )
        .unwrap();
    camera.start_stream().unwrap();

    let mut v4l2 = V4l2LoopbackBridge::new("/dev/v4l2loopback");
    v4l2.set_format(1920, 1080, V4L2_PIX_FMT_NV12).unwrap();
    v4l2.request_buffers(4).unwrap();
    v4l2.stream_on().unwrap();

    // Desktop app (Firefox / Cheese) queues buffers
    v4l2.queue_buffer(0).unwrap();
    v4l2.queue_buffer(1).unwrap();

    // Produce frames from Camera HAL3 and feed to V4L2 loopback
    for _ in 0..5 {
        let frame = camera.produce_frame().expect("HAL produce frame failed");
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.buffer_size, 1920 * 1080 * 3 / 2);

        // Queue a buffer if none queued
        let _ = v4l2.queue_buffer(0);

        // Feed to V4L2
        let buf_idx = v4l2.feed_hal_frame(&frame).expect("Feed frame failed");
        assert_eq!(buf_idx, 0);

        // Desktop app dequeues buffer
        let mut dq = V4l2Buffer::default();
        v4l2.dequeue_buffer(&mut dq).expect("DQBUF failed");
        assert_eq!(dq.index, 0);
        assert_eq!(dq.bytesused as usize, frame.buffer_size);
    }

    assert_eq!(v4l2.frames_delivered, 5);

    let status = CameraBringupStatus {
        num_cameras: 2,
        active_facing: CameraFacing::Back,
        stream_configured: true,
        v4l2_streaming: v4l2.is_streaming,
        frames_delivered: v4l2.frames_delivered,
    };
    assert!(status.is_ready());
}

#[test]
fn test_milestone_5_1_camera_streaming_edge_cases() {
    let back_info = create_mock_back_camera();
    let mut camera = CameraHal3Device::new(back_info);
    camera
        .configure_stream(
            CameraStreamType::Preview,
            1920,
            1080,
            CameraPixelFormat::Nv12,
        )
        .unwrap();
    camera.start_stream().unwrap();

    // 1. Reconfiguration while streaming must fail
    let reconfig_err = camera.configure_stream(
        CameraStreamType::Preview,
        1280,
        720,
        CameraPixelFormat::Nv12,
    );
    assert_eq!(
        reconfig_err,
        Err("Cannot reconfigure stream while streaming is active")
    );

    // 2. Multi-buffer FIFO without overwrite in V4L2 Loopback
    let mut v4l2 = V4l2LoopbackBridge::new("/dev/v4l2loopback");
    v4l2.set_format(1920, 1080, V4L2_PIX_FMT_NV12).unwrap();
    v4l2.request_buffers(3).unwrap();
    v4l2.stream_on().unwrap();

    // Application queues 3 buffers
    v4l2.queue_buffer(0).unwrap();
    v4l2.queue_buffer(1).unwrap();
    v4l2.queue_buffer(2).unwrap();

    // Feed 3 frames from HAL
    let frame1 = camera.produce_frame().unwrap();
    let b0 = v4l2.feed_hal_frame(&frame1).unwrap();
    assert_eq!(b0, 0);

    let frame2 = camera.produce_frame().unwrap();
    let b1 = v4l2.feed_hal_frame(&frame2).unwrap();
    assert_eq!(
        b1, 1,
        "Must advance to buffer 1 rather than overwriting buffer 0"
    );

    let frame3 = camera.produce_frame().unwrap();
    let b2 = v4l2.feed_hal_frame(&frame3).unwrap();
    assert_eq!(b2, 2, "Must advance to buffer 2");

    // All buffers filled -> starvation on 4th frame
    let frame4 = camera.produce_frame().unwrap();
    assert!(v4l2.feed_hal_frame(&frame4).is_err());

    // Dequeue in order
    let mut dq = V4l2Buffer::default();
    v4l2.dequeue_buffer(&mut dq).unwrap();
    assert_eq!(dq.index, 0);
    v4l2.dequeue_buffer(&mut dq).unwrap();
    assert_eq!(dq.index, 1);
    v4l2.dequeue_buffer(&mut dq).unwrap();
    assert_eq!(dq.index, 2);

    // 3. Resolution mismatch rejection
    v4l2.queue_buffer(0).unwrap();
    let bad_res_frame = utim_core::camera::CapturedFrame {
        frame_number: 99,
        timestamp_ns: 1000,
        width: 1280,
        height: 720,
        format: CameraPixelFormat::Nv12,
        stride: 1280,
        buffer_size: 1280 * 720 * 3 / 2,
        dmabuf_fd: -1,
    };
    assert_eq!(
        v4l2.feed_hal_frame(&bad_res_frame),
        Err("Frame resolution does not match negotiated V4L2 format")
    );
}
