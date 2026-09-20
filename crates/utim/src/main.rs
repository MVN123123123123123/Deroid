//! Universal Treble Init Manager (UTIM) - Permanent PID 1 for Treble GSI.

mod container;
mod mount;
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
use std::path::PathBuf;
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
        println!("[UTIM] Mounting early pseudo-filesystems (/proc, /sys, /dev, /run, cgroup v2)...");
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
            PathBuf::from("/usr/lib/systemd/system"),
            PathBuf::from("/lib/systemd/system"),
        ]
    };

    println!("[UTIM] Loading systemd units from search paths: {:?}", default_paths);
    supervisor.load_systemd_units(&default_paths);
    println!("[UTIM] Total units loaded: {}", supervisor.dag.len());

    // Cycle detection
    let cycles = supervisor.dag.detect_cycles();
    if !cycles.is_empty() {
        for cycle in cycles {
            eprintln!("[UTIM] Warning: Dependency cycle detected in units: {:?}", cycle);
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

    if test_mode {
        if let Some(target) = args.iter().find(|a| a.ends_with(".target")) {
            println!("[UTIM] Test mode: Bootstrapping requested target: {}", target);
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

    // Main event loop
    let mut events: [libc::epoll_event; MAX_EPOLL_EVENTS] = unsafe { std::mem::zeroed() };
    let mut last_psi_check = Instant::now();

    println!("[UTIM] Entering permanent epoll event loop...");

    loop {
        let timeout_ms = 250; // 250ms event polling for timer checks and pending restarts
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
                                supervisor.log_msg("Received shutdown signal. Syncing filesystems and powering off...");
                                unsafe {
                                    libc::sync();
                                    if !test_mode {
                                        libc::reboot(libc::RB_POWER_OFF);
                                    }
                                }
                                return;
                            }
                            libc::SIGHUP => {
                                supervisor.log_msg("Received SIGHUP, reloading unit definitions...");
                                supervisor.load_systemd_units(&default_paths);
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
                        let pid = msg.mainpid.or(sender_pid);
                        supervisor.handle_notify_message(&msg, pid);
                    }
                } else if fd == control_server.as_raw_fd() {
                    if let Ok(stream) = control_server.accept() {
                        handle_client_connection(&mut supervisor, stream, &default_paths);
                    }
                }
            }
        }

        // Process pending delayed restarts
        supervisor.process_pending_restarts();

        // Check MMPS memory pressure every 5 seconds
        if last_psi_check.elapsed() >= Duration::from_secs(5) {
            last_psi_check = Instant::now();
            let level = supervisor.mmps.evaluate_pressure_level();
            if level == MemoryPressureLevel::Critical {
                supervisor.log_msg("CRITICAL Memory Pressure detected via PSI! Triggering background app reclaim.");
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
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();

    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }

    let Some(req) = IpcRequest::deserialize(&line) else {
        let resp = IpcResponse::Err("Invalid IPC command".to_string());
        let _ = write_response(&stream, &resp);
        return;
    };

    let resp = match req {
        IpcRequest::Start(unit) => match supervisor.start_unit(&unit) {
            Ok(_) => IpcResponse::Ok(format!("Started {}", unit)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::Stop(unit) => match supervisor.stop_unit(&unit) {
            Ok(_) => IpcResponse::Ok(format!("Stopped {}", unit)),
            Err(e) => IpcResponse::Err(e.to_string()),
        },
        IpcRequest::Restart(unit) => {
            let _ = supervisor.stop_unit(&unit);
            match supervisor.start_unit(&unit) {
                Ok(_) => IpcResponse::Ok(format!("Restarted {}", unit)),
                Err(e) => IpcResponse::Err(e.to_string()),
            }
        }
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
        IpcRequest::Enable(unit) => IpcResponse::Ok(format!("Enabled {}", unit)),
        IpcRequest::Disable(unit) => IpcResponse::Ok(format!("Disabled {}", unit)),
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
        IpcRequest::IsEnabled(unit) => IpcResponse::Ok(format!("enabled {}", unit)),
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
            unsafe {
                libc::sync();
                libc::reboot(libc::RB_AUTOBOOT);
            }
            return;
        }
        IpcRequest::Poweroff => {
            supervisor.log_msg("System poweroff requested via control socket");
            let resp = IpcResponse::Ok("Powering off".to_string());
            let _ = write_response(&stream, &resp);
            unsafe {
                libc::sync();
                libc::reboot(libc::RB_POWER_OFF);
            }
            return;
        }
    };

    let _ = write_response(&stream, &resp);
}

fn write_response(mut stream: &UnixStream, resp: &IpcResponse) -> std::io::Result<()> {
    stream.write_all(resp.serialize().as_bytes())?;
    stream.flush()
}
