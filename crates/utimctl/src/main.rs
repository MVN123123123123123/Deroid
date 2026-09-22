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

    let cmd = args[1].as_str();

    match cmd {
        "reboot" => {
            run_simple_cmd(&socket_path, IpcRequest::Reboot);
        }
        "poweroff" | "halt" => {
            run_simple_cmd(&socket_path, IpcRequest::Poweroff);
        }
        "start" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl start <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_simple_cmd(&socket_path, IpcRequest::Start(unit));
        }
        "stop" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl stop <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_simple_cmd(&socket_path, IpcRequest::Stop(unit));
        }
        "restart" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl restart <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_simple_cmd(&socket_path, IpcRequest::Restart(unit));
        }
        "reload" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl reload <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_simple_cmd(&socket_path, IpcRequest::Reload(unit));
        }
        "status" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl status <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_status_cmd(&socket_path, unit);
        }
        "daemon-reload" => {
            run_simple_cmd(&socket_path, IpcRequest::DaemonReload);
        }
        "is-active" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl is-active <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_is_active(&socket_path, unit);
        }
        "is-enabled" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl is-enabled <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_is_enabled(&socket_path, unit);
        }
        "enable" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl enable <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_enable_cmd(&unit);
        }
        "disable" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl disable <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_disable_cmd(&unit);
        }
        "mask" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl mask <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_mask_cmd(&unit);
        }
        "unmask" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl unmask <unit>");
                process::exit(1);
            }
            let unit = normalize_unit_name(&args[2]);
            run_unmask_cmd(&unit);
        }
        "list-units" => {
            run_list_units(&socket_path);
        }
        "analyze" => {
            run_analyze(&socket_path);
        }
        "power" => {
            if args.len() < 3 {
                eprintln!("Usage: systemctl power <freeze|unfreeze|wakelock-acquire|wakelock-release> [arg]");
                process::exit(1);
            }
            run_power_cmd(&socket_path, &args[2], args.get(3).map(|s| s.as_str()));
        }
        "--version" | "version" => {
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
        Ok(_) => process::exit(0),
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
        Ok(_) => process::exit(1),
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
        Ok(_) => process::exit(3),
        Err(_) => {
            println!("inactive");
            process::exit(3);
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

    if let Ok(content) = std::fs::read_to_string(&path) {
        let parsed = utim_core::unit::parse_unit(unit, &path, &content);
        for target in &parsed.install.wanted_by {
            let target_name = if target.contains('.') {
                target.clone()
            } else {
                format!("{}.target", target)
            };
            let target_wants = format!("/etc/systemd/system/{}.wants", target_name);
            let _ = std::fs::create_dir_all(&target_wants);
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
            let _ = std::fs::create_dir_all(&target_req);
            let symlink_path = format!("{}/{}", target_req, unit);
            let _ = std::fs::remove_file(&symlink_path);
            if let Err(e) = std::os::unix::fs::symlink(&path, &symlink_path) {
                eprintln!("Failed to create symlink {}: {}", symlink_path, e);
                process::exit(1);
            }
            println!("Created symlink {} -> {}.", symlink_path, path.display());
        }
    }
    process::exit(0);
}

fn run_disable_cmd(unit: &str) {
    let etc_systemd = Path::new("/etc/systemd/system");
    if let Ok(entries) = std::fs::read_dir(etc_systemd) {
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
    }
    process::exit(0);
}

fn run_mask_cmd(unit: &str) {
    let mask_path = format!("/etc/systemd/system/{}", unit);
    let _ = std::fs::remove_file(&mask_path);
    if let Err(e) = std::os::unix::fs::symlink("/dev/null", &mask_path) {
        eprintln!("Failed to mask unit {}: {}", unit, e);
        process::exit(1);
    }
    println!("Created symlink {} -> /dev/null.", mask_path);
    process::exit(0);
}

fn run_unmask_cmd(unit: &str) {
    let mask_path = format!("/etc/systemd/system/{}", unit);
    if let Ok(target) = std::fs::read_link(&mask_path) {
        if target == Path::new("/dev/null") {
            let _ = std::fs::remove_file(&mask_path);
            println!("Removed {}.", mask_path);
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
        Ok(_) => process::exit(1),
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
        Ok(_) => process::exit(1),
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
