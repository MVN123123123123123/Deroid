//! Early boot filesystem mounter and Binder devnode setup for PID 1.

use std::ffi::CString;
use std::fs;
use std::io;
use std::path::Path;
use utim_core::fstab::parse_fstab;

pub fn mount_early_filesystems() -> io::Result<()> {
    // Best-effort: every mount is attempted even if an earlier one failed,
    // so a missing /proc never skips cgroup v2 or /run on odd kernels.
    // The first error is remembered and returned at the end.
    let mut first_err: Option<io::Error> = None;
    let mut attempt = |r: io::Result<()>| {
        if let Err(e) = r {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    };

    // 1. Mount /proc
    attempt(mount_fs(
        "proc",
        "/proc",
        "proc",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    ));

    // 2. Mount /sys
    attempt(mount_fs(
        "sysfs",
        "/sys",
        "sysfs",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    ));

    // 3. Mount /dev (devtmpfs with fallback to tmpfs for kernels with CONFIG_DEVTMPFS=n)
    if mount_fs(
        "devtmpfs",
        "/dev",
        "devtmpfs",
        libc::MS_NOSUID,
        Some("mode=0755"),
    )
    .is_err()
    {
        let _ = mount_fs("tmpfs", "/dev", "tmpfs", libc::MS_NOSUID, Some("mode=0755"));
    }

    // Create standard dev subdirectories
    let _ = fs::create_dir_all("/dev/pts");
    let _ = fs::create_dir_all("/dev/shm");
    let _ = fs::create_dir_all("/dev/socket");

    // 4. Mount /dev/pts
    attempt(mount_fs(
        "devpts",
        "/dev/pts",
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("mode=0620,ptmxmode=0666"),
    ));

    // 5. Mount /dev/shm
    attempt(mount_fs(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777"),
    ));

    // 6. Mount /run
    attempt(mount_fs(
        "tmpfs",
        "/run",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755"),
    ));

    // Create systemd compatibility directory marker!
    // This allows Debian package maintainer scripts (dh_installsystemd / dpkg) to detect that systemd is active.
    let _ = fs::create_dir_all("/run/systemd/system");
    let _ = fs::create_dir_all("/run/utim");

    // 7. Mount cgroups v2
    let _ = fs::create_dir_all("/sys/fs/cgroup");
    let _ = mount_fs(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    );
    let _ = fs::create_dir_all("/sys/fs/cgroup/user.slice");
    let _ = fs::create_dir_all("/sys/fs/cgroup/system.slice");
    let _ = fs::write("/sys/fs/cgroup/cgroup.subtree_control", b"+cpu +memory +io +pids\n");

    // 8. Populate static /dev character nodes and standard symlinks
    populate_static_dev_nodes();

    // 9. Remount / read-write so userspace disk writes do not fail with EROFS.
    // A bare MS_REMOUNT with NULL source/fstype is a kernel no-op; pass an
    // explicit "rw" remount and surface the error instead of ignoring it.
    let c_root = CString::new("/").map_err(io::Error::other)?;
    let c_data = CString::new("rw").map_err(io::Error::other)?;
    let ret = unsafe {
        libc::mount(
            c_root.as_ptr(),
            c_root.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT,
            c_data.as_ptr() as *const libc::c_void,
        )
    };
    if ret != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EBUSY) && first_err.is_none() {
            first_err = Some(err);
        }
    }

    // 10. Guarantee root and tmp permissions for desktop apps (e.g. Firefox)
    unsafe {
        if let Ok(c_root_home) = CString::new("/root") {
            libc::chown(c_root_home.as_ptr(), 0, 0);
            libc::chmod(c_root_home.as_ptr(), 0o700);
        }
        if let Ok(c_tmp_dir) = CString::new("/tmp") {
            libc::chown(c_tmp_dir.as_ptr(), 0, 0);
            libc::chmod(c_tmp_dir.as_ptr(), 0o1777);
        }
    }

    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

fn makedev(major: u32, minor: u32) -> libc::dev_t {
    ((major as libc::dev_t & 0xfff) << 8)
        | (minor as libc::dev_t & 0xff)
        | (((major as libc::dev_t) & !0xfff) << 32)
        | (((minor as libc::dev_t) & !0xff) << 12)
}

fn create_dev_node(path: &str, major: u32, minor: u32, mode: libc::mode_t) {
    if fs::symlink_metadata(path).is_ok() {
        return;
    }
    if let Ok(c_path) = CString::new(path) {
        let dev = makedev(major, minor);
        unsafe {
            libc::mknod(c_path.as_ptr(), libc::S_IFCHR | mode, dev);
        }
    }
}

pub fn populate_static_dev_nodes() {
    create_dev_node("/dev/null", 1, 3, 0o666);
    create_dev_node("/dev/zero", 1, 5, 0o666);
    create_dev_node("/dev/full", 1, 7, 0o666);
    create_dev_node("/dev/random", 1, 8, 0o666);
    create_dev_node("/dev/urandom", 1, 9, 0o666);
    create_dev_node("/dev/console", 5, 1, 0o600);
    create_dev_node("/dev/tty", 5, 0, 0o666);
    create_dev_node("/dev/ptmx", 5, 2, 0o666);

    // Direct Rendering Manager (DRM) and Framebuffer nodes: 0660 like udev
    // (root:video). World-writable GPU nodes let any app modeset the display.
    let _ = fs::create_dir_all("/dev/dri");
    create_dev_node("/dev/dri/card0", 226, 0, 0o660);
    create_dev_node("/dev/dri/renderD128", 226, 128, 0o660);
    create_dev_node("/dev/fb0", 29, 0, 0o660);

    // Input event devices (virtio-input, keyboard, touchscreen/tablet)
    let _ = fs::create_dir_all("/dev/input");
    for i in 0..8 {
        create_dev_node(&format!("/dev/input/event{}", i), 13, 64 + i, 0o660);
    }
    create_dev_node("/dev/input/mice", 13, 63, 0o660);

    let symlinks = [
        ("/proc/self/fd", "/dev/fd"),
        ("/proc/self/fd/0", "/dev/stdin"),
        ("/proc/self/fd/1", "/dev/stdout"),
        ("/proc/self/fd/2", "/dev/stderr"),
    ];
    for (src, dst) in symlinks {
        if fs::symlink_metadata(dst).is_err() {
            let _ = std::os::unix::fs::symlink(src, dst);
        }
    }
}

pub fn place_in_cgroup(pid: libc::pid_t) {
    let system_slice = "/sys/fs/cgroup/system.slice/cgroup.procs";
    let root_cgroup = "/sys/fs/cgroup/cgroup.procs";
    if Path::new(system_slice).exists() {
        let _ = fs::write(system_slice, format!("{}\n", pid));
    } else if Path::new(root_cgroup).exists() {
        let _ = fs::write(root_cgroup, format!("{}\n", pid));
    }
}

fn get_slot_suffix() -> Option<String> {
    if let Ok(cmdline) = fs::read_to_string("/proc/cmdline") {
        for arg in cmdline.split_whitespace() {
            if let Some(val) = arg.strip_prefix("androidboot.slot_suffix=") {
                return Some(val.to_string());
            }
            if let Some(val) = arg.strip_prefix("androidboot.slot=") {
                let s = val.trim();
                return Some(if s.starts_with('_') {
                    s.to_string()
                } else {
                    format!("_{}", s)
                });
            }
        }
    }
    None
}

/// Parse vendor fstab and mount partitions (/vendor, /odm, /apex, /firmware, /dsp)
pub fn mount_vendor_partitions() -> io::Result<()> {
    let candidate_fstabs = [
        "/vendor/etc/fstab.ranchu",
        "/vendor/etc/fstab.default",
        "/vendor/etc/fstab.qcom",
        "/vendor/etc/fstab.mtk",
        "/odm/etc/fstab.default",
        "/fstab.ranchu",
        "/fstab.default",
    ];

    let slot_suffix = get_slot_suffix();
    let mut mounted_targets = std::collections::HashSet::new();

    for fstab_path in &candidate_fstabs {
        let Ok(content) = fs::read_to_string(fstab_path) else {
            continue;
        };
        let entries = parse_fstab(&content);
        if entries.is_empty() {
            // An empty fstab must not shadow later candidates.
            continue;
        }
        for entry in entries {
            let mnt = entry.mount_point.as_str();
            if mounted_targets.contains(mnt) {
                continue;
            }
            // Honour the vendor's noauto: a partition marked noauto must be
            // mounted on demand, never automatically at boot.
            if entry.is_noauto() {
                continue;
            }
            if mnt == "/vendor"
                || mnt == "/odm"
                || mnt == "/product"
                || mnt == "/system_ext"
                || mnt == "/vendor_dlkm"
                || mnt == "/odm_dlkm"
                || mnt == "/firmware"
                || mnt == "/dsp"
                || mnt.starts_with("/apex")
            {
                let _ = fs::create_dir_all(mnt);
                let flags = entry.linux_mount_flags();

                let base_name = entry
                    .src
                    .trim_start_matches("/dev/block/mapper/")
                    .trim_start_matches("/dev/block/by-name/")
                    .trim_start_matches("/dev/block/bootdevice/by-name/");

                let mut candidates = Vec::new();
                if Path::new(&entry.src).exists() {
                    candidates.push(entry.src.clone());
                }
                if let Some(ref suffix) = slot_suffix {
                    let suffixed = if base_name.ends_with(suffix) {
                        base_name.to_string()
                    } else {
                        format!("{}{}", base_name, suffix)
                    };
                    candidates.push(format!("/dev/block/mapper/{}", suffixed));
                    candidates.push(format!("/dev/block/by-name/{}", suffixed));
                    candidates.push(format!("/dev/block/bootdevice/by-name/{}", suffixed));
                }
                candidates.push(format!("/dev/block/mapper/{}", base_name));
                candidates.push(format!("/dev/block/by-name/{}", base_name));
                candidates.push(format!("/dev/block/bootdevice/by-name/{}", base_name));
                candidates.push(entry.src.clone());

                let resolved_src = candidates
                    .into_iter()
                    .find(|p| Path::new(p).exists())
                    .unwrap_or_else(|| entry.src.clone());

                let data_owned = entry.mount_data_owned();
                if mount_fs(
                    &resolved_src,
                    mnt,
                    &entry.fs_type,
                    flags,
                    data_owned.as_deref(),
                )
                .is_ok()
                {
                    mounted_targets.insert(entry.mount_point.clone());
                }
            }
        }
    }

    Ok(())
}

#[repr(C)]
struct BinderfsDevice {
    name: [libc::c_char; 256],
    major: u32,
    minor: u32,
}

const BINDER_CTL_ADD: libc::c_ulong = 0xc1086201;

/// Set up Android Binder devnodes (/dev/binder, /dev/vndbinder, /dev/hwbinder)
pub fn setup_binder_devnodes() -> io::Result<()> {
    // If binderfs is supported in kernel (/dev/binderfs), mount it
    let binderfs_dir = "/dev/binderfs";
    let binder_control = format!("{}/binder-control", binderfs_dir);

    if Path::new(binderfs_dir).exists() || fs::create_dir_all(binderfs_dir).is_ok() {
        let _ = mount_fs("binder", binderfs_dir, "binder", 0, None);

        // If binder-control exists, allocate standard devices via BINDER_CTL_ADD
        if Path::new(&binder_control).exists() {
            let c_ctl = CString::new(binder_control).unwrap();
            let fd = unsafe { libc::open(c_ctl.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd >= 0 {
                for node_name in &["binder", "vndbinder", "hwbinder"] {
                    let mut dev = BinderfsDevice {
                        name: [0; 256],
                        major: 0,
                        minor: 0,
                    };
                    for (i, b) in node_name.bytes().enumerate() {
                        dev.name[i] = b as libc::c_char;
                    }
                    unsafe {
                        libc::ioctl(fd, BINDER_CTL_ADD as _, &mut dev);
                    }
                }
                unsafe {
                    libc::close(fd);
                }
            }
        }

        // Symlink binderfs nodes to /dev and chmod 0666
        for node in &["binder", "vndbinder", "hwbinder"] {
            let src = format!("{}/{}", binderfs_dir, node);
            let dst = format!("/dev/{}", node);
            if let Ok(c_src) = CString::new(src.as_str()) {
                unsafe {
                    libc::chmod(c_src.as_ptr(), 0o666);
                }
            }
            if Path::new(&src).exists() && !Path::new(&dst).exists() {
                let _ = std::os::unix::fs::symlink(&src, &dst);
            }
            if let Ok(c_dst) = CString::new(dst.as_str()) {
                unsafe {
                    libc::chmod(c_dst.as_ptr(), 0o666);
                }
            }
        }
    }

    // Also ensure any existing /dev/binder* nodes have 0o666 permissions
    for node in &["binder", "vndbinder", "hwbinder"] {
        let dst = format!("/dev/{}", node);
        if let Ok(c_dst) = CString::new(dst.as_str()) {
            unsafe {
                libc::chmod(c_dst.as_ptr(), 0o666);
            }
        }
    }

    Ok(())
}

fn mount_fs(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let c_source = CString::new(source)?;
    let c_target = CString::new(target)?;
    let c_fstype = CString::new(fstype)?;
    let c_data = data.map(CString::new).transpose()?;

    let ret = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            flags,
            c_data
                .as_ref()
                .map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
        )
    };

    if ret != 0 {
        let err = io::Error::last_os_error();
        // EBUSY is fine (already mounted)
        if err.raw_os_error() != Some(libc::EBUSY) {
            return Err(err);
        }
    }

    Ok(())
}
