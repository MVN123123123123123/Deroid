//! GPU hardware architecture detection, 3D pipeline selection, and environment configuration.
//! Supports Qualcomm Snapdragon Turnip+Zink, ARM Mali/Exynos/PowerVR libhybris-egl, and software fallback.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuArchitecture {
    QualcommAdreno,
    ArmMali,
    SamsungExynos,
    PowerVR,
    SoftwareFallback,
}

impl GpuArchitecture {
    pub fn name(&self) -> &'static str {
        match self {
            GpuArchitecture::QualcommAdreno => "Qualcomm Adreno",
            GpuArchitecture::ArmMali => "ARM Mali / Immortalis",
            GpuArchitecture::SamsungExynos => "Samsung Exynos (Xclipse / Mali)",
            GpuArchitecture::PowerVR => "Imagination PowerVR",
            GpuArchitecture::SoftwareFallback => "Software Rasterizer (LLVMpipe)",
        }
    }

    pub fn is_hardware_accelerated(&self) -> bool {
        !matches!(self, GpuArchitecture::SoftwareFallback)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPipeline {
    TurnipZink,       // Mesa Turnip Vulkan (/dev/kgsl-3d0) + Zink Gallium OpenGL 4.6
    HybrisEgl,        // libhybris-egl wrapping vendor Bionic EGL/GLES drivers
    SoftwareLlvmpipe, // Software fallback via LLVMpipe
}

impl GpuPipeline {
    pub fn name(&self) -> &'static str {
        match self {
            GpuPipeline::TurnipZink => "Turnip (Vulkan) + Zink (OpenGL 4.6)",
            GpuPipeline::HybrisEgl => "libhybris-egl (Vendor GLES/EGL Shim)",
            GpuPipeline::SoftwareLlvmpipe => "Mesa LLVMpipe (Software)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDeviceInfo {
    pub architecture: GpuArchitecture,
    pub pipeline: GpuPipeline,
    pub chip_model: Option<String>,
    pub device_node: Option<String>,
    pub driver_path: Option<String>,
}

pub struct GpuDetector {
    sysfs_root: PathBuf,
    dev_root: PathBuf,
    vendor_root: PathBuf,
}

impl Default for GpuDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuDetector {
    pub fn new() -> Self {
        Self {
            sysfs_root: PathBuf::from("/sys"),
            dev_root: PathBuf::from("/dev"),
            vendor_root: PathBuf::from("/vendor"),
        }
    }

    pub fn with_roots(sysfs_root: PathBuf, dev_root: PathBuf, vendor_root: PathBuf) -> Self {
        Self {
            sysfs_root,
            dev_root,
            vendor_root,
        }
    }

    /// Detect GPU architecture and select optimal 3D pipeline.
    pub fn detect_gpu(&self) -> GpuDeviceInfo {
        // 1. Check Qualcomm Adreno (/dev/kgsl-3d0)
        let kgsl_node = self.dev_root.join("kgsl-3d0");
        if kgsl_node.exists() {
            let (chip_model, _mhz) = self.read_adreno_gpu();
            return GpuDeviceInfo {
                architecture: GpuArchitecture::QualcommAdreno,
                pipeline: GpuPipeline::TurnipZink,
                chip_model,
                device_node: Some(kgsl_node.to_string_lossy().to_string()),
                driver_path: Some("/usr/lib/aarch64-linux-gnu/dri/zink_dri.so".into()),
            };
        }

        // 2. Check ARM Mali (/dev/mali0 or /dev/mali)
        let mali_node = self.dev_root.join("mali0");
        let mali_alt = self.dev_root.join("mali");
        if mali_node.exists() || mali_alt.exists() {
            let active_node = if mali_node.exists() {
                mali_node
            } else {
                mali_alt
            };
            let vendor_egl = self.vendor_root.join("lib64/egl/libEGL_mali.so");
            return GpuDeviceInfo {
                architecture: GpuArchitecture::ArmMali,
                pipeline: GpuPipeline::HybrisEgl,
                chip_model: Some("ARM Mali".into()),
                device_node: Some(active_node.to_string_lossy().to_string()),
                driver_path: Some(vendor_egl.to_string_lossy().to_string()),
            };
        }

        // 3. Check PowerVR (/dev/pvr_sync)
        let pvr_node = self.dev_root.join("pvr_sync");
        if pvr_node.exists() {
            let pvr_egl = self.vendor_root.join("lib64/egl/libEGL_POWERVR_ROGUE.so");
            return GpuDeviceInfo {
                architecture: GpuArchitecture::PowerVR,
                pipeline: GpuPipeline::HybrisEgl,
                chip_model: Some("PowerVR Rogue".into()),
                device_node: Some(pvr_node.to_string_lossy().to_string()),
                driver_path: Some(pvr_egl.to_string_lossy().to_string()),
            };
        }

        // 4. Check vendor EGL directory inspection
        let vendor_egl_dir = self.vendor_root.join("lib64/egl");
        if let Ok(entries) = fs::read_dir(vendor_egl_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name.contains("adreno") {
                    return GpuDeviceInfo {
                        architecture: GpuArchitecture::QualcommAdreno,
                        pipeline: GpuPipeline::TurnipZink,
                        chip_model: Some("Qualcomm Adreno".into()),
                        device_node: Some("/dev/kgsl-3d0".into()),
                        driver_path: Some(entry.path().to_string_lossy().to_string()),
                    };
                } else if name.contains("xclipse")
                    || name.contains("exynos")
                    || name.contains("samsung")
                {
                    return GpuDeviceInfo {
                        architecture: GpuArchitecture::SamsungExynos,
                        pipeline: GpuPipeline::HybrisEgl,
                        chip_model: Some("Samsung Exynos (Xclipse / Mali)".into()),
                        device_node: Some("/dev/mali0".into()),
                        driver_path: Some(entry.path().to_string_lossy().to_string()),
                    };
                } else if name.contains("mali") {
                    return GpuDeviceInfo {
                        architecture: GpuArchitecture::ArmMali,
                        pipeline: GpuPipeline::HybrisEgl,
                        chip_model: Some("ARM Mali".into()),
                        device_node: Some("/dev/mali0".into()),
                        driver_path: Some(entry.path().to_string_lossy().to_string()),
                    };
                } else if name.contains("powervr") || name.contains("pvr") {
                    return GpuDeviceInfo {
                        architecture: GpuArchitecture::PowerVR,
                        pipeline: GpuPipeline::HybrisEgl,
                        chip_model: Some("PowerVR Rogue".into()),
                        device_node: Some("/dev/pvr_sync".into()),
                        driver_path: Some(entry.path().to_string_lossy().to_string()),
                    };
                }
            }
        }

        // 5. Fallback: LLVMpipe software rasterizer
        GpuDeviceInfo {
            architecture: GpuArchitecture::SoftwareFallback,
            pipeline: GpuPipeline::SoftwareLlvmpipe,
            chip_model: Some("LLVMpipe Software Rasterizer".into()),
            device_node: None,
            driver_path: Some("/usr/lib/aarch64-linux-gnu/dri/kms_swrast_dri.so".into()),
        }
    }

    /// GPU name from sysfs, plus the current devfreq clock. `gpu_model` only
    /// exists on newer kernels, so the clock is reported separately - never
    /// as the model.
    fn read_adreno_gpu(&self) -> (Option<String>, Option<u32>) {
        let model = fs::read_to_string(self.sysfs_root.join("class/kgsl/kgsl-3d0/gpu_model"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mhz = fs::read_to_string(
            self.sysfs_root
                .join("class/kgsl/kgsl-3d0/devfreq/cur_freq"),
        )
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|hz| (hz / 1_000_000) as u32);
        (model, mhz)
    }

    /// Generate environment variables matching the selected GPU pipeline.
    pub fn generate_environment_vars(&self, info: &GpuDeviceInfo) -> HashMap<String, String> {
        let mut vars = HashMap::new();

        match info.pipeline {
            GpuPipeline::TurnipZink => {
                vars.insert("MESA_LOADER_DRIVER_OVERRIDE".into(), "zink".into());
                vars.insert("GALLIUM_DRIVER".into(), "zink".into());
                vars.insert("TU_DEBUG".into(), "kgsl".into());
                vars.insert(
                    "VK_ICD_FILENAMES".into(),
                    "/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json".into(),
                );
                vars.insert("MESA_VK_WSI_PRESENT_MODE".into(), "mailbox".into());
                vars.insert("LIBGL_ALWAYS_SOFTWARE".into(), "0".into());
            }
            GpuPipeline::HybrisEgl => {
                vars.insert("EGL_PLATFORM".into(), "hwcomposer".into());
                vars.insert("HYBRIS_EGLPLATFORM".into(), "hwcomposer".into());
                vars.insert("LIBHYBRIS_WINSYS".into(), "hwcomposer".into());
                vars.insert(
                    "LD_LIBRARY_PATH".into(),
                    "/usr/lib/aarch64-linux-gnu/libhybris:/vendor/lib64:/system/lib64".into(),
                );
                vars.insert("LIBGL_ALWAYS_SOFTWARE".into(), "0".into());
            }
            GpuPipeline::SoftwareLlvmpipe => {
                vars.insert("LIBGL_ALWAYS_SOFTWARE".into(), "1".into());
                vars.insert("GALLIUM_DRIVER".into(), "llvmpipe".into());
            }
        }

        vars
    }

    /// Format environment variables as shell export lines or systemd environment format.
    pub fn format_env_file(&self, info: &GpuDeviceInfo) -> String {
        let mut output =
            String::from("# Universal Treble Linux - Auto-generated GPU Pipeline Config\n");
        output.push_str(&format!(
            "# Detected Architecture: {}\n",
            info.architecture.name()
        ));
        output.push_str(&format!("# Active Pipeline: {}\n", info.pipeline.name()));
        if let Some(chip) = &info.chip_model {
            output.push_str(&format!("# Chip Model: {}\n", chip));
        }
        output.push('\n');

        let vars = self.generate_environment_vars(info);
        let mut sorted_keys: Vec<&String> = vars.keys().collect();
        sorted_keys.sort();

        for key in sorted_keys {
            output.push_str(&format!("{}={}\n", key, vars[key]));
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adreno_detection_and_env() {
        let temp_dir = std::env::temp_dir().join("utim_test_adreno");
        let _ = fs::remove_dir_all(&temp_dir);

        let dev_dir = temp_dir.join("dev");
        let sys_dir = temp_dir.join("sys/class/kgsl/kgsl-3d0");
        let vendor_dir = temp_dir.join("vendor");
        fs::create_dir_all(&dev_dir).unwrap();
        fs::create_dir_all(&sys_dir).unwrap();
        fs::create_dir_all(&vendor_dir).unwrap();

        // Create mock /dev/kgsl-3d0
        fs::write(dev_dir.join("kgsl-3d0"), "").unwrap();
        fs::write(sys_dir.join("gpu_model"), "Adreno 740\n").unwrap();

        let detector = GpuDetector::with_roots(temp_dir.join("sys"), dev_dir, vendor_dir);
        let info = detector.detect_gpu();

        assert_eq!(info.architecture, GpuArchitecture::QualcommAdreno);
        assert_eq!(info.pipeline, GpuPipeline::TurnipZink);
        assert_eq!(info.chip_model.as_deref(), Some("Adreno 740"));
        assert!(info.architecture.is_hardware_accelerated());

        let env = detector.generate_environment_vars(&info);
        assert_eq!(env.get("GALLIUM_DRIVER").map(|s| s.as_str()), Some("zink"));
        assert_eq!(
            env.get("MESA_LOADER_DRIVER_OVERRIDE").map(|s| s.as_str()),
            Some("zink")
        );
        assert!(env.get("VK_ICD_FILENAMES").unwrap().contains("freedreno"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_mali_detection_and_env() {
        let temp_dir = std::env::temp_dir().join("utim_test_mali");
        let _ = fs::remove_dir_all(&temp_dir);

        let dev_dir = temp_dir.join("dev");
        let sys_dir = temp_dir.join("sys");
        let vendor_dir = temp_dir.join("vendor");
        fs::create_dir_all(&dev_dir).unwrap();
        fs::create_dir_all(&sys_dir).unwrap();
        fs::create_dir_all(&vendor_dir).unwrap();

        // Create mock /dev/mali0
        fs::write(dev_dir.join("mali0"), "").unwrap();

        let detector = GpuDetector::with_roots(sys_dir, dev_dir, vendor_dir);
        let info = detector.detect_gpu();

        assert_eq!(info.architecture, GpuArchitecture::ArmMali);
        assert_eq!(info.pipeline, GpuPipeline::HybrisEgl);

        let env = detector.generate_environment_vars(&info);
        assert_eq!(
            env.get("EGL_PLATFORM").map(|s| s.as_str()),
            Some("hwcomposer")
        );
        assert!(env.get("LD_LIBRARY_PATH").unwrap().contains("libhybris"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_software_fallback() {
        let temp_dir = std::env::temp_dir().join("utim_test_sw");
        let _ = fs::remove_dir_all(&temp_dir);

        let dev_dir = temp_dir.join("dev");
        let sys_dir = temp_dir.join("sys");
        let vendor_dir = temp_dir.join("vendor");
        fs::create_dir_all(&dev_dir).unwrap();
        fs::create_dir_all(&sys_dir).unwrap();
        fs::create_dir_all(&vendor_dir).unwrap();

        let detector = GpuDetector::with_roots(sys_dir, dev_dir, vendor_dir);
        let info = detector.detect_gpu();

        assert_eq!(info.architecture, GpuArchitecture::SoftwareFallback);
        assert_eq!(info.pipeline, GpuPipeline::SoftwareLlvmpipe);
        assert!(!info.architecture.is_hardware_accelerated());

        let env = detector.generate_environment_vars(&info);
        assert_eq!(
            env.get("LIBGL_ALWAYS_SOFTWARE").map(|s| s.as_str()),
            Some("1")
        );
        assert_eq!(
            env.get("GALLIUM_DRIVER").map(|s| s.as_str()),
            Some("llvmpipe")
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_exynos_detection() {
        let temp_dir = std::env::temp_dir().join("utim_test_exynos");
        let _ = fs::remove_dir_all(&temp_dir);

        let dev_dir = temp_dir.join("dev");
        let sys_dir = temp_dir.join("sys");
        let vendor_egl = temp_dir.join("vendor/lib64/egl");
        fs::create_dir_all(&dev_dir).unwrap();
        fs::create_dir_all(&sys_dir).unwrap();
        fs::create_dir_all(&vendor_egl).unwrap();

        fs::write(vendor_egl.join("libEGL_xclipse.so"), "").unwrap();

        let detector = GpuDetector::with_roots(sys_dir, dev_dir, temp_dir.join("vendor"));
        let info = detector.detect_gpu();

        assert_eq!(info.architecture, GpuArchitecture::SamsungExynos);
        assert_eq!(info.pipeline, GpuPipeline::HybrisEgl);
        assert!(info.architecture.is_hardware_accelerated());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
