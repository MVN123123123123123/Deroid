//! deb-systemd-invoke: Debian policy-rc.d aware service action invoker.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const VALID_ACTIONS: &[&str] = &[
    "start",
    "stop",
    "restart",
    "reload",
    "force-reload",
    "try-restart",
    "status",
    "enable",
    "disable",
    "mask",
    "unmask",
    "reenable",
    "reload-or-restart",
];

fn policy_script() -> PathBuf {
    match env::var("DPKG_ROOT") {
        Ok(r) if !r.is_empty() => Path::new(&r).join("usr/sbin/policy-rc.d"),
        _ => PathBuf::from("/usr/sbin/policy-rc.d"),
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Parse --root/--chroot FIRST: the offline guard below must consult the
    // chroot's /run/systemd/system, not the host's (S3).
    let mut root_dir: Option<String> = None;
    let mut clean_args: Vec<&str> = Vec::new();
    let mut no_action = false;
    let mut no_wait = false;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--root" || arg == "--chroot" {
            match args.get(i + 1) {
                Some(v) if !v.starts_with('-') => {
                    i += 1;
                    root_dir = Some(args[i].clone());
                }
                _ => {
                    eprintln!("deb-systemd-invoke: {arg} requires a value");
                    process::exit(1);
                }
            }
        } else if let Some(v) = arg
            .strip_prefix("--root=")
            .or_else(|| arg.strip_prefix("--chroot="))
        {
            if v.is_empty() || v.starts_with('-') {
                eprintln!("deb-systemd-invoke: {arg} requires a value");
                process::exit(1);
            }
            root_dir = Some(v.to_string());
        } else if arg == "--no-action" {
            // The whole purpose of --no-action is to suppress the call.
            no_action = true;
        } else if arg == "--no-wait" || arg == "--no-block" {
            no_wait = true;
        } else if !arg.starts_with("--") {
            clean_args.push(arg.as_str());
        }
        // Every other `--` token is silently discarded for compatibility
        // (--quiet, --user, ...); only --no-action/--no-wait change behaviour.
        i += 1;
    }

    // Debian init-system-helpers specification:
    // If /run/systemd/system does not exist, systemd / UTIM is not running as PID 1
    // (e.g. during chroot bootstrap, container build, or offline installation).
    // All service actions must exit cleanly with 0.
    // The probe consults the chroot when --root was given, never the host (S3).
    let run_systemd_dir =
        env::var("DEB_SYSTEMD_SYSTEM_DIR").unwrap_or_else(|_| "/run/systemd/system".to_string());
    let probe: PathBuf = match &root_dir {
        Some(r) => Path::new(r).join("run/systemd/system"),
        None => PathBuf::from(&run_systemd_dir),
    };
    if !probe.exists() {
        process::exit(0);
    }

    if clean_args.len() < 2 {
        eprintln!("Usage: deb-systemd-invoke <action> <unit>...");
        process::exit(1);
    }

    let action = clean_args[0];
    let units = &clean_args[1..];

    if !VALID_ACTIONS.contains(&action) {
        eprintln!(
            "deb-systemd-invoke: unknown action {action:?}; expected one of {VALID_ACTIONS:?}"
        );
        process::exit(1);
    }

    if no_action {
        process::exit(0);
    }

    // Policy check is gated on the validated action, so an unknown action
    // can never bypass policy-rc.d by skipping validation (S17).
    let should_check_policy = matches!(
        action,
        "start" | "restart" | "reload" | "try-restart" | "force-reload"
    );

    let mut allowed_units = Vec::new();
    for &unit in units {
        let base_name = unit.strip_suffix(".service").unwrap_or(unit);
        if should_check_policy && is_action_forbidden_by_policy(base_name, action) {
            println!(
                "deb-systemd-invoke: action '{}' on unit '{}' forbidden by policy-rc.d",
                action, unit
            );
        } else {
            allowed_units.push(unit);
        }
    }

    if allowed_units.is_empty() {
        process::exit(0);
    }

    // Invoke systemctl / utimctl
    let systemctl_bin = if Path::new("/bin/systemctl").exists() {
        "/bin/systemctl"
    } else if Path::new("/usr/bin/systemctl").exists() {
        "/usr/bin/systemctl"
    } else {
        "utimctl"
    };

    let mut cmd = Command::new(systemctl_bin);
    if let Some(r) = &root_dir {
        cmd.arg(format!("--root={r}"));
    }
    if no_wait {
        cmd.arg("--no-block");
    }
    cmd.arg(action);
    for unit in &allowed_units {
        cmd.arg(unit);
    }

    match cmd.status() {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            process::exit(code);
        }
        Err(e) => {
            eprintln!(
                "deb-systemd-invoke: failed to execute {}: {}",
                systemctl_bin, e
            );
            // In chroot or package installation without active init, exit cleanly with 0
            process::exit(0);
        }
    }
}

fn is_action_forbidden_by_policy(service: &str, action: &str) -> bool {
    // Honour $DPKG_ROOT like the image's own invoke-rc.d does
    // (build/rootfs/usr/sbin/invoke-rc.d: POLICYHELPER=$DPKG_ROOT/...).
    let policy_script = policy_script();
    if !policy_script.exists() {
        return false;
    }

    match Command::new(&policy_script)
        .arg(service)
        .arg(action)
        .status()
    {
        Ok(status) => {
            match status.code() {
                Some(0) | Some(104) => false, // 0 = allowed, 104 = fallback allowed
                Some(101) | Some(102) | Some(103) | Some(105) => true, // 101/105 = forbidden
                _ => true, // Treat any unexpected error code as forbidden in policy-rc.d
            }
        }
        Err(_) => false,
    }
}
