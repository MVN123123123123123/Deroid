//! Mobile Power Governor (MPG) for Android WakeLock and Cgroup v2 Freezing.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

pub struct MobilePowerGovernor {
    sys_power_dir: PathBuf,
    cgroup_root: PathBuf,
    battery_dir: PathBuf,
    active_wake_locks: HashSet<String>,
    charge_limit_percent: u32,
}

impl Default for MobilePowerGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl MobilePowerGovernor {
    pub fn new() -> Self {
        Self::with_paths(
            PathBuf::from("/sys/power"),
            PathBuf::from("/sys/fs/cgroup"),
            PathBuf::from("/sys/class/power_supply/battery"),
        )
    }

    pub fn with_paths(sys_power_dir: PathBuf, cgroup_root: PathBuf, battery_dir: PathBuf) -> Self {
        Self {
            sys_power_dir,
            cgroup_root,
            battery_dir,
            active_wake_locks: HashSet::new(),
            charge_limit_percent: 80,
        }
    }

    /// Acquire a partial Android kernel wake lock.
    pub fn acquire_wake_lock(&mut self, name: &str) -> io::Result<()> {
        let wake_lock_path = self.sys_power_dir.join("wake_lock");
        if wake_lock_path.exists() {
            let mut file = OpenOptions::new().write(true).open(&wake_lock_path)?;
            file.write_all(name.as_bytes())?;
            file.flush()?;
        }
        self.active_wake_locks.insert(name.to_string());
        Ok(())
    }

    /// Release a held Android kernel wake lock.
    pub fn release_wake_lock(&mut self, name: &str) -> io::Result<()> {
        let wake_unlock_path = self.sys_power_dir.join("wake_unlock");
        if wake_unlock_path.exists() {
            let mut file = OpenOptions::new().write(true).open(&wake_unlock_path)?;
            file.write_all(name.as_bytes())?;
            file.flush()?;
        }
        self.active_wake_locks.remove(name);
        Ok(())
    }

    /// Check if any wake locks are currently held.
    pub fn has_active_wake_locks(&self) -> bool {
        !self.active_wake_locks.is_empty()
    }

    pub fn active_wake_locks(&self) -> &HashSet<String> {
        &self.active_wake_locks
    }

    /// Freeze or unfreeze a cgroup v2 slice (e.g. "user.slice").
    /// Writing "1" to cgroup.freeze stops all processes in that slice with zero CPU wakeups.
    pub fn set_cgroup_freeze(&self, slice_name: &str, freeze: bool) -> io::Result<()> {
        let slice_dir = self.cgroup_root.join(slice_name);
        let freeze_file = slice_dir.join("cgroup.freeze");
        if freeze_file.exists() {
            let val = if freeze { "1\n" } else { "0\n" };
            fs::write(freeze_file, val)?;
        }
        Ok(())
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
    pub fn configure_autosleep(&self, enable: bool) -> io::Result<()> {
        let autosleep_file = self.sys_power_dir.join("autosleep");
        if autosleep_file.exists() {
            let state = if enable { "mem\n" } else { "off\n" };
            fs::write(autosleep_file, state)?;
        }
        Ok(())
    }

    /// Apply battery charge limit threshold (e.g. 80%) to preserve battery longevity.
    pub fn set_charge_limit(&mut self, limit_pct: u32) -> io::Result<()> {
        self.charge_limit_percent = limit_pct.min(100);
        let limit_file = self.battery_dir.join("charge_control_limit_max");
        if limit_file.exists() {
            fs::write(limit_file, format!("{}\n", self.charge_limit_percent))?;
        }
        Ok(())
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

        let mut mpg = MobilePowerGovernor::with_paths(power_dir.clone(), cgroup_dir.clone(), battery_dir);

        mpg.acquire_wake_lock("test_lock").unwrap();
        assert!(mpg.has_active_wake_locks());
        let wl_content = fs::read_to_string(power_dir.join("wake_lock")).unwrap();
        assert_eq!(wl_content, "test_lock");

        mpg.release_wake_lock("test_lock").unwrap();
        assert!(!mpg.has_active_wake_locks());
        let wul_content = fs::read_to_string(power_dir.join("wake_unlock")).unwrap();
        assert_eq!(wul_content, "test_lock");

        mpg.set_cgroup_freeze("user.slice", true).unwrap();
        assert!(mpg.is_cgroup_frozen("user.slice").unwrap());

        mpg.set_cgroup_freeze("user.slice", false).unwrap();
        assert!(!mpg.is_cgroup_frozen("user.slice").unwrap());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
