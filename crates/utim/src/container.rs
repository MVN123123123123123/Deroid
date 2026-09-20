//! Sandboxed Android HAL Container and Bionic min-init supervisor.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use utim_core::android_rc::{parse_android_rc, AndroidService};

pub struct AndroidHalContainer {
    vendor_init_dir: PathBuf,
    odm_init_dir: PathBuf,
    discovered_services: Vec<AndroidService>,
    running_hal_pids: HashMap<String, i32>,
}

impl AndroidHalContainer {
    pub fn new() -> Self {
        Self::with_dirs(
            PathBuf::from("/vendor/etc/init"),
            PathBuf::from("/odm/etc/init"),
        )
    }

    pub fn with_dirs(vendor_init_dir: PathBuf, odm_init_dir: PathBuf) -> Self {
        let mut container = Self {
            vendor_init_dir,
            odm_init_dir,
            discovered_services: Vec::new(),
            running_hal_pids: HashMap::new(),
        };
        container.discover_vendor_services();
        container
    }

    /// Scan vendor and odm directories for .rc init definitions.
    pub fn discover_vendor_services(&mut self) {
        self.discovered_services.clear();

        for dir in [&self.vendor_init_dir, &self.odm_init_dir] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("rc") {
                        if let Ok(content) = fs::read_to_string(&path) {
                            let svcs = parse_android_rc(&content);
                            self.discovered_services.extend(svcs);
                        }
                    }
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn services(&self) -> &[AndroidService] {
        &self.discovered_services
    }

    /// Find a service by name or by HAL subsystem (e.g. "composer", "audio", "radio").
    #[allow(dead_code)]
    pub fn find_subsystem_service(&self, subsystem: &str) -> Option<&AndroidService> {
        self.discovered_services
            .iter()
            .find(|s| s.matches_subsystem(subsystem))
    }

    /// Register a running HAL daemon PID.
    #[allow(dead_code)]
    pub fn register_hal_pid(&mut self, name: &str, pid: i32) {
        self.running_hal_pids.insert(name.to_string(), pid);
    }

    /// Handle HAL process exit and determine if it should be restarted (self-healing watchdog).
    pub fn handle_hal_exit(&mut self, pid: i32) -> Option<String> {
        let mut exited_name = None;
        for (name, &running_pid) in &self.running_hal_pids {
            if running_pid == pid {
                exited_name = Some(name.clone());
                break;
            }
        }

        if let Some(ref name) = exited_name {
            self.running_hal_pids.remove(name);
        }

        exited_name
    }
}
