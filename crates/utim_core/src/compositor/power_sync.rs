//! UTIM Mobile Power Governor (MPG) and Memory Supervisor (MMPS) Synchronization.
//! Communicates with UTIM PID 1 over /run/utim/control.sock:
//! triggers cgroup v2 freezing (user.slice) on display sleep,
//! manages wakelocks, and enforces the mobile dynamic oom_score_adj hierarchy.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ipc::{send_ipc_request, IpcRequest, IpcResponse, DEFAULT_CONTROL_SOCKET};

/// Dynamic OOM score roles defined in Universal Treble GSI blueprint
pub mod oom_roles {
    pub const UTLC_COMPOSITOR: i32 = -900;
    pub const ACTIVE_FOREGROUND_APP: i32 = 0;
    pub const RECENTS_CACHED_APP: i32 = 200;
    pub const BACKGROUND_INACTIVE_APP: i32 = 800;
}

/// Power saver operational modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerSaverMode {
    #[default]
    Off,
    Normal,
    SuperExtreme,
}

pub struct UtimPowerSync {
    pub socket_path: PathBuf,
    pub is_display_on: bool,
    pub active_foreground_pid: Option<i32>,
    pub recents_pids: Vec<i32>,
    pub is_connected: bool,
    pub power_saver_mode: PowerSaverMode,
}

impl Default for UtimPowerSync {
    fn default() -> Self {
        Self::new(Path::new(DEFAULT_CONTROL_SOCKET))
    }
}

impl UtimPowerSync {
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
            is_display_on: true,
            active_foreground_pid: None,
            recents_pids: Vec::new(),
            is_connected: false,
            power_saver_mode: PowerSaverMode::Off,
        }
    }

    /// Set UTLC's own oom_score_adj to -900 (Immune from OOM killer).
    /// Requires CAP_SYS_RESOURCE; reports the failure instead of silently
    /// running killable while the caller assumes immunity.
    pub fn configure_self_oom_score(&self) -> std::io::Result<()> {
        std::fs::write(
            "/proc/self/oom_score_adj",
            format!("{}\n", oom_roles::UTLC_COMPOSITOR),
        )?;
        Ok(())
    }

    /// Display sleep event (DPMS off or lock screen timeout)
    /// Triggers UTIM to freeze user application slice (cgroup.freeze = 1)
    /// and release display wakelock, allowing deep autosleep (<= 0.8% / hr).
    pub fn on_display_sleep(&mut self) -> Result<(), String> {
        // 1. Freeze user.slice via UTIM
        let req_freeze = IpcRequest::FreezeCgroup("user.slice".into());
        Self::ensure_ok(self.send_request(&req_freeze)?, "freeze user.slice")?;

        // 2. Release UTLC display wakelock
        let req_wl = IpcRequest::ReleaseWakeLock("utlc-display".into());
        Self::ensure_ok(self.send_request(&req_wl)?, "release utlc-display wakelock")?;

        self.is_display_on = false;
        Ok(())
    }

    /// Display wake event (Power button / Touch / Fingerprint)
    /// Triggers UTIM to unfreeze user application slice in < 150us.
    pub fn on_display_wake(&mut self) -> Result<(), String> {
        // 1. Acquire display wakelock
        let req_wl = IpcRequest::AcquireWakeLock("utlc-display".into());
        Self::ensure_ok(self.send_request(&req_wl)?, "acquire utlc-display wakelock")?;

        // 2. Unfreeze user.slice via UTIM
        let req_unfreeze = IpcRequest::UnfreezeCgroup("user.slice".into());
        Self::ensure_ok(self.send_request(&req_unfreeze)?, "unfreeze user.slice")?;

        self.is_display_on = true;
        Ok(())
    }

    /// Set the active power saver mode.
    pub fn set_power_saver_mode(&mut self, mode: PowerSaverMode) -> Result<(), String> {
        match mode {
            PowerSaverMode::Off => self.restore_normal_power_mode()?,
            PowerSaverMode::Normal => self.apply_normal_power_saver()?,
            PowerSaverMode::SuperExtreme => self.apply_super_extreme_power_saver()?,
        }
        self.power_saver_mode = mode;
        Ok(())
    }

    /// Normal power saver mode:
    /// Freeze background inactive apps when not in foreground.
    pub fn apply_normal_power_saver(&mut self) -> Result<(), String> {
        if self.socket_path.exists() {
            let req_freeze = IpcRequest::FreezeCgroup("user.slice".into());
            let _ = self.send_request(&req_freeze);
        }
        self.power_saver_mode = PowerSaverMode::Normal;
        Ok(())
    }

    /// Super extreme power saver mode:
    /// Freeze user application slice entirely, release display wakelock for deepest sleep.
    pub fn apply_super_extreme_power_saver(&mut self) -> Result<(), String> {
        if self.socket_path.exists() {
            let req_freeze = IpcRequest::FreezeCgroup("user.slice".into());
            let _ = self.send_request(&req_freeze);
            let req_wl = IpcRequest::ReleaseWakeLock("utlc-display".into());
            let _ = self.send_request(&req_wl);
        }
        self.power_saver_mode = PowerSaverMode::SuperExtreme;
        Ok(())
    }

    /// Restore standard power mode: unfreeze user application slices.
    pub fn restore_normal_power_mode(&mut self) -> Result<(), String> {
        if self.socket_path.exists() {
            let req_unfreeze = IpcRequest::UnfreezeCgroup("user.slice".into());
            let _ = self.send_request(&req_unfreeze);
            let req_wl = IpcRequest::AcquireWakeLock("utlc-display".into());
            let _ = self.send_request(&req_wl);
        }
        self.power_saver_mode = PowerSaverMode::Off;
        Ok(())
    }

    /// Synchronize dynamic OOM score hierarchy when switching apps.
    /// Pids that dropped out of every managed list are explicitly demoted
    /// to the kernel default instead of keeping a stale boost forever.
    pub fn on_app_switched(
        &mut self,
        new_fg_pid: i32,
        recents: &[i32],
        inactive: &[i32],
    ) -> Result<(), String> {
        let recents_set: HashSet<i32> = recents.iter().copied().collect();
        let new_set: HashSet<i32> = std::iter::once(new_fg_pid)
            .chain(recents.iter().copied())
            .chain(inactive.iter().copied())
            .collect();

        // Explicit demotion for pids we managed before but no longer do.
        let prev: HashSet<i32> = self
            .active_foreground_pid
            .into_iter()
            .chain(self.recents_pids.iter().copied())
            .collect();
        for stale in prev.difference(&new_set) {
            let req = IpcRequest::SetOomScore(*stale, 0); // kernel default
            Self::ensure_ok(self.send_request(&req)?, "demote stale oom score")?;
        }

        // 1. Foreground app -> 0
        let req_fg = IpcRequest::SetOomScore(new_fg_pid, oom_roles::ACTIVE_FOREGROUND_APP);
        Self::ensure_ok(self.send_request(&req_fg)?, "set foreground oom score")?;

        // 2. Recents cached apps -> +200
        for &pid in recents {
            if pid != new_fg_pid {
                let req_rec = IpcRequest::SetOomScore(pid, oom_roles::RECENTS_CACHED_APP);
                Self::ensure_ok(self.send_request(&req_rec)?, "set recents oom score")?;
            }
        }

        // 3. Background inactive apps -> +800
        for &pid in inactive {
            if pid != new_fg_pid && !recents_set.contains(&pid) {
                let req_inact = IpcRequest::SetOomScore(pid, oom_roles::BACKGROUND_INACTIVE_APP);
                Self::ensure_ok(self.send_request(&req_inact)?, "set inactive oom score")?;
            }
        }

        // Only record the new hierarchy once every request was accepted:
        // on error the state still describes what was actually applied.
        self.active_foreground_pid = Some(new_fg_pid);
        self.recents_pids = recents.to_vec();

        Ok(())
    }

    /// A non-Ok daemon reply is a failure, not a success.
    fn ensure_ok(resp: IpcResponse, what: &str) -> Result<(), String> {
        match resp {
            IpcResponse::Ok(_) => Ok(()),
            other => Err(format!("{}: unexpected UTIM reply {:?}", what, other)),
        }
    }

    fn send_request(&mut self, req: &IpcRequest) -> Result<IpcResponse, String> {
        if !self.socket_path.exists() {
            self.is_connected = false;
            return Err(format!(
                "utim control socket {} absent",
                self.socket_path.display()
            ));
        }

        match send_ipc_request(&self.socket_path, req) {
            Ok(resp) => {
                self.is_connected = true;
                Ok(resp)
            }
            Err(e) => {
                self.is_connected = false;
                Err(format!("IPC failed: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_power_sync_sleep_and_wake() {
        let mut sync = UtimPowerSync::new(Path::new("/tmp/nonexistent-utim-ctrl.sock"));
        assert!(sync.is_display_on);

        // No daemon socket: failures are reported, never mocked as success.
        assert!(sync.on_display_sleep().is_err());
        assert!(!sync.is_connected);
        assert!(sync.is_display_on); // state untouched on failure

        assert!(sync.on_display_wake().is_err());
        assert!(sync.is_display_on);
    }

    #[test]
    fn test_app_switching_oom_hierarchy() {
        let mut sync = UtimPowerSync::new(Path::new("/tmp/nonexistent-utim-ctrl.sock"));
        let fg = 101;
        let recents = vec![102, 103];
        let inactive = vec![104, 105];

        assert!(sync.on_app_switched(fg, &recents, &inactive).is_err());
        // Nothing was applied without a daemon to talk to.
        assert_eq!(sync.active_foreground_pid, None);
        assert!(sync.recents_pids.is_empty());
    }

    #[test]
    fn test_power_saver_modes() {
        let mut sync = UtimPowerSync::new(Path::new("/tmp/nonexistent-utim-ctrl.sock"));
        assert_eq!(sync.power_saver_mode, PowerSaverMode::Off);

        sync.set_power_saver_mode(PowerSaverMode::Normal).unwrap();
        assert_eq!(sync.power_saver_mode, PowerSaverMode::Normal);

        sync.set_power_saver_mode(PowerSaverMode::SuperExtreme)
            .unwrap();
        assert_eq!(sync.power_saver_mode, PowerSaverMode::SuperExtreme);

        sync.set_power_saver_mode(PowerSaverMode::Off).unwrap();
        assert_eq!(sync.power_saver_mode, PowerSaverMode::Off);
    }
}
