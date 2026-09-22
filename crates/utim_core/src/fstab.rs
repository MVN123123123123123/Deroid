//! Android fstab parser for mounting /vendor, /odm, /firmware, /dsp, etc.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FstabEntry {
    pub src: String,
    pub mount_point: String,
    pub fs_type: String,
    pub mnt_flags: Vec<String>,
    pub fs_mgr_flags: Vec<String>,
}

impl FstabEntry {
    /// Convert parsed mount flags into Linux sys_mount flags bitmask.
    pub fn linux_mount_flags(&self) -> libc::c_ulong {
        let mut flags = 0;
        for flag in &self.mnt_flags {
            match flag.as_str() {
                "ro" => flags |= libc::MS_RDONLY,
                "nosuid" => flags |= libc::MS_NOSUID,
                "nodev" => flags |= libc::MS_NODEV,
                "noexec" => flags |= libc::MS_NOEXEC,
                "noatime" => flags |= libc::MS_NOATIME,
                "nodiratime" => flags |= libc::MS_NODIRATIME,
                "relatime" => flags |= libc::MS_RELATIME,
                "sync" => flags |= libc::MS_SYNCHRONOUS,
                "remount" => flags |= libc::MS_REMOUNT,
                "bind" => flags |= libc::MS_BIND,
                _ => {}
            }
        }
        flags
    }

    /// Check if this is marked as a logical partition in dynamic partition setup.
    pub fn is_logical(&self) -> bool {
        self.fs_mgr_flags.iter().any(|f| f == "logical")
    }

    /// Check if this entry should be mounted in first stage / early boot.
    pub fn is_first_stage(&self) -> bool {
        self.fs_mgr_flags.iter().any(|f| f == "first_stage_mount")
    }
}

/// Parse an Android fstab content string line by line.
pub fn parse_fstab(content: &str) -> Vec<FstabEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }

        let src = parts[0].to_string();
        let mount_point = parts[1].to_string();
        let fs_type = parts[2].to_string();
        let mnt_flags = parts[3].split(',').map(|s| s.trim().to_string()).collect();
        let fs_mgr_flags = if parts.len() >= 5 {
            parts[4].split(',').map(|s| s.trim().to_string()).collect()
        } else {
            Vec::new()
        };

        entries.push(FstabEntry {
            src,
            mount_point,
            fs_type,
            mnt_flags,
            fs_mgr_flags,
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_android_fstab() {
        let fstab_data = r#"
# Android fstab file.
# <src>                  <mnt_point> <type>  <mnt_flags>                    <fs_mgr_flags>
system                   /system     ext4    ro,barrier=1                   wait,logical,first_stage_mount
vendor                   /vendor     ext4    ro,barrier=1                   wait,logical,first_stage_mount
odm                      /odm        erofs   ro                             wait,logical
/dev/block/by-name/modem /firmware   vfat    ro,shortname=lower,uid=1000    wait
"#;

        let entries = parse_fstab(fstab_data);
        assert_eq!(entries.len(), 4);

        assert_eq!(entries[0].src, "system");
        assert_eq!(entries[0].mount_point, "/system");
        assert_eq!(entries[0].fs_type, "ext4");
        assert!(entries[0].is_logical());
        assert!(entries[0].is_first_stage());
        assert_eq!(
            entries[0].linux_mount_flags() & libc::MS_RDONLY,
            libc::MS_RDONLY
        );

        assert_eq!(entries[1].mount_point, "/vendor");
        assert_eq!(entries[2].mount_point, "/odm");
        assert_eq!(entries[2].fs_type, "erofs");
        assert_eq!(entries[3].mount_point, "/firmware");
        assert_eq!(entries[3].fs_type, "vfat");
    }
}
