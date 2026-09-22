//! Early boot filesystem mounter and Binder devnode setup for PID 1.

use std::ffi::CString;
use std::fs;
use std::io;
use std::path::Path;
use utim_core::fstab::parse_fstab;

pub fn mount_early_filesystems() -> io::Result<()> {
    // 1. Mount /proc
    mount_fs(
        "proc",
        "/proc",
        "proc",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )?;

    // 2. Mount /sys
    mount_fs(
        "sysfs",
        "/sys",
        "sysfs",
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )?;

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
    mount_fs(
        "devpts",
        "/dev/pts",
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("mode=0620,ptmxmode=0666"),
    )?;

    // 5. Mount /dev/shm
    mount_fs(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777"),
    )?;

    // 6. Mount /run
    mount_fs(
        "tmpfs",
        "/run",
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755"),
    )?;

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

    // 8. Populate static /dev character nodes and standard symlinks
    populate_static_dev_nodes();

    // 9. Remount / read-write so userspace disk writes do not fail with EROFS
    let c_root = CString::new("/").map_err(io::Error::other)?;
    unsafe {
        libc::mount(
            std::ptr::null(),
            c_root.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT,
            std::ptr::null(),
        );
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

    for fstab_path in &candidate_fstabs {
        if let Ok(content) = fs::read_to_string(fstab_path) {
            let entries = parse_fstab(&content);
            for entry in entries {
                let mnt = entry.mount_point.as_str();
                if mnt == "/vendor"
                    || mnt == "/odm"
                    || mnt == "/firmware"
                    || mnt == "/dsp"
                    || mnt.starts_with("/apex")
                {
                    let _ = fs::create_dir_all(mnt);
                    let flags = entry.linux_mount_flags();

                    let resolved_src = if Path::new(&entry.src).exists() {
                        entry.src.clone()
                    } else {
                        let name = entry
                            .src
                            .trim_start_matches("/dev/block/mapper/")
                            .trim_start_matches("/dev/block/by-name/")
                            .trim_start_matches("/dev/block/bootdevice/by-name/");
                        let mapper_path = format!("/dev/block/mapper/{}", name);
                        let by_name_path = format!("/dev/block/by-name/{}", name);
                        if Path::new(&mapper_path).exists() {
                            mapper_path
                        } else if Path::new(&by_name_path).exists() {
                            by_name_path
                        } else {
                            entry.src.clone()
                        }
                    };

                    let _ = mount_fs(&resolved_src, mnt, &entry.fs_type, flags, None);
                }
            }
            break;
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

        // Symlink binderfs nodes to /dev
        for node in &["binder", "vndbinder", "hwbinder"] {
            let src = format!("{}/{}", binderfs_dir, node);
            let dst = format!("/dev/{}", node);
            if Path::new(&src).exists() && !Path::new(&dst).exists() {
                let _ = std::os::unix::fs::symlink(&src, &dst);
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
