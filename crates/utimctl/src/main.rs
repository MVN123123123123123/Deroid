//! utimctl: Drop-in /bin/systemctl CLI for UTIM init.

use std::env;
use std::path::{Path, PathBuf};
use std::process;
use utim_core::ipc::{send_ipc_request, IpcRequest, IpcResponse, DEFAULT_CONTROL_SOCKET};

fn main() {
    let args: Vec<String> = env::args().collect();
    let socket_path = env::var("UTIM_CONTROL_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONTROL_SOCKET));

    // Handle symlink invocations like /sbin/reboot, /sbin/poweroff, /sbin/halt
    if let Some(prog) = args.first() {
        let prog_name = Path::new(prog)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        match prog_name {
            "reboot" => {
                run_simple_cmd(&socket_path, IpcRequest::Reboot);
                return;
            }
            "poweroff" | "halt" | "shutdown" => {
                run_simple_cmd(&socket_path, IpcRequest::Poweroff);
                return;
            }
            _ => {}
        }
    }

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    // S5: strip systemctl-compatible options BEFORE dispatch, so an option
    // token can never be mistaken for a unit name (`status --no-pager foo`
    // must query foo, not --no-pager.service). Unknown flags are an
    // explicit error, not a unit name.
    let cleaned = match strip_options(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("utimctl: {e}");
            print_usage();
            process::exit(1);
        }
    };
    if cleaned.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let cmd = cleaned[1].as_str();
    let operands = &cleaned[2..];

    match cmd {
        "reboot" => {
            reject_operands("reboot", operands);
            run_simple_cmd(&socket_path, IpcRequest::Reboot);
        }
        "poweroff" | "halt" => {
            reject_operands(cmd, operands);
            run_simple_cmd(&socket_path, IpcRequest::Poweroff);
        }
        "start" => {
            let unit = require_one_unit("start", operands);
            run_simple_cmd(&socket_path, IpcRequest::Start(unit));
        }
        "stop" => {
            let unit = require_one_unit("stop", operands);
            run_simple_cmd(&socket_path, IpcRequest::Stop(unit));
        }
        "restart" => {
            let unit = require_one_unit("restart", operands);
            run_simple_cmd(&socket_path, IpcRequest::Restart(unit));
        }
        "reload" => {
            let unit = require_one_unit("reload", operands);
            run_simple_cmd(&socket_path, IpcRequest::Reload(unit));
        }
        "status" => {
            let unit = require_one_unit("status", operands);
            run_status_cmd(&socket_path, unit);
        }
        "daemon-reload" => {
            reject_operands("daemon-reload", operands);
            run_simple_cmd(&socket_path, IpcRequest::DaemonReload);
        }
        "is-active" => {
            let unit = require_one_unit("is-active", operands);
            run_is_active(&socket_path, unit);
        }
        "is-enabled" => {
            let unit = require_one_unit("is-enabled", operands);
            run_is_enabled(&socket_path, unit);
        }
        "enable" => {
            let unit = require_one_unit("enable", operands);
            run_enable_cmd(&unit);
        }
        "disable" => {
            let unit = require_one_unit("disable", operands);
            run_disable_cmd(&unit);
        }
        "mask" => {
            let unit = require_one_unit("mask", operands);
            run_mask_cmd(&unit);
        }
        "unmask" => {
            let unit = require_one_unit("unmask", operands);
            run_unmask_cmd(&unit);
        }
        "list-units" => {
            reject_operands("list-units", operands);
            run_list_units(&socket_path);
        }
        "analyze" => {
            // `analyze` alone == `analyze time`; anything else is rejected
            // rather than silently ignored.
            if operands.len() > 1 || (operands.len() == 1 && operands[0] != "time") {
                eprintln!("Usage: systemctl analyze [time]");
                process::exit(1);
            }
            run_analyze(&socket_path);
        }
        "power" => {
            if operands.len() != 1 && operands.len() != 2 {
                eprintln!(
                    "Usage: systemctl power <freeze|unfreeze|wakelock-acquire|wakelock-release> [arg]"
                );
                process::exit(1);
            }
            run_power_cmd(
                &socket_path,
                &operands[0],
                operands.get(1).map(|s| s.as_str()),
            );
        }
        "--version" | "version" => {
            reject_operands("version", operands);
            println!("utimctl (UTIM 0.1.0)");
            println!("+SYSTEMD_COMPAT +ARM64_64K_PAGES +CGROUP_V2 +ANDROID_TREBLE");
        }
        _ => {
            eprintln!("Unknown command: {}", cmd);
            print_usage();
            process::exit(1);
        }
    }
}

/// Drop every recognised `--flag` / `-f` token so option position can never
/// be mistaken for a unit name. Flags that take a value consume it (both
/// `--flag value` and `--flag=value`); anything else starting with `-` is
/// an explicit error, never a unit name.
fn strip_options(args: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    out.push(args[0].clone());
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        // Split `--flag=value` before matching so value-carrying long
        // options are recognised in both spellings.
        let (flag, inline_value) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        match flag {
            "--no-pager" | "--no-legend" | "--full" | "-l" | "--plain" | "--no-block"
            | "--system" | "--user" | "--quiet" | "-q" | "--now" | "--force" | "--all"
            | "--reverse" | "--show-types" | "--value" | "--failed" => {}
            "--state" | "--type" | "-t" | "--job-mode" | "--no-ask-password" | "-H" | "--host"
            | "-M" | "--machine" | "--lines" | "-n" => {
                if inline_value.is_none() {
                    // Consume the value token; it must not look like a flag.
                    match args.get(i + 1) {
                        Some(v) if !v.starts_with('-') => {
                            i += 1;
                        }
                        _ => return Err(format!("option {flag} requires a value")),
                    }
                }
            }
            s if s.starts_with('-') => return Err(format!("unrecognised option {s}")),
            _ => out.push(a.clone()),
        }
        i += 1;
    }
    Ok(out)
}

/// Exactly one operand is required; anything else is a usage error, never a
/// silently ignored extra argument.
fn require_one_unit(cmd: &str, operands: &[String]) -> String {
    if operands.len() != 1 {
        eprintln!("Usage: systemctl {cmd} <unit>");
        process::exit(1);
    }
    normalize_unit_name(&operands[0])
}

fn reject_operands(cmd: &str, operands: &[String]) {
    if !operands.is_empty() {
        eprintln!("systemctl {cmd} takes no arguments");
        process::exit(1);
    }
}

fn normalize_unit_name(name: &str) -> String {
    if !name.contains('.') {
        format!("{}.service", name)
    } else {
        name.to_string()
    }
}

fn run_simple_cmd(socket_path: &Path, req: IpcRequest) {
    match send_ipc_request(socket_path, &req) {
        Ok(IpcResponse::Ok(msg)) => {
            if !msg.is_empty() {
                println!("{}", msg);
            }
            process::exit(0);
        }
        Ok(IpcResponse::Err(err)) => {
            eprintln!("Failed: {}", err);
            process::exit(1);
        }
        Ok(other) => {
            eprintln!("Failed: unexpected reply {other:?} to {req:?}");
            process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Failed to connect to UTIM daemon at {}: {}",
                socket_path.display(),
                e
            );
            process::exit(1);
        }
    }
}

fn run_status_cmd(socket_path: &Path, unit: String) {
    match send_ipc_request(socket_path, &IpcRequest::Status(unit.clone())) {
        Ok(IpcResponse::Status {
            name,
            state,
            pid,
            description,
            details,
        }) => {
            let symbol = if state == "active" { "●" } else { "○" };
            println!("{} {} - {}", symbol, name, description);
            println!("     Loaded: loaded (/usr/lib/systemd/system/{})", name);
            let pid_str = pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string());
            println!("     Active: {} (PID: {})", state, pid_str);
            if !details.is_empty() {
                println!("\n{}", details);
            }

            if state == "active" {
                process::exit(0);
            } else {
                process::exit(3);
            }
        }
        Ok(IpcResponse::Err(e)) => {
            eprintln!("Unit {} could not be found: {}", unit, e);
            process::exit(4);
        }
        Ok(other) => {
            eprintln!("utimctl: unexpected reply {other:?} to status {unit}");
            process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Failed to connect to UTIM daemon at {}: {}",
                socket_path.display(),
                e
            );
            process::exit(1);
        }
    }
}

fn run_is_active(socket_path: &Path, unit: String) {
    match send_ipc_request(socket_path, &IpcRequest::IsActive(unit)) {
        Ok(IpcResponse::Ok(state)) => {
            println!("{}", state);
            process::exit(0);
        }
        Ok(IpcResponse::Err(state)) => {
            println!("{}", state);
            process::exit(3);
        }
        Ok(other) => {
            eprintln!("utimctl: unexpected reply {other:?}");
            process::exit(1);
        }
        Err(e) => {
            // Never claim "inactive" when the truth is "cannot ask".
            eprintln!(
                "utimctl: cannot query UTIM at {}: {}",
                socket_path.display(),
                e
            );
            process::exit(1);
        }
    }
}

fn run_is_enabled(_socket_path: &Path, unit: String) {
    let enabled = check_enabled_glob(&unit);
    if enabled {
        println!("enabled");
        process::exit(0);
    } else {
        println!("disabled");
        process::exit(1);
    }
}

fn check_enabled_glob(unit: &str) -> bool {
    let etc_systemd = Path::new("/etc/systemd/system");
    if let Ok(entries) = std::fs::read_dir(etc_systemd) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ext = path.extension().and_then(|s| s.to_str());
                if (ext == Some("wants") || ext == Some("requires"))
                    && std::fs::symlink_metadata(path.join(unit)).is_ok()
                {
                    return true;
                }
            }
        }
    }
    false
}

fn run_enable_cmd(unit: &str) {
    // Read unit file to discover WantedBy= and RequiredBy=
    let unit_paths = [
        format!("/etc/systemd/system/{}", unit),
        format!("/usr/lib/systemd/system/{}", unit),
        format!("/lib/systemd/system/{}", unit),
    ];

    let mut found_path = None;
    for p in &unit_paths {
        let path = Path::new(p);
        if path.exists() {
            found_path = Some(path.to_path_buf());
            break;
        }
    }

    let Some(path) = found_path else {
        eprintln!("Failed to enable unit: Unit file {} does not exist", unit);
        process::exit(1);
    };

    // Search both the existence of the unit and its directory, so the symlink
    // targets the copy we actually validated.
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "Failed to enable unit: cannot read {}: {}",
                path.display(),
                e
            );
            process::exit(1);
        }
    };
    let parsed = utim_core::unit::parse_unit(unit, &path, &content);
    if parsed.install.wanted_by.is_empty() && parsed.install.required_by.is_empty() {
        eprintln!("Failed to enable unit: {unit} is static (no [Install] WantedBy=/RequiredBy=)");
        process::exit(1);
    }
    for target in &parsed.install.wanted_by {
        let target_name = if target.contains('.') {
            target.clone()
        } else {
            format!("{}.target", target)
        };
        let target_wants = format!("/etc/systemd/system/{}.wants", target_name);
        if let Err(e) = std::fs::create_dir_all(&target_wants) {
            eprintln!(
                "Failed to enable unit: cannot create {}: {}",
                target_wants, e
            );
            process::exit(1);
        }
        let symlink_path = format!("{}/{}", target_wants, unit);
        let _ = std::fs::remove_file(&symlink_path);
        if let Err(e) = std::os::unix::fs::symlink(&path, &symlink_path) {
            eprintln!("Failed to create symlink {}: {}", symlink_path, e);
            process::exit(1);
        }
        println!("Created symlink {} -> {}.", symlink_path, path.display());
    }
    for target in &parsed.install.required_by {
        let target_name = if target.contains('.') {
            target.clone()
        } else {
            format!("{}.target", target)
        };
        let target_req = format!("/etc/systemd/system/{}.requires", target_name);
        if let Err(e) = std::fs::create_dir_all(&target_req) {
            eprintln!("Failed to enable unit: cannot create {}: {}", target_req, e);
            process::exit(1);
        }
        let symlink_path = format!("{}/{}", target_req, unit);
        let _ = std::fs::remove_file(&symlink_path);
        if let Err(e) = std::os::unix::fs::symlink(&path, &symlink_path) {
            eprintln!("Failed to create symlink {}: {}", symlink_path, e);
            process::exit(1);
        }
        println!("Created symlink {} -> {}.", symlink_path, path.display());
    }
    process::exit(0);
}

fn run_disable_cmd(unit: &str) {
    let etc_systemd = Path::new("/etc/systemd/system");
    let entries = match std::fs::read_dir(etc_systemd) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to disable unit {unit}: cannot read /etc/systemd/system: {e}");
            process::exit(1);
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let ext = path.extension().and_then(|s| s.to_str());
            if ext == Some("wants") || ext == Some("requires") {
                let symlink_path = path.join(unit);
                if std::fs::symlink_metadata(&symlink_path).is_ok() {
                    let _ = std::fs::remove_file(&symlink_path);
                    println!("Removed {}.", symlink_path.display());
                }
            }
        }
    }
    process::exit(0);
}

fn run_mask_cmd(unit: &str) {
    let mask_path = format!("/etc/systemd/system/{}", unit);
    // Refuse to clobber a real file: masking would delete an admin-authored
    // unit with no backup, and unmask could never restore it.
    match std::fs::symlink_metadata(&mask_path) {
        Ok(m) if m.file_type().is_symlink() => {
            let _ = std::fs::remove_file(&mask_path);
        }
        Ok(m) if m.is_dir() => {
            eprintln!("Failed to mask unit: {unit} is a directory");
            process::exit(1);
        }
        Ok(_) => {
            eprintln!(
                "Failed to mask unit: {mask_path} is a regular file; move it aside first (masking would delete it)"
            );
            process::exit(1);
        }
        Err(_) => {}
    }
    if let Err(e) = std::os::unix::fs::symlink("/dev/null", &mask_path) {
        eprintln!("Failed to mask unit {}: {}", unit, e);
        process::exit(1);
    }
    println!("Created symlink {} -> /dev/null.", mask_path);
    process::exit(0);
}

fn run_unmask_cmd(unit: &str) {
    let mask_path = format!("/etc/systemd/system/{}", unit);
    match std::fs::read_link(&mask_path) {
        Ok(target) if target == Path::new("/dev/null") => {
            if let Err(e) = std::fs::remove_file(&mask_path) {
                eprintln!("Failed to unmask unit {unit}: {e}");
                process::exit(1);
            }
            println!("Removed {}.", mask_path);
        }
        Ok(_) => {
            eprintln!(
                "Failed to unmask unit: {unit} is not masked ({} is not a /dev/null symlink)",
                mask_path
            );
            process::exit(1);
        }
        Err(_) => {
            eprintln!("Failed to unmask unit: {unit} is not masked");
            process::exit(1);
        }
    }
    process::exit(0);
}

fn run_list_units(socket_path: &Path) {
    match send_ipc_request(socket_path, &IpcRequest::ListUnits) {
        Ok(IpcResponse::UnitList(units)) => {
            println!("{:<35} {:<12} {:<40}", "UNIT", "ACTIVE", "DESCRIPTION");
            println!("{}", "-".repeat(90));
            for (name, state, desc) in units {
                println!("{:<35} {:<12} {:<40}", name, state, desc);
            }
            process::exit(0);
        }
        Ok(IpcResponse::Err(e)) => {
            eprintln!("utimctl: list-units failed: {e}");
            process::exit(1);
        }
        Ok(other) => {
            eprintln!("utimctl: unexpected reply {other:?} to list-units");
            process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Failed to connect to UTIM daemon at {}: {}",
                socket_path.display(),
                e
            );
            process::exit(1);
        }
    }
}

fn run_analyze(socket_path: &Path) {
    match send_ipc_request(socket_path, &IpcRequest::AnalyzeTime) {
        Ok(IpcResponse::Time {
            kernel_sec,
            init_sec,
            total_sec,
        }) => {
            println!(
                "Startup finished in {:.3}s (kernel) + {:.3}s (utim-early) = {:.3}s",
                kernel_sec, init_sec, total_sec
            );
            println!("graphical.target reached in {:.3}s", total_sec);
            process::exit(0);
        }
        Ok(IpcResponse::Err(e)) => {
            eprintln!("utimctl: analyze failed: {e}");
            process::exit(1);
        }
        Ok(other) => {
            eprintln!("utimctl: unexpected reply {other:?} to analyze");
            process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Failed to connect to UTIM daemon at {}: {}",
                socket_path.display(),
                e
            );
            process::exit(1);
        }
    }
}

fn run_power_cmd(socket_path: &Path, action: &str, arg: Option<&str>) {
    let req = match action {
        "freeze" => IpcRequest::FreezeCgroup(arg.unwrap_or("user.slice").to_string()),
        "unfreeze" => IpcRequest::UnfreezeCgroup(arg.unwrap_or("user.slice").to_string()),
        "wakelock-acquire" => IpcRequest::AcquireWakeLock(arg.unwrap_or("user-cli").to_string()),
        "wakelock-release" => IpcRequest::ReleaseWakeLock(arg.unwrap_or("user-cli").to_string()),
        _ => {
            eprintln!("Unknown power action: {}", action);
            process::exit(1);
        }
    };
    run_simple_cmd(socket_path, req);
}

fn print_usage() {
    eprintln!("Usage: systemctl [OPTIONS] COMMAND [ARG...]");
    eprintln!("Commands:");
    eprintln!("  start <unit>         Start a unit");
    eprintln!("  stop <unit>          Stop a unit");
    eprintln!("  restart <unit>       Restart a unit");
    eprintln!("  reload <unit>        Reload unit configuration");
    eprintln!("  status <unit>        Show unit status");
    eprintln!("  is-active <unit>     Check if a unit is active");
    eprintln!("  is-enabled <unit>    Check if a unit is enabled");
    eprintln!("  enable <unit>        Enable a unit to start at boot");
    eprintln!("  disable <unit>       Disable a unit from starting at boot");
    eprintln!("  mask <unit>          Mask a unit (/dev/null symlink)");
    eprintln!("  unmask <unit>        Unmask a unit");
    eprintln!("  daemon-reload        Reload all systemd unit definitions");
    eprintln!("  list-units           List all loaded units and states");
    eprintln!("  analyze [time]       Show boot timing statistics");
    eprintln!("  power <subcmd>       Mobile power governor control");
    eprintln!("  reboot               Reboot the system");
    eprintln!("  poweroff             Power off the system");
    eprintln!("  halt                 Halt the system");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_before_and_after_unit_are_stripped() {
        // S5: `status --no-pager foo` must query foo, never --no-pager.service.
        let cleaned = strip_options(&argv(&["systemctl", "status", "--no-pager", "foo"])).unwrap();
        assert_eq!(cleaned, argv(&["systemctl", "status", "foo"]));

        let cleaned = strip_options(&argv(&["systemctl", "stop", "-q", "foo"])).unwrap();
        assert_eq!(cleaned, argv(&["systemctl", "stop", "foo"]));

        // A valued option consumes the next token, so `-n foo` leaves `stop`
        // with no unit (a loud usage error downstream, never a wrong unit).
        let cleaned = strip_options(&argv(&["systemctl", "stop", "-n", "foo"])).unwrap();
        assert_eq!(cleaned, argv(&["systemctl", "stop"]));
    }

    #[test]
    fn valued_options_consume_their_value() {
        let cleaned = strip_options(&argv(&["systemctl", "list-units", "--state=failed"])).unwrap();
        assert_eq!(cleaned, argv(&["systemctl", "list-units"]));

        let cleaned =
            strip_options(&argv(&["systemctl", "list-units", "--type", "service"])).unwrap();
        assert_eq!(cleaned, argv(&["systemctl", "list-units"]));

        // A dangling valued option is an error, not a silently dropped token.
        assert!(strip_options(&argv(&["systemctl", "list-units", "--state"])).is_err());
    }

    #[test]
    fn unknown_options_are_an_error_not_a_unit() {
        // S5: `--help`/`--now`-style typos must not become unit names.
        assert!(strip_options(&argv(&["systemctl", "status", "--bogus", "foo"])).is_err());
        assert!(strip_options(&argv(&["systemctl", "--help"])).is_err());
    }

    #[test]
    fn normalize_adds_service_suffix_only_when_bare() {
        assert_eq!(normalize_unit_name("foo"), "foo.service");
        assert_eq!(normalize_unit_name("foo.service"), "foo.service");
        assert_eq!(normalize_unit_name("foo.target"), "foo.target");
    }
}
