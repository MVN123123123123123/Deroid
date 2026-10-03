//! Android HAL abstraction, VINTF inspection, and synthetic systemd unit synthesis.

use crate::unit::{ExecCommand, ServiceSection, ServiceType, SystemdUnit};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalInterfaceMode {
    LegacyHidl, // Android 8 - 11: /dev/hwbinder, hwservicemanager
    ModernAidl, // Android 12 - 16+: /dev/binder, servicemanager
    Unknown,
}

pub struct HalManager {
    vendor_dir: PathBuf,
}

impl Default for HalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl HalManager {
    pub fn new() -> Self {
        Self::with_vendor(PathBuf::from("/vendor"))
    }

    pub fn with_vendor(vendor_dir: PathBuf) -> Self {
        Self { vendor_dir }
    }

    /// Check if required Binder device nodes exist.
    pub fn verify_binder_devices() -> (bool, bool, bool) {
        let binder = Path::new("/dev/binder").exists();
        let vndbinder = Path::new("/dev/vndbinder").exists();
        let hwbinder = Path::new("/dev/hwbinder").exists();
        (binder, vndbinder, hwbinder)
    }

    /// Inspect VINTF manifest to determine if device uses HIDL or Stable AIDL.
    pub fn detect_interface_mode(&self) -> HalInterfaceMode {
        let mut candidates = vec![
            self.vendor_dir.join("etc/vintf/manifest.xml"),
            self.vendor_dir.join("manifest.xml"),
        ];

        // Search VINTF fragment directory /vendor/etc/vintf/manifest/*.xml
        let fragment_dir = self.vendor_dir.join("etc/vintf/manifest");
        if let Ok(entries) = std::fs::read_dir(&fragment_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("xml") {
                    candidates.push(path);
                }
            }
        }

        let mut found_aidl = false;
        let mut found_hidl = false;

        for manifest_path in &candidates {
            if let Ok(content) = std::fs::read_to_string(manifest_path) {
                // H23: match only HAL format markers. A bare substring such
                // as "composer3" also fires on comments in HIDL-era manifests
                // and would misreport the interface mode.
                if content.contains("format=\"aidl\"") || content.contains("<aidl>") {
                    found_aidl = true;
                }
                if content.contains("format=\"hidl\"") || content.contains("<hidl>") {
                    found_hidl = true;
                }
            }
        }

        if found_aidl {
            return HalInterfaceMode::ModernAidl;
        }
        if found_hidl {
            return HalInterfaceMode::LegacyHidl;
        }

        // Fallback: If /dev/binder exists and /dev/hwbinder does not, assume AIDL
        if Path::new("/dev/binder").exists() && !Path::new("/dev/hwbinder").exists() {
            HalInterfaceMode::ModernAidl
        } else {
            HalInterfaceMode::LegacyHidl
        }
    }

    /// Generate synthetic systemd units for Android HAL daemons so standard Linux
    /// services can declare dependencies like `After=android-hal-audio.service`.
    pub fn create_synthetic_units(&self) -> Vec<SystemdUnit> {
        let hal_defs = [
            (
                "android-hal-composer.service",
                "Android Hardware Composer (HWC) Display HAL",
                vec![
                    "/vendor/bin/hw/android.hardware.graphics.composer@2.1-service",
                    "/vendor/bin/hw/android.hardware.graphics.composer3-service",
                ],
            ),
            (
                "android-hal-audio.service",
                "Android Audio Hardware Abstraction Layer",
                vec![
                    "/vendor/bin/hw/android.hardware.audio.service",
                    "/vendor/bin/hw/android.hardware.audio.core-service",
                ],
            ),
            (
                "android-hal-radio.service",
                "Android Cellular Baseband Radio Interface Layer (rild)",
                vec![
                    "/vendor/bin/hw/rild",
                    "/vendor/bin/hw/android.hardware.radio-service",
                ],
            ),
            (
                "android-hal-camera.service",
                "Android Camera Provider HAL3",
                vec![
                    "/vendor/bin/hw/android.hardware.camera.provider@2.4-service",
                    "/vendor/bin/hw/android.hardware.camera.provider-service",
                ],
            ),
            (
                "android-hal-sensors.service",
                "Android Sensors Hardware Abstraction Layer",
                vec![
                    "/vendor/bin/hw/android.hardware.sensors@1.0-service",
                    "/vendor/bin/hw/android.hardware.sensors-service",
                ],
            ),
        ];

        let mut units = Vec::new();

        for (name, desc, candidate_bins) in hal_defs {
            // H24: only synthesise a unit for a HAL that is actually present.
            // Falling back to a candidate known not to exist would ship a
            // dead ExecStart that dependents can order against.
            let Some(chosen_bin) = candidate_bins.iter().find(|b| Path::new(b).exists()) else {
                continue;
            };

            let mut unit = SystemdUnit::new(
                name.to_string(),
                PathBuf::from(format!("/synthetic/{}", name)),
            );
            unit.unit.description = desc.to_string();
            unit.unit.default_dependencies = false;

            let mut svc = ServiceSection {
                service_type: ServiceType::Simple,
                oom_score_adjust: Some(-700),
                ..Default::default()
            };
            // Parse of a literal candidate path cannot fail, but never emit
            // a unit with an empty ExecStart (fail-closed, not fail-open).
            let Some(cmd) = ExecCommand::parse(chosen_bin) else {
                continue;
            };
            svc.exec_start.push(cmd);

            unit.service = Some(svc);
            units.push(unit);
        }

        units
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_vintf_detection() {
        // H32: pid-suffixed temp dir; the fixed "utim_test_hal" name raced
        // with concurrent cargo test invocations deleting it mid-test.
        let temp_dir = std::env::temp_dir().join(format!("utim_test_hal_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let vintf_dir = temp_dir.join("etc/vintf");
        fs::create_dir_all(&vintf_dir).unwrap();

        let manifest = r#"
<manifest version="5.0" type="device">
    <hal format="aidl">
        <name>android.hardware.graphics.composer3</name>
    </hal>
</manifest>
"#;
        fs::write(vintf_dir.join("manifest.xml"), manifest).unwrap();

        let hal_mgr = HalManager::with_vendor(temp_dir.clone());
        assert_eq!(
            hal_mgr.detect_interface_mode(),
            HalInterfaceMode::ModernAidl
        );

        // H24: no /vendor/bin/hw/* binaries exist in this container, so no
        // synthetic units may be emitted (a dead ExecStart fallback is a bug,
        // not a placeholder).
        let units = hal_mgr.create_synthetic_units();
        assert!(
            units.is_empty(),
            "absent HALs must be skipped, got {:?}",
            units.iter().map(|u| &u.name).collect::<Vec<_>>()
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_vintf_fragment_directory_search() {
        let temp_dir =
            std::env::temp_dir().join(format!("utim_test_vintf_frag_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let fragment_dir = temp_dir.join("etc/vintf/manifest");
        fs::create_dir_all(&fragment_dir).unwrap();

        // Write fragment file hwc3.xml
        let fragment_content = r#"
<manifest version="5.0" type="device">
    <hal format="aidl">
        <name>android.hardware.graphics.composer3</name>
        <fqname>IComposer/default</fqname>
    </hal>
</manifest>
"#;
        fs::write(fragment_dir.join("hwc3.xml"), fragment_content).unwrap();

        let hal_mgr = HalManager::with_vendor(temp_dir.clone());
        assert_eq!(
            hal_mgr.detect_interface_mode(),
            HalInterfaceMode::ModernAidl
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
