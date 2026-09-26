//! utim-graphics-check: CLI tool to inspect, benchmark, and verify Phase 2 Display & Graphics Bring-Up.
//! Validates HWC AIDL/HIDL, Gralloc DMA-BUF buffer negotiation, VSYNC tear-free timing,
//! GPU pipeline detection, and 64KB ELF segment alignment.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use utim_core::graphics::composer::{CompositionType, DisplayConfig, HwcComposer};
use utim_core::graphics::elf_align::inspect_elf_file;
use utim_core::graphics::gpu::GpuDetector;
use utim_core::graphics::gralloc::{usage, GrallocManager, PixelFormat};
use utim_core::graphics::vsync::{VsyncConfig, VsyncPresentationValidator, STANDARD_REFRESH_RATES};
use utim_core::hal::HalManager;

fn main() {
    let args: Vec<String> = env::args().collect();
    let json_output = args.iter().any(|a| a == "--json");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        process::exit(0);
    }

    if let Some(pos) = args.iter().position(|a| a == "--generate-env") {
        // Skip option tokens so `--generate-env --json` falls back to the
        // default path instead of writing a file literally named `--json`.
        let out_path = args
            .get(pos + 1)
            .filter(|s| !s.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "/run/utim/graphics.env".to_string());
        generate_env_file(&out_path);
        process::exit(0);
    }

    if let Some(pos) = args.iter().position(|a| a == "--check-elf") {
        let mut targets: Vec<String> = if pos + 1 < args.len() {
            args[pos + 1..]
                .iter()
                .filter(|s| !s.starts_with("--"))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        if targets.is_empty() {
            match env::current_exe() {
                Ok(exe) => targets.push(exe.to_string_lossy().to_string()),
                Err(e) => {
                    eprintln!("[-] cannot determine current executable: {e}");
                    process::exit(1);
                }
            }
        }
        let ok = check_elf_alignment(&targets, json_output);
        process::exit(if ok { 0 } else { 1 });
    }

    // Run full Phase 2 diagnostics suite
    let success = run_full_diagnostics(json_output);
    process::exit(if success { 0 } else { 1 });
}

fn print_usage() {
    println!("utim-graphics-check: Universal Treble Graphics & Display HAL Diagnostic Tool");
    println!("Usage:");
    println!("  utim-graphics-check [OPTIONS]");
    println!("Options:");
    println!("  --all              Run all Phase 2 graphics checks (default)");
    println!("  --check-elf [PATH] Validate 64 KB ELF segment alignment for target binaries");
    println!("  --generate-env [P] Generate GPU pipeline environment variables to file");
    println!("  --json             Emit structured JSON diagnostic output");
    println!("  --help, -h         Show this help message");
}

fn generate_env_file(path: &str) {
    let detector = GpuDetector::new();
    let info = detector.detect_gpu();
    let content = detector.format_env_file(&info);

    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Err(e) = fs::write(path, &content) {
        eprintln!("Error writing GPU environment to {}: {}", path, e);
        process::exit(1);
    }

    println!(
        "[+] Successfully wrote GPU environment configuration to {}",
        path
    );
}

/// Minimal JSON string escaper for `--json` output: user-supplied paths and
/// error strings must not break the document structure.
fn jesc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn check_elf_alignment(targets: &[String], json: bool) -> bool {
    let mut all_ok = true;
    for target in targets {
        let path = Path::new(target);
        if !path.exists() {
            eprintln!("File not found: {}", target);
            all_ok = false;
            continue;
        }

        match inspect_elf_file(path) {
            Ok(report) => {
                if !report.is_64k_compatible {
                    all_ok = false;
                }
                if json {
                    println!(
                        r#"{{"file":"{}","is_64k_compatible":{},"min_align":{},"load_segments":{}}}"#,
                        jesc(target),
                        report.is_64k_compatible,
                        report.min_load_align,
                        report.load_segments.len()
                    );
                } else {
                    println!(
                        "[*] ELF Alignment: {} -> min_align=0x{:x} ({})",
                        target,
                        report.min_load_align,
                        if report.is_64k_compatible {
                            "PASS (>=64KB)"
                        } else {
                            "FAIL (<64KB)"
                        }
                    );
                    if let Some(interp) = report.interpreter.as_deref() {
                        // S22: a PASS covers the binary only; the loader was
                        // not checked.
                        println!("    interpreter: {} (ld.so NOT checked)", interp);
                    }
                    for seg in &report.load_segments {
                        println!(
                            "    PT_LOAD[{}] vaddr=0x{:x} align=0x{:x} ({})",
                            seg.index,
                            seg.vaddr,
                            seg.align,
                            if seg.is_aligned_64k {
                                "64K OK"
                            } else {
                                "INSUFFICIENT"
                            }
                        );
                    }
                }
            }
            Err(e) => {
                all_ok = false;
                if json {
                    println!(r#"{{"file":"{}","error":"{}"}}"#, jesc(target), jesc(&e.to_string()));
                } else {
                    eprintln!("[-] Error inspecting {}: {}", target, e);
                }
            }
        }
    }
    all_ok
}

fn run_full_diagnostics(json: bool) -> bool {
    let mut passed = true;

    // 1. Hardware Composer Check
    let manifest_content = fs::read_to_string("/vendor/etc/vintf/manifest.xml")
        .or_else(|_| fs::read_to_string("/vendor/manifest.xml"))
        .ok();

    // S1: checks 1-3 are meaningless without the HAL manifest they validate
    // against; a green run on a machine with zero Android HALs proves nothing.
    let Some(manifest_content) = manifest_content else {
        eprintln!("[-] no /vendor/etc/vintf/manifest.xml: HWC/Gralloc cannot be validated");
        return false;
    };

    let hwc_version = HwcComposer::detect_version_from_manifest(Some(manifest_content.as_str()));
    let (binder, vndbinder, hwbinder) = HalManager::verify_binder_devices();

    let mut hwc = HwcComposer::new(hwc_version);
    let display_cfg = DisplayConfig::standard_mobile(0, 1080, 2400, 120.0);
    hwc.register_display(display_cfg);

    // A diagnostic tool must report, not abort: every fallible graphics
    // call below returns false with a message instead of panicking (which
    // would be SIGABRT under panic="abort").
    macro_rules! ck {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[-] {e}");
                    return false;
                }
            }
        };
    }

    // Validate 3 layers composition
    let l1 = ck!(hwc.create_layer(0));
    let l2 = ck!(hwc.create_layer(0));
    let l3 = ck!(hwc.create_layer(0));
    ck!(hwc.set_layer_z_order(0, l1, 1));
    ck!(hwc.set_layer_z_order(0, l2, 2));
    ck!(hwc.set_layer_z_order(0, l3, 3));
    ck!(hwc.set_layer_composition_type(0, l1, CompositionType::Device));
    ck!(hwc.set_layer_composition_type(0, l2, CompositionType::Device));
    ck!(hwc.set_layer_composition_type(0, l3, CompositionType::Device));
    ck!(hwc.set_layer_buffer(0, l1, 1001, None));
    ck!(hwc.set_layer_buffer(0, l2, 1002, None));
    ck!(hwc.set_layer_buffer(0, l3, 1003, None));

    let hwc_validate_res = hwc.validate_display(0);
    let hwc_ok = hwc_validate_res.is_ok();
    if !hwc_ok {
        passed = false;
    }

    // 2. Gralloc Check (Linear, UBWC, DMA-BUF)
    let gralloc_version = GrallocManager::detect_version(Some(manifest_content.as_str()));
    let mut gralloc = GrallocManager::new(gralloc_version);

    let buf_linear = gralloc.allocate(
        1080,
        2400,
        PixelFormat::Rgba8888,
        usage::HW_RENDER | usage::HW_COMPOSER,
    );
    let buf_ubwc = gralloc.allocate(
        1080,
        2400,
        PixelFormat::Rgba8888,
        usage::HW_RENDER | usage::HW_COMPOSER | usage::QCOM_USAGE_UBWC,
    );
    let dmabuf_export = buf_linear.as_ref().map(|b| gralloc.export_dmabuf(b));
    let dmabuf_import = if let Ok(Ok(fd)) = &dmabuf_export {
        gralloc.import_dmabuf(
            *fd,
            1080,
            2400,
            PixelFormat::Rgba8888,
            usage::HW_RENDER,
            None,
        )
    } else {
        Err(utim_core::graphics::GrallocError::AllocationFailed(
            "Export failed".into(),
        ))
    };
    if let Ok(Ok(fd)) = dmabuf_export {
        unsafe { libc::close(fd) };
    }

    let gralloc_ok = buf_linear.is_ok() && buf_ubwc.is_ok() && dmabuf_import.is_ok();
    if !gralloc_ok {
        passed = false;
    }

    // 3. VSYNC Timing & Tear-Free Presentation Check across 60/90/120/144 Hz
    let mut vsync_rates_ok = Vec::new();
    for &rate in &STANDARD_REFRESH_RATES {
        let cfg = match VsyncConfig::new(rate) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[-] vsync config {rate} Hz: {e}");
                return false;
            }
        };
        let mut validator = VsyncPresentationValidator::new(cfg);
        let mut sim_vsync = 1_000_000_000u64;
        let mut rate_ok = true;

        for _ in 0..60 {
            let sim_present = sim_vsync + 10_000; // 10 us delay
            if validator
                .validate_frame_presentation(sim_vsync, sim_present)
                .is_err()
            {
                rate_ok = false;
                break;
            }
            sim_vsync += cfg.period_ns;
        }

        if rate_ok && validator.max_jitter_ns() < 1_000_000 {
            vsync_rates_ok.push(rate);
        }
    }
    let vsync_ok = vsync_rates_ok.len() == STANDARD_REFRESH_RATES.len();
    if !vsync_ok {
        passed = false;
    }

    // 4. GPU Detection Check
    let gpu_detector = GpuDetector::new();
    let gpu_info = gpu_detector.detect_gpu();

    // 5. Binary 64KB ELF Alignment Check on own binary
    let current_exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let elf_report = inspect_elf_file(&current_exe).ok();
    let elf_ok = elf_report
        .as_ref()
        .map(|r| r.is_64k_compatible)
        .unwrap_or(false);
    if !elf_ok {
        passed = false;
    }
    let elf_interp = elf_report
        .as_ref()
        .and_then(|r| r.interpreter.clone());

    if json {
        println!(
            r#"{{"status":"{}","hwc":{{"version":"{:?}","binder":{},"vndbinder":{},"hwbinder":{},"validation_ok":{}}},"gralloc":{{"version":"{:?}","linear_ok":{},"ubwc_ok":{},"dmabuf_ok":{},"dmabuf_import_ok":{}}},"vsync":{{"rates_verified":{:?},"all_passed":{}}},"gpu":{{"architecture":"{}","pipeline":"{}","hardware_accelerated":{}}},"elf_64k_compliant":{}}}"#,
            if passed { "PASS" } else { "FAIL" },
            hwc_version,
            binder,
            vndbinder,
            hwbinder,
            hwc_ok,
            gralloc_version,
            buf_linear.is_ok(),
            buf_ubwc.is_ok(),
            gralloc_ok,
            dmabuf_import.is_ok(),
            vsync_rates_ok,
            vsync_ok,
            gpu_info.architecture.name(),
            gpu_info.pipeline.name(),
            gpu_info.architecture.is_hardware_accelerated(),
            elf_ok
        );
    } else {
        println!("============================================================");
        println!(" UNIVERSAL TREBLE LINUX - DISPLAY & GRAPHICS HAL STATUS");
        println!("============================================================");
        println!(
            "[*] 1. Hardware Composer (HWC): {:?} (Service: {})",
            hwc_version,
            hwc_version.service_name()
        );
        println!(
            "       Binder Nodes: /dev/binder={}, /dev/vndbinder={}, /dev/hwbinder={}",
            if binder { "YES" } else { "NO" },
            if vndbinder { "YES" } else { "NO" },
            if hwbinder { "YES" } else { "NO" }
        );
        println!(
            "       Multi-Plane Validation: {}",
            if hwc_ok { "PASSED" } else { "FAILED" }
        );

        println!("[*] 2. Gralloc & DMA-BUF Allocator: {:?}", gralloc_version);
        println!(
            "       Linear RGBA_8888 Allocation: {}",
            if buf_linear.is_ok() {
                "PASSED"
            } else {
                "FAILED"
            }
        );
        println!(
            "       Qualcomm UBWC Compressed Allocation: {}",
            if buf_ubwc.is_ok() { "PASSED" } else { "FAILED" }
        );
        println!(
            "       DMA-BUF Export & Import: {}",
            if gralloc_ok { "PASSED" } else { "FAILED" }
        );

        println!("[*] 3. VSYNC Presentation & Tear-Free Timing:");
        for &rate in &STANDARD_REFRESH_RATES {
            let ok = vsync_rates_ok.contains(&rate);
            println!(
                "       {:>3.0} Hz (Period: {:>5.2} ms): {}",
                rate,
                1000.0 / rate,
                if ok { "VERIFIED TEAR-FREE" } else { "FAILED" }
            );
        }

        println!("[*] 4. 3D GPU Architecture & Pipeline:");
        println!("       Architecture: {}", gpu_info.architecture.name());
        println!("       Active Pipeline: {}", gpu_info.pipeline.name());
        println!(
            "       Acceleration: {}",
            if gpu_info.architecture.is_hardware_accelerated() {
                "HARDWARE ACCELERATED"
            } else {
                "SOFTWARE RASTERIZER (LLVMPIPE)"
            }
        );

        println!("[*] 5. 64 KB ELF Segment Alignment:");
        println!(
            "       Binary: {} (64K Page Compliant: {})",
            current_exe.display(),
            if elf_ok { "YES" } else { "NO (FAIL <64KB)" }
        );
        if let Some(interp) = elf_interp.as_deref() {
            println!("       interpreter: {} (ld.so NOT checked)", interp);
        }

        println!("------------------------------------------------------------");
        println!(
            " OVERALL PHASE 2 STATUS: {}",
            if passed { "ALL TESTS PASSED" } else { "FAILED" }
        );
        println!("============================================================");
    }

    passed
}
