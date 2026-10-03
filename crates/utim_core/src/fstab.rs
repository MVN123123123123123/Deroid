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
            // `flag` may carry a `k=v` payload (uid=, barrier=); only the key
            // selects the VFS flag. Positive forms (suid/dev/exec/atime/...)
            // are the kernel default, so they consume the token and set
            // nothing; `defaults`/`auto` are pure no-ops.
            let key = flag.split_once('=').map_or(flag.as_str(), |(k, _)| k);
            match key {
                "ro" => flags |= libc::MS_RDONLY,
                "nosuid" => flags |= libc::MS_NOSUID,
                "nodev" => flags |= libc::MS_NODEV,
                "noexec" => flags |= libc::MS_NOEXEC,
                "noatime" => flags |= libc::MS_NOATIME,
                "nodiratime" => flags |= libc::MS_NODIRATIME,
                "relatime" => flags |= libc::MS_RELATIME,
                "strictatime" => flags |= libc::MS_STRICTATIME,
                "dirsync" => flags |= libc::MS_DIRSYNC,
                "sync" => flags |= libc::MS_SYNCHRONOUS,
                "remount" => flags |= libc::MS_REMOUNT,
                "bind" => flags |= libc::MS_BIND,
                "rbind" => flags |= libc::MS_BIND | libc::MS_REC,
                "rw" | "suid" | "dev" | "exec" | "atime" | "diratime" | "async" | "defaults"
                | "auto" | "noauto" | "user" | "users" | "nostrictatime" => {}
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

    /// Whether this entry must NOT be mounted automatically (`noauto` /
    /// `auto-fail` in the vendor fstab). The mount loop skips such entries.
    pub fn is_noauto(&self) -> bool {
        self.mnt_flags
            .iter()
            .any(|f| f == "noauto" || f == "auto-fail")
    }

    /// Flags consumed by `linux_mount_flags`; everything else is a
    /// filesystem `data=` option. `defaults` is a no-op, not a data option.
    fn is_vfs_flag(flag: &str) -> bool {
        matches!(
            flag.split_once('=').map_or(flag, |(k, _)| k),
            "ro" | "rw"
                | "defaults"
                | "noauto"
                | "auto"
                | "user"
                | "users"
                | "suid"
                | "nosuid"
                | "dev"
                | "nodev"
                | "exec"
                | "noexec"
                | "atime"
                | "noatime"
                | "diratime"
                | "nodiratime"
                | "relatime"
                | "strictatime"
                | "nostrictatime"
                | "sync"
                | "async"
                | "dirsync"
                | "remount"
                | "bind"
                | "rbind"
        )
    }

    /// Owned mount(2) `data` string with fs-specific options (uid=,
    /// shortname=, barrier=, ...). Bare MS_* keywords are consumed by
    /// [`FstabEntry::linux_mount_flags`]; everything else is forwarded.
    /// Returns None when there is nothing to forward.
    pub fn mount_data_owned(&self) -> Option<String> {
        let extra: Vec<&str> = self
            .mnt_flags
            .iter()
            .filter(|flag| !Self::is_vfs_flag(flag) && !flag.is_empty())
            .map(|s| s.as_str())
            .collect();
        if extra.is_empty() {
            None
        } else {
            Some(extra.join(","))
        }
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

    #[test]
    fn test_rbind_maps_to_bind_rec() {
        // S11: rbind must perform a recursive bind, not leak "rbind" to the fs driver.
        let entries = parse_fstab("src /product none rbind wait\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].linux_mount_flags() & (libc::MS_BIND | libc::MS_REC),
            libc::MS_BIND | libc::MS_REC
        );
        assert_eq!(entries[0].mount_data_owned(), None);
    }

    #[test]
    fn test_defaults_is_a_noop_not_data() {
        // S11: `defaults` sets no VFS bit and must not reach mount(2) data.
        let entries = parse_fstab("src /vendor ext4 defaults,noatime wait\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].linux_mount_flags() & libc::MS_NOATIME,
            libc::MS_NOATIME
        );
        assert_eq!(entries[0].mount_data_owned(), None);
    }

    #[test]
    fn test_noauto_entries_are_marked() {
        // S11: the mount loop skips entries the vendor marked noauto.
        let entries = parse_fstab("src /vendor ext4 defaults,noatime,noauto wait\n");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_noauto());
        assert_eq!(entries[0].mount_data_owned(), None);

        let entries = parse_fstab("src /odm erofs ro wait,logical\n");
        assert!(!entries[0].is_noauto());
    }

    #[test]
    fn test_positive_permission_forms_consumed_not_forwarded() {
        // S11: suid/dev/exec are the kernel default (no bit to set) and must
        // not be forwarded to the filesystem driver as data options.
        let entries = parse_fstab("src /dsp ext4 ro,suid,dev,exec wait\n");
        assert_eq!(entries.len(), 1);
        let flags = entries[0].linux_mount_flags();
        assert_eq!(flags & libc::MS_NOSUID, 0);
        assert_eq!(flags & libc::MS_NODEV, 0);
        assert_eq!(flags & libc::MS_NOEXEC, 0);
        assert_eq!(entries[0].mount_data_owned(), None);
    }

    #[test]
    fn test_fs_options_still_forwarded() {
        // The classifier must not swallow real fs options.
        let entries = parse_fstab("src /system ext4 ro,barrier=1 wait\n");
        assert_eq!(entries[0].mount_data_owned().as_deref(), Some("barrier=1"));
    }
}
