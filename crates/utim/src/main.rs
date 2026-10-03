//! Universal Treble Init Manager (UTIM) - Permanent PID 1 for Treble GSI.

mod container;
mod mount;
mod network;
mod notify;
mod reaper;
mod server;
mod socket_act;
mod supervisor;

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use utim_core::dag::UnitState;
use utim_core::ipc::{IpcRequest, IpcResponse, DEFAULT_CONTROL_SOCKET};
use utim_core::mmps::MemoryPressureLevel;

use crate::mount::{mount_early_filesystems, mount_vendor_partitions, setup_binder_devnodes};
use crate::notify::{NotifyServer, NOTIFY_SOCKET_PATH};
use crate::reaper::{reap_zombies, SignalHandler};
use crate::server::ControlServer;
use crate::supervisor::Supervisor;

const MAX_EPOLL_EVENTS: usize = 32;

fn main() {
    let args: Vec<String> = env::args().collect();
    let is_pid_1 = std::process::id() == 1;
    let force_system = args.contains(&"--system".to_string()) || is_pid_1;
    let test_mode = args.contains(&"--test-mode".to_string());

    println!("============================================================");
    println!(" UTIM: Universal Treble Init Manager (Version 0.1.0)");
    println!(" High-Performance Phone-Optimized PID 1 for Project Treble");
    println!("============================================================");

    let early_start = Instant::now();

    if force_system && !test_mode {
        println!(
            "[UTIM] Mounting early pseudo-filesystems (/proc, /sys, /dev, /run, cgroup v2)..."
        );
        if let Err(e) = mount_early_filesystems() {
            eprintln!("[UTIM] Warning: Early mount error: {}", e);
        }

        println!("[UTIM] Mounting vendor partitions from fstab (/vendor, /odm, /firmware)...");
        if let Err(e) = mount_vendor_partitions() {
            eprintln!("[UTIM] Warning: Vendor mount error: {}", e);
        }

        println!("[UTIM] Initializing Android Binder nodes (/dev/binder, /dev/vndbinder, /dev/hwbinder)...");
        if let Err(e) = setup_binder_devnodes() {
            eprintln!("[UTIM] Warning: Binder devnode error: {}", e);
        }

        println!("[UTIM] Initializing network subsystem (lo, eth0, DNS)...");
        if let Err(e) = network::setup_network_subsystem() {
            eprintln!("[UTIM] Warning: Network setup error: {}", e);
        }
    } else {
        // In user / test mode, ensure /run/utim and /run/systemd/system exist if writable
        let _ = fs::create_dir_all("/run/utim");
        let _ = fs::create_dir_all("/run/systemd/system");
    }

    let early_duration = early_start.elapsed();

    let mut supervisor = Supervisor::new();
    supervisor.early_init_duration = early_duration;

    let mut custom_unit_dir = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--unit-dir" && i + 1 < args.len() {
            custom_unit_dir = Some(PathBuf::from(&args[i + 1]));
            break;
        }
        i += 1;
    }

    let default_paths = if let Some(dir) = custom_unit_dir {
        vec![dir]
    } else if test_mode {
        vec![]
    } else {
        vec![
            PathBuf::from("/etc/systemd/system"),
            PathBuf::from("/run/systemd/system"),
            PathBuf::from("/usr/lib/systemd/system"),
            PathBuf::from("/lib/systemd/system"),
        ]
    };

    println!(
        "[UTIM] Loading systemd units from search paths: {:?}",
        default_paths
    );
    supervisor.load_systemd_units(&default_paths);
    println!("[UTIM] Total units loaded: {}", supervisor.dag.len());

    // Cycle detection
    let cycles = supervisor.dag.detect_cycles();
    if !cycles.is_empty() {
        for cycle in cycles {
            eprintln!(
                "[UTIM] Warning: Dependency cycle detected in units: {:?}",
                cycle
            );
        }
    }

    // Set up signal handling
    let signal_handler = SignalHandler::new().expect("Failed to initialize signalfd");

    // Set up notify server
    let notify_path = if test_mode {
        PathBuf::from("/tmp/utim_test_notify.sock")
    } else {
        PathBuf::from(NOTIFY_SOCKET_PATH)
    };
    let notify_server = NotifyServer::bind(&notify_path).expect("Failed to bind notify socket");

    // Set up control socket server
    let control_path = if test_mode {
        PathBuf::from("/tmp/utim_test_control.sock")
    } else {
        PathBuf::from(DEFAULT_CONTROL_SOCKET)
    };
    let control_server = ControlServer::bind(&control_path).expect("Failed to bind control socket");

    // Create epoll instance
    let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epoll_fd < 0 {
        panic!("Failed to create epoll instance");
    }

    register_epoll(epoll_fd, signal_handler.as_raw_fd(), libc::EPOLLIN as u32);
    register_epoll(epoll_fd, notify_server.as_raw_fd(), libc::EPOLLIN as u32);
    register_epoll(epoll_fd, control_server.as_raw_fd(), libc::EPOLLIN as u32);

    let mut registered_sockets: std::collections::HashSet<RawFd> = std::collections::HashSet::new();
    for fd in supervisor.sockets.all_fds() {
        if registered_sockets.insert(fd) {
            register_epoll(epoll_fd, fd, libc::EPOLLIN as u32);
        }
    }

    if test_mode {
        if let Some(target) = args.iter().find(|a| a.ends_with(".target")) {
            println!(
                "[UTIM] Test mode: Bootstrapping requested target: {}",
                target
            );
            let _ = supervisor.start_unit(target);
        } else {
            println!("[UTIM] Test mode: Skipping target bootstrap.");
        }
        println!("[UTIM] Test mode initialized successfully. Exiting setup cleanly.");
        return;
    }

    // If a target is specified in args, start that, otherwise start default.target or multi-user.target
    let target = args
        .iter()
        .find(|a| a.ends_with(".target"))
        .cloned()
        .unwrap_or_else(|| {
            if supervisor.dag.get("graphical.target").is_some() {
                "graphical.target".to_string()
            } else if supervisor.dag.get("multi-user.target").is_some() {
                "multi-user.target".to_string()
            } else {
                "default.target".to_string()
            }
        });

    println!("[UTIM] Bootstrapping target: {}", target);
    let _ = supervisor.start_unit(&target);

    // Register any newly bound sockets from bootstrap
    for fd in supervisor.sockets.all_fds() {
        if registered_sockets.insert(fd) {
            register_epoll(epoll_fd, fd, libc::EPOLLIN as u32);
        }
    }

    // Main event loop
    let mut events: [libc::epoll_event; MAX_EPOLL_EVENTS] = unsafe { std::mem::zeroed() };
    let mut last_psi_check = Instant::now();
    let mut psi_frozen = false;

    let mut registered_socket_count = supervisor.sockets.count();

    println!("[UTIM] Entering permanent epoll event loop...");

    loop {
        // Register any newly opened activation sockets only when new sockets exist
        let current_count = supervisor.sockets.count();
        if current_count != registered_socket_count {
            registered_socket_count = current_count;
            for fd in supervisor.sockets.all_fds() {
                if registered_sockets.insert(fd) {
                    register_epoll(epoll_fd, fd, libc::EPOLLIN as u32);
                }
            }
        }

        let timeout_ms = supervisor.next_deadline_ms(&last_psi_check);
        let nfds = unsafe {
            libc::epoll_wait(
                epoll_fd,
                events.as_mut_ptr(),
                MAX_EPOLL_EVENTS as libc::c_int,
                timeout_ms,
            )
        };

        if nfds > 0 {
            for ev in events.iter().take(nfds as usize) {
                let fd = ev.u64 as RawFd;

                if fd == signal_handler.as_raw_fd() {
                    let sigs = signal_handler.read_signals();
                    for sig in sigs {
                        match sig.ssi_signo as i32 {
                            libc::SIGCHLD => {
                                let exits = reap_zombies();
                                if !exits.is_empty() {
                                    supervisor.handle_process_exits(&exits);
                                }
                            }
                            libc::SIGTERM | libc::SIGINT | libc::SIGPWR => {
                                supervisor.log_msg("Received shutdown signal. Tearing down units, syncing filesystems and powering off...");
                                supervisor.shutdown_all_units();
                                unsafe {
                                    libc::sync();
                                    if !test_mode {
                                        // B1: a failed reboot must be loud and non-fatal;
                                        // never exit PID 1 silently.
                                        if libc::reboot(libc::RB_POWER_OFF) != 0 {
                                            let e = std::io::Error::last_os_error();
                                            supervisor.log_msg(&format!(
                                                "reboot(RB_POWER_OFF) failed: {}; staying as PID 1",
                                                e
                                            ));
                                        } else {
                                            return;
                                        }
                                    } else {
                                        return;
                                    }
                                }
                                // Fall through (do not return) when reboot failed
                                // or in test mode handled above.
                            }
                            libc::SIGHUP => {
                                supervisor
                                    .log_msg("Received SIGHUP, reloading unit definitions...");
                                supervisor.load_systemd_units(&default_paths);
                                // Prune activation sockets of removed units
                                // and stop tracking them in epoll.
                                let live: std::collections::HashSet<String> =
                                    supervisor.dag.all_nodes().keys().cloned().collect();
                                for fd in supervisor.sockets.prune_removed_units(&live) {
                                    registered_sockets.remove(&fd);
                                    // P8/B8: DEL before close so a recycled fd
                                    // number is never un-registered.
                                    unsafe {
                                        libc::epoll_ctl(
                                            epoll_fd,
                                            libc::EPOLL_CTL_DEL,
                                            fd,
                                            std::ptr::null_mut(),
                                        );
                                        libc::close(fd);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let exits = reap_zombies();
                    if !exits.is_empty() {
                        supervisor.handle_process_exits(&exits);
                    }
                } else if fd == notify_server.as_raw_fd() {
                    let msgs = notify_server.recv_messages_with_sender();
                    for (msg, sender_pid) in msgs {
                        // C3: identity comes from SO_PASSCRED only; MAINPID
                        // inside the payload is validated in handle_notify_message.
                        supervisor.handle_notify_message(&msg, sender_pid);
                    }
                } else if fd == control_server.as_raw_fd() {
                    match control_server.accept_with_cred() {
                        Ok((stream, cred)) => {
                            handle_client_connection(
                                &mut supervisor,
                                stream,
                                &default_paths,
                                cred.uid,
                            );
                        }
                        Err(e) => {
                            let kind = e.kind();
                            if kind != std::io::ErrorKind::WouldBlock {
                                supervisor.log_msg(&format!("Control accept error: {}", e));
                            }
                        }
                    }
                } else if let Some(active_sock) = supervisor.sockets.find_by_fd(fd).cloned() {
                    let svc_name = active_sock.service_name;
                    if let Some(node) = supervisor.dag.get(&svc_name) {
                        // C5: Failed is terminal. A level-triggered readable
                        // socket must not re-arm a dead unit (fork bomb).
                        if node.state == UnitState::Inactive {
                            supervisor
                                .log_msg(&format!("Socket activation triggered for {}", svc_name));
                            let _ = supervisor.start_unit(&svc_name);
                        }
                    }
                }
            }
        }

        // Process pending delayed restarts
        supervisor.process_pending_restarts();

        // Escalate stops that ignored SIGTERM to SIGKILL
        supervisor.process_stop_timeouts();

        // Process watchdog timer checks
        supervisor.check_watchdogs();

        // Check MMPS memory pressure every 5 seconds. On Critical, freeze
        // the background slice to stop CPU burn; unfreeze as soon as
        // pressure subsides so the phone never wedges frozen.
        if last_psi_check.elapsed() >= Duration::from_secs(5) {
            last_psi_check = Instant::now();
            let level = supervisor.mmps.evaluate_pressure_level();
            if level == MemoryPressureLevel::Critical && !psi_frozen {
                supervisor.log_msg(
                    "CRITICAL Memory Pressure via PSI: freezing user.slice until pressure subsides.",
                );
                match supervisor.mpg.set_cgroup_freeze("user.slice", true) {
                    Ok(_) => psi_frozen = true,
                    Err(e) => supervisor.log_msg(&format!("PSI reclaim: freeze failed: {}", e)),
                }
            } else if level != MemoryPressureLevel::Critical && psi_frozen {
                supervisor.log_msg("Memory pressure relieved: unfreezing user.slice.");
                match supervisor.mpg.set_cgroup_freeze("user.slice", false) {
                    Ok(_) => psi_frozen = false,
                    Err(e) => supervisor.log_msg(&format!("PSI relief: unfreeze failed: {}", e)),
                }
            }
        }
    }
}

fn register_epoll(epoll_fd: RawFd, target_fd: RawFd, events: u32) {
    let mut ev = libc::epoll_event {
        events,
        u64: target_fd as u64,
    };
    unsafe {
        libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, target_fd, &mut ev);
    }
}

fn handle_client_connection(
    supervisor: &mut Supervisor,
    stream: UnixStream,
    search_paths: &[PathBuf],
    peer_uid: u32,
) {
    // C7: bound the stall. Accepted stream already has a 100ms read timeout
    // (server.rs); additionally cap the line at 4KiB here so a slow client
    // cannot hold the single-threaded event loop. For full isolation, fork
    // a short-lived handler instead of running inline.
    let Ok(stream_clone) = stream.try_clone() else {
        return;
    };
    use std::io::Read;
    let _ = stream_clone.set_nonblocking(false);
    let reader = BufReader::new(stream_clone);
    let mut line = String::new();
    // Take at most 4KiB for the request line (plus the global 64KiB IPC cap
    // enforced inside deserialize); a longer line is rejected as invalid.
    let mut limited = reader.take(4096);

    if limited.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }

    let Some(req) = IpcRequest::deserialize(&line) else {
        let resp = IpcResponse::Err("Invalid IPC command".to_string());
        let _ = write_response(&stream, &resp);
        return;
    };

    if peer_uid != 0 {
        match req {
            IpcRequest::Status(_)
            | IpcRequest::ListUnits
            | IpcRequest::IsActive(_)
            | IpcRequest::IsEnabled(_)
            | IpcRequest::AnalyzeTime => {}
            _ => {
                let resp =
                    IpcResponse::Err("Permission denied: root privileges required".to_string());
                let _ = write_response(&stream, &resp);
                return;
            }
        }
    }

    let resp = match req {
        IpcRequest::Start(unit) => match supervisor.start_unit(&unit) {
            Ok(_) => IpcResponse::Ok(format!("Started {}", unit)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::Stop(unit) => match supervisor.stop_unit(&unit) {
            Ok(_) => IpcResponse::Ok(format!("Stopped {}", unit)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::Restart(unit) => match supervisor.restart_unit(&unit) {
            Ok(_) => IpcResponse::Ok(format!("Restarted {}", unit)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::Reload(unit) => match supervisor.reload_unit(&unit) {
            Ok(_) => IpcResponse::Ok(format!("Reloaded {}", unit)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::Status(unit) => {
            if let Some(node) = supervisor.dag.get(&unit) {
                IpcResponse::Status {
                    name: unit.clone(),
                    state: node.state.as_str().to_string(),
                    pid: node.pid,
                    description: node.unit.unit.description.clone(),
                    details: format!(
                        "Loaded: {}\nActive: {}\nMain PID: {:?}",
                        node.unit.path.display(),
                        node.state.as_str(),
                        node.pid
                    ),
                }
            } else {
                IpcResponse::Err(format!("Unit {} not found", unit))
            }
        }
        IpcRequest::ListUnits => {
            let mut list = Vec::new();
            for (name, node) in supervisor.dag.all_nodes() {
                list.push((
                    name.clone(),
                    node.state.as_str().to_string(),
                    node.unit.unit.description.clone(),
                ));
            }
            IpcResponse::UnitList(list)
        }
        IpcRequest::DaemonReload => {
            supervisor.load_systemd_units(search_paths);
            IpcResponse::Ok("Configuration reloaded".to_string())
        }
        IpcRequest::Enable(unit) => {
            if let Some(node) = supervisor.dag.get(&unit) {
                let unit_path = &node.unit.path;
                let install = &node.unit.install;
                let mut targets = install.wanted_by.clone();
                if targets.is_empty() && install.required_by.is_empty() && install.alias.is_empty()
                {
                    targets.push("multi-user.target".to_string());
                }
                for target in targets {
                    let target_name = if target.contains('.') {
                        target
                    } else {
                        format!("{}.target", target)
                    };
                    let wants_dir =
                        PathBuf::from(format!("/etc/systemd/system/{}.wants", target_name));
                    let _ = fs::create_dir_all(&wants_dir);
                    let symlink_path = wants_dir.join(&unit);
                    let _ = fs::remove_file(&symlink_path);
                    let _ = std::os::unix::fs::symlink(unit_path, &symlink_path);
                }
                for req in &install.required_by {
                    let req_name = if req.contains('.') {
                        req.clone()
                    } else {
                        format!("{}.target", req)
                    };
                    let req_dir =
                        PathBuf::from(format!("/etc/systemd/system/{}.requires", req_name));
                    let _ = fs::create_dir_all(&req_dir);
                    let symlink_path = req_dir.join(&unit);
                    let _ = fs::remove_file(&symlink_path);
                    let _ = std::os::unix::fs::symlink(unit_path, &symlink_path);
                }
                for alias in &install.alias {
                    let alias_path = PathBuf::from(format!("/etc/systemd/system/{}", alias));
                    let _ = fs::remove_file(&alias_path);
                    let _ = std::os::unix::fs::symlink(unit_path, &alias_path);
                }
                IpcResponse::Ok(format!("Enabled {}", unit))
            } else {
                IpcResponse::Err(format!("Unit {} not found", unit))
            }
        }
        IpcRequest::Disable(unit) => {
            let etc = Path::new("/etc/systemd/system");
            if let Ok(entries) = fs::read_dir(etc) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".wants") || name.ends_with(".requires") {
                            let link = p.join(&unit);
                            if link.is_symlink() || link.exists() {
                                let _ = fs::remove_file(&link);
                            }
                        }
                    }
                }
            }
            let direct = etc.join(&unit);
            if direct.is_symlink() || direct.exists() {
                let _ = fs::remove_file(&direct);
            }
            IpcResponse::Ok(format!("Disabled {}", unit))
        }
        IpcRequest::IsActive(unit) => {
            if let Some(node) = supervisor.dag.get(&unit) {
                if node.state == UnitState::Active {
                    IpcResponse::Ok("active".to_string())
                } else {
                    IpcResponse::Err(node.state.as_str().to_string())
                }
            } else {
                IpcResponse::Err("unknown".to_string())
            }
        }
        IpcRequest::IsEnabled(unit) => {
            let mut is_enabled = false;
            let etc = Path::new("/etc/systemd/system");
            if let Ok(entries) = fs::read_dir(etc) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".wants") || name.ends_with(".requires") {
                            let link = p.join(&unit);
                            if link.is_symlink() || link.exists() {
                                is_enabled = true;
                                break;
                            }
                        }
                    }
                }
            }
            if !is_enabled && etc.join(&unit).exists() {
                is_enabled = true;
            }
            if is_enabled {
                IpcResponse::Ok("enabled".to_string())
            } else {
                IpcResponse::Ok("disabled".to_string())
            }
        }
        IpcRequest::FreezeCgroup(slice) => match supervisor.mpg.set_cgroup_freeze(&slice, true) {
            Ok(_) => IpcResponse::Ok(format!("Frozen {}", slice)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::UnfreezeCgroup(slice) => {
            match supervisor.mpg.set_cgroup_freeze(&slice, false) {
                Ok(_) => IpcResponse::Ok(format!("Unfrozen {}", slice)),
                Err(e) => IpcResponse::Err(e.to_string()),
            }
        }
        IpcRequest::AcquireWakeLock(name) => match supervisor.mpg.acquire_wake_lock(&name) {
            Ok(_) => IpcResponse::Ok(format!("Wake lock {} acquired", name)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::ReleaseWakeLock(name) => match supervisor.mpg.release_wake_lock(&name) {
            Ok(_) => IpcResponse::Ok(format!("Wake lock {} released", name)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::SetOomScore(pid, score) => {
            match utim_core::mmps::MemorySupervisor::apply_oom_score_adj(pid, score) {
                Ok(_) => IpcResponse::Ok(format!("Set OOM score for PID {} to {}", pid, score)),
                Err(e) => IpcResponse::Err(e.to_string()),
            }
        }
        IpcRequest::AnalyzeTime => {
            let total = supervisor.boot_start_time.elapsed().as_secs_f64();
            let early = supervisor.early_init_duration.as_secs_f64();
            let kernel = 0.500; // Typical Android kernel boot estimate
            IpcResponse::Time {
                kernel_sec: kernel,
                init_sec: early,
                total_sec: kernel + total,
            }
        }
        IpcRequest::Reboot => {
            supervisor.log_msg("System reboot requested via control socket");
            let resp = IpcResponse::Ok("Rebooting".to_string());
            let _ = write_response(&stream, &resp);
            supervisor.shutdown_all_units();
            unsafe {
                libc::sync();
                // B1: do not exit PID 1 when reboot fails.
                if libc::reboot(libc::RB_AUTOBOOT) != 0 {
                    let e = std::io::Error::last_os_error();
                    supervisor.log_msg(&format!(
                        "reboot(RB_AUTOBOOT) failed: {}; staying as PID 1",
                        e
                    ));
                } else {
                    return;
                }
            }
            // Reboot failed: answer already sent; stay in event loop.
            // Fall through to the trailing write (harmless duplicate) is
            // avoided by returning after handling below.
            let _ = write_response(
                &stream,
                &IpcResponse::Err("reboot failed; PID 1 still running".to_string()),
            );
            return;
        }
        IpcRequest::Poweroff => {
            supervisor.log_msg("System poweroff requested via control socket");
            let resp = IpcResponse::Ok("Powering off".to_string());
            let _ = write_response(&stream, &resp);
            supervisor.shutdown_all_units();
            unsafe {
                libc::sync();
                // B1: do not exit PID 1 when poweroff fails.
                if libc::reboot(libc::RB_POWER_OFF) != 0 {
                    let e = std::io::Error::last_os_error();
                    supervisor.log_msg(&format!(
                        "reboot(RB_POWER_OFF) failed: {}; staying as PID 1",
                        e
                    ));
                } else {
                    return;
                }
            }
            let _ = write_response(
                &stream,
                &IpcResponse::Err("poweroff failed; PID 1 still running".to_string()),
            );
            return;
        }
    };

    let _ = write_response(&stream, &resp);
}

fn write_response(mut stream: &UnixStream, resp: &IpcResponse) -> std::io::Result<()> {
    stream.write_all(resp.serialize().as_bytes())?;
    stream.flush()
}
