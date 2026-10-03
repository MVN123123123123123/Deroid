//! Mobile Power Governor (MPG) for Android WakeLock and Cgroup v2 Freezing.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

pub struct MobilePowerGovernor {
    sys_power_dir: PathBuf,
    cgroup_root: PathBuf,
    battery_dir: PathBuf,
    cpu_sys_dir: PathBuf,
    active_wake_locks: HashSet<String>,
    charge_limit_percent: u32,
    wake_lock_file: Option<fs::File>,
    wake_unlock_file: Option<fs::File>,
}

impl Default for MobilePowerGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl MobilePowerGovernor {
    pub fn new() -> Self {
        Self::with_paths_all(
            PathBuf::from("/sys/power"),
            PathBuf::from("/sys/fs/cgroup"),
            PathBuf::from("/sys/class/power_supply/battery"),
            PathBuf::from("/sys/devices/system/cpu"),
        )
    }

    pub fn with_paths(sys_power_dir: PathBuf, cgroup_root: PathBuf, battery_dir: PathBuf) -> Self {
        Self::with_paths_all(
            sys_power_dir,
            cgroup_root,
            battery_dir,
            PathBuf::from("/sys/devices/system/cpu"),
        )
    }

    pub fn with_paths_all(
        sys_power_dir: PathBuf,
        cgroup_root: PathBuf,
        battery_dir: PathBuf,
        cpu_sys_dir: PathBuf,
    ) -> Self {
        Self {
            sys_power_dir,
            cgroup_root,
            battery_dir,
            cpu_sys_dir,
            active_wake_locks: HashSet::new(),
            charge_limit_percent: 80,
            wake_lock_file: None,
            wake_unlock_file: None,
        }
    }

    /// Acquire a partial Android kernel wake lock.
    pub fn acquire_wake_lock(&mut self, name: &str) -> io::Result<()> {
        if self.wake_lock_file.is_none() {
            let wake_lock_path = self.sys_power_dir.join("wake_lock");
            if wake_lock_path.exists() {
                {
                    let f = OpenOptions::new().write(true).open(&wake_lock_path)?;
                    self.wake_lock_file = Some(f)
                }
            } else {
                // B7: no file => no lock; do not record a phantom lock.
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "{} does not exist (no wake_lock interface?)",
                        wake_lock_path.display()
                    ),
                ));
            }
        }
        if let Some(ref mut file) = self.wake_lock_file {
            // Android wake_lock interface requires a trailing newline.
            file.write_all(format!("{}\n", name).as_bytes())?;
            file.flush()?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "wake_lock file unavailable",
            ));
        }
        self.active_wake_locks.insert(name.to_string());
        Ok(())
    }

    /// Release a held Android kernel wake lock.
    pub fn release_wake_lock(&mut self, name: &str) -> io::Result<()> {
        if self.wake_unlock_file.is_none() {
            let wake_unlock_path = self.sys_power_dir.join("wake_unlock");
            if wake_unlock_path.exists() {
                {
                    let f = OpenOptions::new().write(true).open(&wake_unlock_path)?;
                    self.wake_unlock_file = Some(f)
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} does not exist", wake_unlock_path.display()),
                ));
            }
        }
        if let Some(ref mut file) = self.wake_unlock_file {
            file.write_all(format!("{}\n", name).as_bytes())?;
            file.flush()?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "wake_unlock file unavailable",
            ));
        }
        self.active_wake_locks.remove(name);
        Ok(())
    }

    /// Check if any wake locks are currently held.
    pub fn has_active_wake_locks(&self) -> bool {
        !self.active_wake_locks.is_empty()
    }

    /// Check if a specific wake lock is currently held.
    pub fn is_wake_lock_active(&self, name: &str) -> bool {
        self.active_wake_locks.contains(name)
    }

    pub fn active_wake_locks(&self) -> &HashSet<String> {
        &self.active_wake_locks
    }

    /// Freeze or unfreeze a cgroup v2 slice (e.g. "user.slice").
    /// Writing "1" to cgroup.freeze stops all processes in that slice with zero CPU wakeups.
    /// B7: missing cgroup.freeze is an error, not silent success, so the PSI
    /// handler does not believe it is throttling when it did nothing.
    pub fn set_cgroup_freeze(&self, slice_name: &str, freeze: bool) -> io::Result<()> {
        let slice_dir = self.cgroup_root.join(slice_name);
        let freeze_file = slice_dir.join("cgroup.freeze");
        let val = if freeze { "1\n" } else { "0\n" };
        fs::write(&freeze_file, val).map_err(|e| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} missing (cgroup2 not mounted?): {}",
                    freeze_file.display(),
                    e
                ),
            )
        })
    }

    /// Check the freeze state of a cgroup slice.
    pub fn is_cgroup_frozen(&self, slice_name: &str) -> io::Result<bool> {
        let freeze_file = self.cgroup_root.join(slice_name).join("cgroup.freeze");
        if freeze_file.exists() {
            let content = fs::read_to_string(freeze_file)?;
            Ok(content.trim() == "1")
        } else {
            Ok(false)
        }
    }

    /// Coordinate kernel autosleep (/sys/power/autosleep).
    /// If enable is true and no wake locks are held, the kernel will suspend-to-RAM automatically.
    /// Missing file is an error (B7), not silent success.
    pub fn configure_autosleep(&self, enable: bool) -> io::Result<()> {
        let autosleep_file = self.sys_power_dir.join("autosleep");
        let state = if enable { "mem\n" } else { "off\n" };
        fs::write(&autosleep_file, state).map_err(|e| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} missing: {}", autosleep_file.display(), e),
            )
        })
    }

    /// Apply battery charge limit threshold (e.g. 80%) to preserve battery longevity.
    /// Missing file is an error (B7), not silent success.
    pub fn set_charge_limit(&mut self, limit_pct: u32) -> io::Result<()> {
        self.charge_limit_percent = limit_pct.min(100);
        let limit_file = self.battery_dir.join("charge_control_limit_max");
        fs::write(&limit_file, format!("{}\n", self.charge_limit_percent)).map_err(|e| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} missing: {}", limit_file.display(), e),
            )
        })
    }

    /// Set CPU governor across available CPU cores (e.g. "powersave" or "schedutil").
    pub fn set_cpu_governor(&self, governor: &str) -> io::Result<usize> {
        let mut count = 0;
        let mut last_err = None;
        if let Ok(entries) = fs::read_dir(&self.cpu_sys_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("cpu") && name_str[3..].chars().all(|c| c.is_ascii_digit())
                {
                    let gov_path = entry.path().join("cpufreq/scaling_governor");
                    if gov_path.exists() {
                        match fs::write(&gov_path, format!("{}\n", governor)) {
                            Ok(_) => count += 1,
                            Err(e) => last_err = Some(e),
                        }
                    }
                }
            }
        }
        if count == 0 {
            if let Some(e) = last_err {
                return Err(e);
            }
        }
        Ok(count)
    }

    /// Set maximum CPU frequency limit across available CPU cores.
    pub fn set_cpu_max_frequency_limit(&self, max_khz: u32) -> io::Result<usize> {
        let mut count = 0;
        let mut last_err = None;
        if let Ok(entries) = fs::read_dir(&self.cpu_sys_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("cpu") && name_str[3..].chars().all(|c| c.is_ascii_digit())
                {
                    let freq_path = entry.path().join("cpufreq/scaling_max_freq");
                    if freq_path.exists() {
                        match fs::write(&freq_path, format!("{}\n", max_khz)) {
                            Ok(_) => count += 1,
                            Err(e) => last_err = Some(e),
                        }
                    }
                }
            }
        }
        if count == 0 {
            if let Some(e) = last_err {
                return Err(e);
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_mpg_wakelocks_and_freezer() {
        let temp_dir = std::env::temp_dir().join("utim_test_mpg");
        let _ = fs::remove_dir_all(&temp_dir);

        let power_dir = temp_dir.join("power");
        let cgroup_dir = temp_dir.join("cgroup");
        let battery_dir = temp_dir.join("battery");

        fs::create_dir_all(&power_dir).unwrap();
        fs::create_dir_all(cgroup_dir.join("user.slice")).unwrap();
        fs::create_dir_all(&battery_dir).unwrap();

        fs::write(power_dir.join("wake_lock"), "").unwrap();
        fs::write(power_dir.join("wake_unlock"), "").unwrap();
        fs::write(power_dir.join("autosleep"), "off\n").unwrap();
        fs::write(cgroup_dir.join("user.slice").join("cgroup.freeze"), "0\n").unwrap();

        let cpu_dir = temp_dir.join("cpu");
        fs::create_dir_all(cpu_dir.join("cpu0/cpufreq")).unwrap();
        fs::write(cpu_dir.join("cpu0/cpufreq/scaling_governor"), "schedutil\n").unwrap();
        fs::write(cpu_dir.join("cpu0/cpufreq/scaling_max_freq"), "2800000\n").unwrap();

        let mut mpg = MobilePowerGovernor::with_paths_all(
            power_dir.clone(),
            cgroup_dir.clone(),
            battery_dir,
            cpu_dir.clone(),
        );

        mpg.acquire_wake_lock("test_lock").unwrap();
        assert!(mpg.has_active_wake_locks());
        let wl_content = fs::read_to_string(power_dir.join("wake_lock")).unwrap();
        assert_eq!(wl_content, "test_lock\n");

        mpg.release_wake_lock("test_lock").unwrap();
        assert!(!mpg.has_active_wake_locks());
        let wul_content = fs::read_to_string(power_dir.join("wake_unlock")).unwrap();
        assert_eq!(wul_content, "test_lock\n");

        mpg.set_cgroup_freeze("user.slice", true).unwrap();
        assert!(mpg.is_cgroup_frozen("user.slice").unwrap());

        mpg.set_cgroup_freeze("user.slice", false).unwrap();
        assert!(!mpg.is_cgroup_frozen("user.slice").unwrap());

        let count = mpg.set_cpu_governor("powersave").unwrap();
        assert_eq!(count, 1);
        let gov = fs::read_to_string(cpu_dir.join("cpu0/cpufreq/scaling_governor")).unwrap();
        assert_eq!(gov, "powersave\n");

        let fcount = mpg.set_cpu_max_frequency_limit(1000000).unwrap();
        assert_eq!(fcount, 1);
        let freq = fs::read_to_string(cpu_dir.join("cpu0/cpufreq/scaling_max_freq")).unwrap();
        assert_eq!(freq, "1000000\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
