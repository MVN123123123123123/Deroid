//! Comprehensive Phase 2 Display & Graphics HAL Bring-Up Verification Test Suite.
//! Tests HWC AIDL composer3 vs HIDL 2.x, Gralloc DMA-BUF, UBWC/AFBC negotiation,
//! VSYNC 60/90/120/144Hz timing and tear-free presentation, GPU detection, and 64KB ELF alignment.

use std::fs;
use std::path::Path;

use utim_core::graphics::composer::{
    CompositionType, DisplayConfig, HwcComposer, HwcError, HwcVersion, Rect, Transform,
};
use utim_core::graphics::elf_align::{inspect_elf_file, REQUIRED_PAGE_ALIGNMENT};
use utim_core::graphics::gpu::{GpuArchitecture, GpuDetector, GpuPipeline};
use utim_core::graphics::gralloc::{
    usage, GrallocError, GrallocManager, GrallocVersion, PixelFormat,
};
use utim_core::graphics::vsync::{
    VsyncConfig, VsyncController, VsyncPresentationValidator, STANDARD_REFRESH_RATES,
};

#[test]
fn test_hwc_aidl_composer3_full_pipeline() {
    let mut hwc = HwcComposer::new(HwcVersion::AidlComposer3);
    assert!(hwc.version().is_aidl());
    assert_eq!(hwc.version().binder_device(), "/dev/binder");
    assert_eq!(
        hwc.version().service_name(),
        "android.hardware.graphics.composer3.IComposer/default"
    );

    let display_id = 0;
    let config = DisplayConfig::standard_mobile(display_id, 1080, 2400, 120.0);
    hwc.register_display(config);

    // Create 4 layers (Wallpaper, App Content, UI Shell, Status Bar)
    let wallpaper = hwc.create_layer(display_id).unwrap();
    let app_content = hwc.create_layer(display_id).unwrap();
    let ui_shell = hwc.create_layer(display_id).unwrap();
    let status_bar = hwc.create_layer(display_id).unwrap();

    // Set z-orders
    hwc.set_layer_z_order(display_id, wallpaper, 0).unwrap();
    hwc.set_layer_z_order(display_id, app_content, 1).unwrap();
    hwc.set_layer_z_order(display_id, ui_shell, 2).unwrap();
    hwc.set_layer_z_order(display_id, status_bar, 3).unwrap();

    // Set geometry
    let full_screen = Rect::new(0, 0, 1080, 2400);
    let bar_rect = Rect::new(0, 0, 1080, 80);
    hwc.set_layer_display_frame(display_id, wallpaper, full_screen)
        .unwrap();
    hwc.set_layer_display_frame(display_id, app_content, full_screen)
        .unwrap();
    hwc.set_layer_display_frame(display_id, ui_shell, full_screen)
        .unwrap();
    hwc.set_layer_display_frame(display_id, status_bar, bar_rect)
        .unwrap();

    // Set buffers and composition types
    for (i, &l) in [wallpaper, app_content, ui_shell, status_bar]
        .iter()
        .enumerate()
    {
        hwc.set_layer_composition_type(display_id, l, CompositionType::Device)
            .unwrap();
        hwc.set_layer_buffer(display_id, l, 2000 + (i as u64), None)
            .unwrap();
    }

    // Set transform on app_content (Rotate90)
    hwc.set_layer_transform(display_id, app_content, Transform::Rotate90)
        .unwrap();

    // Validate display: 4 layers fit in standard 4 overlay planes
    let (changed, has_client) = hwc.validate_display(display_id).unwrap();
    assert_eq!(changed, 0);
    assert!(!has_client);

    // Accept changes and present
    hwc.accept_display_changes(display_id).unwrap();
    let (present_fence, release_fences) = hwc.present_display(display_id).unwrap();
    assert!(present_fence.is_some());
    assert_eq!(release_fences.len(), 4);
    assert!(release_fences.contains_key(&wallpaper));
    assert!(release_fences.contains_key(&status_bar));
}

#[test]
fn test_hwc_plane_overflow_and_client_target_fallback() {
    let mut hwc = HwcComposer::new(HwcVersion::Hwc2_4);
    let mut config = DisplayConfig::standard_mobile(0, 1440, 3120, 90.0);
    config.max_overlay_planes = 3; // Strict limit: 3 hardware planes
    hwc.register_display(config);

    let mut layers = Vec::new();
    for i in 0..4 {
        let l = hwc.create_layer(0).unwrap();
        hwc.set_layer_z_order(0, l, i as u32).unwrap();
        hwc.set_layer_composition_type(0, l, CompositionType::Device)
            .unwrap();
        hwc.set_layer_buffer(0, l, 3000 + (i as u64), None).unwrap();
        layers.push(l);
    }
    // F11: layer count is bounded by max_overlay_planes+1; the 5th layer
    // must be rejected instead of growing without bound.
    assert!(hwc.create_layer(0).is_err());

    // Validation must demote layers 2, 3 to Client composition
    // because ClientTarget consumes 1 plane, leaving 3-1=2 planes for Device overlays.
    let (changed, has_client) = hwc.validate_display(0).unwrap();
    assert_eq!(changed, 2); // 2 layers demoted (layers 2..3)
    assert!(has_client);

    // Presenting without ClientTarget must fail
    let err = hwc.present_display(0).unwrap_err();
    assert_eq!(err, HwcError::NoClientTarget);

    // Set ClientTarget and present (F5: target invalidates validation).
    hwc.set_client_target(0, 9999, None).unwrap();
    let _ = hwc.validate_display(0).unwrap();
    let (present_fence, release_fences) = hwc.present_display(0).unwrap();
    assert!(present_fence.is_some());
    assert_eq!(release_fences.len(), 4);
}

#[test]
fn test_gralloc_linear_and_compressed_allocations() {
    let mut gralloc = GrallocManager::new(GrallocVersion::AidlAllocator);

    // 1. RGBA_8888 Linear
    let buf_rgba = gralloc
        .allocate(
            1080,
            2340,
            PixelFormat::Rgba8888,
            usage::HW_RENDER | usage::HW_COMPOSER,
        )
        .expect("Allocate RGBA_8888");
    assert_eq!(buf_rgba.planes.len(), 1);
    assert_eq!(buf_rgba.byte_stride, buf_rgba.stride_pixels * 4);
    assert!(buf_rgba.stride_pixels >= 1080);
    assert_eq!(buf_rgba.stride_pixels % 32, 0);

    // 2. Qualcomm UBWC Compressed RGBA_8888
    let buf_ubwc = gralloc
        .allocate(
            1080,
            2340,
            PixelFormat::Rgba8888,
            usage::HW_RENDER | usage::HW_COMPOSER | usage::QCOM_USAGE_UBWC,
        )
        .expect("Allocate UBWC");
    assert!(buf_ubwc.is_ubwc);
    assert_eq!(buf_ubwc.stride_pixels % 64, 0);
    assert_eq!(buf_ubwc.planes.len(), 2); // Pixel data + UBWC metadata plane
    assert!(buf_ubwc.planes[1].size_bytes > 0);
    assert_eq!(buf_ubwc.planes[1].size_bytes % 4096, 0); // 4KB page aligned metadata

    let (_, mod_hi, _mod_lo) = buf_ubwc.wayland_dmabuf_params();
    assert_eq!(mod_hi, 0x05000000); // QCOM modifier prefix

    // 3. ARM AFBC Compressed RGBA_8888
    let buf_afbc = gralloc
        .allocate(
            1080,
            2340,
            PixelFormat::Rgba8888,
            usage::HW_RENDER | usage::HW_COMPOSER | usage::ARM_USAGE_AFBC,
        )
        .expect("Allocate AFBC");
    assert!(buf_afbc.is_afbc);
    assert_eq!(buf_afbc.planes.len(), 2); // Pixel data + AFBC header plane
    let (_, mod_hi_afbc, _) = buf_afbc.wayland_dmabuf_params();
    assert_eq!(mod_hi_afbc, 0x08000000); // ARM AFBC modifier prefix

    // 4. NV12 Camera/Video YUV (Linear & UBWC 4-plane)
    let buf_nv12 = gralloc
        .allocate(1920, 1080, PixelFormat::Nv12, usage::HW_VIDEO_ENCODER)
        .expect("Allocate NV12");
    assert_eq!(buf_nv12.planes.len(), 2);
    assert_eq!(buf_nv12.planes[0].stride_bytes, buf_nv12.stride_pixels);
    assert_eq!(buf_nv12.planes[1].stride_bytes, buf_nv12.stride_pixels);

    let buf_nv12_ubwc = gralloc
        .allocate(
            1920,
            1080,
            PixelFormat::Nv12,
            usage::HW_VIDEO_ENCODER | usage::QCOM_USAGE_UBWC,
        )
        .expect("Allocate UBWC NV12");
    assert!(buf_nv12_ubwc.is_ubwc);
    assert_eq!(buf_nv12_ubwc.planes.len(), 4); // Y + Y meta + UV + UV meta

    // 5. DMA-BUF Export, Safe Clone FD Isolation & Foreign Import
    let exported_fd = gralloc.export_dmabuf(&buf_rgba).expect("Export DMA-BUF");
    assert!(exported_fd >= 0);

    let imported_buf = gralloc
        .import_dmabuf(
            exported_fd,
            1080,
            2340,
            PixelFormat::Rgba8888,
            usage::HW_RENDER,
            None,
        )
        .expect("Import DMA-BUF");
    assert!(imported_buf.fd.is_some());
    assert_ne!(imported_buf.fd.unwrap(), exported_fd);
    unsafe { libc::close(exported_fd) };

    let cloned_buf = buf_rgba.clone();
    assert_ne!(buf_rgba.fd, cloned_buf.fd);
    drop(cloned_buf);
    let orig_flags = unsafe { libc::fcntl(buf_rgba.fd.unwrap(), libc::F_GETFD) };
    assert!(orig_flags >= 0);

    // 6. Conflicting & Incompatible compression usage
    assert!(gralloc
        .allocate(
            1080,
            2340,
            PixelFormat::Rgba8888,
            usage::QCOM_USAGE_UBWC | usage::ARM_USAGE_AFBC,
        )
        .is_err());
    assert!(gralloc
        .allocate(1920, 1080, PixelFormat::Yv12, usage::QCOM_USAGE_UBWC)
        .is_err());

    // 7. Invalid dimensions
    let err = gralloc
        .allocate(0, 1080, PixelFormat::Rgba8888, 0)
        .unwrap_err();
    assert_eq!(err, GrallocError::InvalidDimensions(0, 1080));
    assert!(gralloc
        .allocate(25000, 1080, PixelFormat::Rgba8888, 0)
        .is_err());
}

#[test]
fn test_vsync_tear_free_and_jitter_verification() {
    for &rate in &STANDARD_REFRESH_RATES {
        let config = VsyncConfig::new(rate).unwrap();
        let mut ctrl = VsyncController::new(0, rate).unwrap();
        ctrl.set_enabled(true);

        let mut validator = VsyncPresentationValidator::new(config);

        let mut current_time = 10_000_000_000u64; // 10s baseline
        for seq in 1..=240 {
            let evt = ctrl.on_hardware_vsync(current_time).unwrap();
            assert_eq!(evt.sequence, seq);

            // Simulate frame render finishing 200us after VSYNC (tightly locked)
            let present_time = current_time + 200_000;
            validator
                .validate_frame_presentation(current_time, present_time)
                .unwrap();

            // Next VSYNC pulse prediction
            let predicted = ctrl.next_vsync_timestamp(current_time + 1000);
            assert_eq!(predicted, current_time + config.period_ns);

            current_time += config.period_ns;
        }

        assert_eq!(validator.presented_frames(), 240);
        assert!(validator.max_jitter_ns() < 500_000); // Sub-millisecond jitter
    }
}

#[test]
fn test_vsync_dynamic_refresh_rate_transition() {
    // Test LTPO panel dynamic switching: 60Hz -> 120Hz -> 90Hz -> 60Hz
    let mut ctrl = VsyncController::new(0, 60.0).unwrap();
    let mut validator = VsyncPresentationValidator::new(ctrl.config());
    assert_eq!(ctrl.config().period_ns, 16_666_667);

    ctrl.set_refresh_rate(120.0).unwrap();
    validator.set_refresh_rate(120.0).unwrap();
    assert_eq!(ctrl.config().period_ns, 8_333_333);
    assert_eq!(validator.config().period_ns, 8_333_333);

    ctrl.set_refresh_rate(90.0).unwrap();
    validator.set_refresh_rate(90.0).unwrap();
    assert_eq!(ctrl.config().period_ns, 11_111_111);

    ctrl.set_refresh_rate(60.0).unwrap();
    validator.set_refresh_rate(60.0).unwrap();
    assert_eq!(ctrl.config().period_ns, 16_666_667);

    // Invalid rate
    assert!(ctrl.set_refresh_rate(0.0).is_err());
    assert!(ctrl.set_refresh_rate(-60.0).is_err());
    assert!(ctrl.set_refresh_rate(500.0).is_err());
    assert!(ctrl.set_refresh_rate(f64::NAN).is_err());
}

#[test]
fn test_gpu_detection_and_environment_profiles() {
    let temp = std::env::temp_dir().join("utim_test_gpu_profiles");
    let _ = fs::remove_dir_all(&temp);

    let dev = temp.join("dev");
    let sys = temp.join("sys/class/kgsl/kgsl-3d0");
    let vendor = temp.join("vendor/lib64/egl");
    fs::create_dir_all(&dev).unwrap();
    fs::create_dir_all(&sys).unwrap();
    fs::create_dir_all(&vendor).unwrap();

    // Scenario A: Qualcomm Snapdragon (Adreno 730)
    fs::write(dev.join("kgsl-3d0"), "").unwrap();
    fs::write(sys.join("gpu_model"), "Adreno 730\n").unwrap();

    let detector = GpuDetector::with_roots(temp.join("sys"), dev.clone(), temp.join("vendor"));
    let info = detector.detect_gpu();
    assert_eq!(info.architecture, GpuArchitecture::QualcommAdreno);
    assert_eq!(info.pipeline, GpuPipeline::TurnipZink);
    assert_eq!(info.chip_model.as_deref(), Some("Adreno 730"));

    let env_file = detector.format_env_file(&info);
    assert!(env_file.contains("GALLIUM_DRIVER=zink"));
    assert!(env_file.contains("MESA_LOADER_DRIVER_OVERRIDE=zink"));
    assert!(env_file.contains("TU_DEBUG=kgsl"));
    assert!(env_file.contains("freedreno_icd.aarch64.json"));

    // Scenario B: ARM Mali
    let _ = fs::remove_file(dev.join("kgsl-3d0"));
    fs::write(dev.join("mali0"), "").unwrap();
    fs::write(vendor.join("libEGL_mali.so"), "").unwrap();

    let info_mali = detector.detect_gpu();
    assert_eq!(info_mali.architecture, GpuArchitecture::ArmMali);
    assert_eq!(info_mali.pipeline, GpuPipeline::HybrisEgl);

    let env_mali = detector.format_env_file(&info_mali);
    assert!(env_mali.contains("EGL_PLATFORM=hwcomposer"));
    assert!(env_mali.contains("HYBRIS_EGLPLATFORM=hwcomposer"));
    assert!(env_mali.contains("LIBHYBRIS_WINSYS=hwcomposer"));

    // Scenario C: Samsung Exynos (Xclipse)
    let _ = fs::remove_file(dev.join("mali0"));
    let _ = fs::remove_file(vendor.join("libEGL_mali.so"));
    fs::write(vendor.join("libEGL_xclipse.so"), "").unwrap();

    let info_exynos = detector.detect_gpu();
    assert_eq!(info_exynos.architecture, GpuArchitecture::SamsungExynos);
    assert_eq!(info_exynos.pipeline, GpuPipeline::HybrisEgl);

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn test_elf_alignment_real_release_binaries() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let release_dir = workspace_root.join("target/aarch64-unknown-linux-gnu/release");

    let binaries = [
        "utim",
        "utimctl",
        "deb-systemd-helper",
        "deb-systemd-invoke",
        "utim-graphics-check",
    ];

    for bin_name in &binaries {
        let bin_path = release_dir.join(bin_name);
        if bin_path.exists() {
            let report = inspect_elf_file(&bin_path)
                .unwrap_or_else(|e| panic!("Failed to inspect {}: {}", bin_path.display(), e));
            assert!(
                report.is_64k_compatible,
                "Binary {} does not have 64 KB page alignment! min_align=0x{:x}",
                bin_name, report.min_load_align
            );
            assert!(
                report.min_load_align >= REQUIRED_PAGE_ALIGNMENT,
                "Binary {} alignment 0x{:x} < 0x{:x}",
                bin_name,
                report.min_load_align,
                REQUIRED_PAGE_ALIGNMENT
            );
        }
    }
}
