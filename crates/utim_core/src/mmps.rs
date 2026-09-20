//! Mobile Memory Pressure Supervisor (MMPS) for Linux PSI and oom_score_adj hierarchy.

use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPriorityRole {
    InitPid1,          // -1000: Immune from OOM
    Compositor,        // -900: Display & Window Manager
    TelephonyAudio,    // -800: In-call audio, cellular radio, emergency services
    AndroidHal,        // -700: Vendor HAL daemons (HWC, Audio, Camera)
    ForegroundApp,     // 0: Active focused application
    BackgroundService, // +300: Standard daemons (sshd, cups, avahi)
    BackgroundApp,     // +800 to +1000: Cached apps and inactive tabs (first killed)
}

impl ProcessPriorityRole {
    pub fn oom_score_adj(&self) -> i32 {
        match self {
            ProcessPriorityRole::InitPid1 => -1000,
            ProcessPriorityRole::Compositor => -900,
            ProcessPriorityRole::TelephonyAudio => -800,
            ProcessPriorityRole::AndroidHal => -700,
            ProcessPriorityRole::ForegroundApp => 0,
            ProcessPriorityRole::BackgroundService => 300,
            ProcessPriorityRole::BackgroundApp => 800,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryPressureMetrics {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub full_avg300: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressureLevel {
    None,
    Low,
    Medium,
    Critical,
}

pub struct MemorySupervisor {
    proc_pressure_path: PathBuf,
}

impl Default for MemorySupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySupervisor {
    pub fn new() -> Self {
        Self::with_path(PathBuf::from("/proc/pressure/memory"))
    }

    pub fn with_path(proc_pressure_path: PathBuf) -> Self {
        Self { proc_pressure_path }
    }

    /// Read and parse Linux PSI (/proc/pressure/memory).
    pub fn read_psi(&self) -> io::Result<MemoryPressureMetrics> {
        let content = fs::read_to_string(&self.proc_pressure_path)?;
        parse_psi_output(&content).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Failed to parse PSI memory content")
        })
    }

    /// Determine memory pressure severity level.
    pub fn evaluate_pressure_level(&self) -> MemoryPressureLevel {
        match self.read_psi() {
            Ok(metrics) => {
                if metrics.full_avg10 > 25.0 || metrics.some_avg10 > 60.0 {
                    MemoryPressureLevel::Critical
                } else if metrics.full_avg10 > 10.0 || metrics.some_avg10 > 30.0 {
                    MemoryPressureLevel::Medium
                } else if metrics.some_avg10 > 10.0 {
                    MemoryPressureLevel::Low
                } else {
                    MemoryPressureLevel::None
                }
            }
            Err(_) => MemoryPressureLevel::None,
        }
    }

    /// Apply oom_score_adj to a target process.
    pub fn apply_oom_score_adj(pid: i32, score: i32) -> io::Result<()> {
        let path = format!("/proc/{}/oom_score_adj", pid);
        let clamped = score.clamp(-1000, 1000);
        fs::write(path, format!("{}\n", clamped))
    }

    /// Apply role-based oom_score_adj to a target process.
    pub fn apply_role_score(pid: i32, role: ProcessPriorityRole) -> io::Result<()> {
        Self::apply_oom_score_adj(pid, role.oom_score_adj())
    }
}

pub fn parse_psi_output(content: &str) -> Option<MemoryPressureMetrics> {
    let mut some_avg10 = 0.0;
    let mut some_avg60 = 0.0;
    let mut some_avg300 = 0.0;
    let mut full_avg10 = 0.0;
    let mut full_avg60 = 0.0;
    let mut full_avg300 = 0.0;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("some ") {
            for token in rest.split_whitespace() {
                if let Some((k, v)) = token.split_once('=') {
                    match k {
                        "avg10" => some_avg10 = v.parse().unwrap_or(0.0),
                        "avg60" => some_avg60 = v.parse().unwrap_or(0.0),
                        "avg300" => some_avg300 = v.parse().unwrap_or(0.0),
                        _ => {}
                    }
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("full ") {
            for token in rest.split_whitespace() {
                if let Some((k, v)) = token.split_once('=') {
                    match k {
                        "avg10" => full_avg10 = v.parse().unwrap_or(0.0),
                        "avg60" => full_avg60 = v.parse().unwrap_or(0.0),
                        "avg300" => full_avg300 = v.parse().unwrap_or(0.0),
                        _ => {}
                    }
                }
            }
        }
    }

    Some(MemoryPressureMetrics {
        some_avg10,
        some_avg60,
        some_avg300,
        full_avg10,
        full_avg60,
        full_avg300,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_psi() {
        let sample = r#"
some avg10=12.50 avg60=8.20 avg300=4.10 total=123456
full avg10=26.80 avg60=14.00 avg300=5.50 total=45678
"#;
        let metrics = parse_psi_output(sample).unwrap();
        assert_eq!(metrics.some_avg10, 12.50);
        assert_eq!(metrics.full_avg10, 26.80);

        let temp_psi = std::env::temp_dir().join("test_psi_mem");
        fs::write(&temp_psi, sample).unwrap();

        let mmps = MemorySupervisor::with_path(temp_psi.clone());
        assert_eq!(mmps.evaluate_pressure_level(), MemoryPressureLevel::Critical);

        let _ = fs::remove_file(temp_psi);
    }

    #[test]
    fn test_role_scores() {
        assert_eq!(ProcessPriorityRole::InitPid1.oom_score_adj(), -1000);
        assert_eq!(ProcessPriorityRole::Compositor.oom_score_adj(), -900);
        assert_eq!(ProcessPriorityRole::TelephonyAudio.oom_score_adj(), -800);
        assert_eq!(ProcessPriorityRole::AndroidHal.oom_score_adj(), -700);
        assert_eq!(ProcessPriorityRole::ForegroundApp.oom_score_adj(), 0);
        assert_eq!(ProcessPriorityRole::BackgroundService.oom_score_adj(), 300);
        assert_eq!(ProcessPriorityRole::BackgroundApp.oom_score_adj(), 800);
    }
}
