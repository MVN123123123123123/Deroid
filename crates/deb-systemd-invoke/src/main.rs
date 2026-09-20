//! deb-systemd-invoke: Debian policy-rc.d aware service action invoker.

use std::env;
use std::path::Path;
use std::process::{self, Command};

fn main() {
    // Debian init-system-helpers specification:
    // If /run/systemd/system does not exist, systemd / UTIM is not running as PID 1
    // (e.g. during chroot bootstrap, container build, or offline installation).
    // All service actions must exit cleanly with 0.
    let run_systemd_dir = env::var("DEB_SYSTEMD_SYSTEM_DIR").unwrap_or_else(|_| "/run/systemd/system".to_string());
    if !Path::new(&run_systemd_dir).exists() {
        process::exit(0);
    }

    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: deb-systemd-invoke <action> <unit>...");
        process::exit(1);
    }

    // Filter out flags like --user, --quiet, and --root <path>
    let mut clean_args = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--root" {
            if i + 1 < args.len() {
                i += 1; // skip the chroot path value
            }
        } else if !arg.starts_with("--") {
            clean_args.push(arg.as_str());
        }
        i += 1;
    }

    if clean_args.len() < 2 {
        eprintln!("Usage: deb-systemd-invoke <action> <unit>...");
        process::exit(1);
    }

    let action = clean_args[0];
    let units = &clean_args[1..];

    // Policy check: /usr/sbin/policy-rc.d
    // policy-rc.d is only queried for actions that start/restart services (never stop)
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
            eprintln!("deb-systemd-invoke: failed to execute {}: {}", systemctl_bin, e);
            // In chroot or package installation without active init, exit cleanly with 0
            process::exit(0);
        }
    }
}

fn is_action_forbidden_by_policy(service: &str, action: &str) -> bool {
    let policy_script = "/usr/sbin/policy-rc.d";
    if !Path::new(policy_script).exists() {
        return false;
    }

    match Command::new(policy_script).arg(service).arg(action).status() {
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
