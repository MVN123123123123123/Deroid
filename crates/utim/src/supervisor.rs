//! Process supervisor and systemd unit lifecycle manager.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use utim_core::dag::{UnitDag, UnitState};
use utim_core::hal::HalManager;
use utim_core::mmps::MemorySupervisor;
use utim_core::mpg::MobilePowerGovernor;
use utim_core::ring_buffer::ByteRingBuffer;
use utim_core::unit::{apply_dropin, expand_command_args, ExecCommand, ServiceType, UnitKind};

use crate::container::AndroidHalContainer;
use crate::notify::NotifyMessage;
use crate::reaper::ProcessExitInfo;
use crate::socket_act::{SocketActivationManager, SD_LISTEN_FDS_START};

pub struct Supervisor {
    pub dag: UnitDag,
    pub mpg: MobilePowerGovernor,
    pub mmps: MemorySupervisor,
    pub sockets: SocketActivationManager,
    pub container: AndroidHalContainer,
    pub log_ring: ByteRingBuffer<65536>,
    pub pid_to_unit: HashMap<i32, String>,
    pub pending_restarts: Vec<(String, Instant)>,
    pub boot_start_time: Instant,
    pub early_init_duration: Duration,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            dag: UnitDag::new(),
            mpg: MobilePowerGovernor::new(),
            mmps: MemorySupervisor::new(),
            sockets: SocketActivationManager::new(),
            container: AndroidHalContainer::new(),
            log_ring: ByteRingBuffer::new(),
            pid_to_unit: HashMap::new(),
            pending_restarts: Vec::new(),
            boot_start_time: Instant::now(),
            early_init_duration: Duration::ZERO,
        }
    }

    /// Load unit files from systemd search paths with strict priority order
    /// (/etc > /run > /usr/lib > /lib), respecting masking and drop-in configurations.
    pub fn load_systemd_units(&mut self, search_paths: &[PathBuf]) {
        let mut masked_units: HashSet<String> = HashSet::new();

        // Pass 1: Load unit files respecting priority and detecting /dev/null masks
        for dir in search_paths {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                    if name.ends_with(".service") || name.ends_with(".target") || name.ends_with(".socket") {
                        // Check if masked via /dev/null symlink
                        if let Ok(dest) = fs::read_link(&path) {
                            if dest == Path::new("/dev/null") {
                                masked_units.insert(name.clone());
                                continue;
                            }
                        }

                        if masked_units.contains(&name) {
                            continue;
                        }

                        // Only insert if not already loaded from a higher priority directory
                        if self.dag.get(&name).is_none() && path.is_file() {
                            if let Ok(content) = fs::read_to_string(&path) {
                                let unit = utim_core::unit::parse_unit(&name, &path, &content);
                                self.dag.insert(unit);
                            }
                        }
                    }
                }
            }
        }

        // Pass 2: Scan .wants, .requires, and .d drop-in directories
        for dir in search_paths {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if let Some(target_name) = dir_name.strip_suffix(".wants") {
                            if let Ok(want_entries) = fs::read_dir(&path) {
                                for want in want_entries.flatten() {
                                    let want_name = want.file_name().to_string_lossy().to_string();
                                    if want_name.ends_with(".service") || want_name.ends_with(".target") || want_name.ends_with(".socket") {
                                        if let Some(target_node) = self.dag.get_mut(target_name) {
                                            if !target_node.unit.unit.wants.contains(&want_name) {
                                                target_node.unit.unit.wants.push(want_name);
                                            }
                                        }
                                    }
                                }
                            }
                        } else if let Some(target_name) = dir_name.strip_suffix(".requires") {
                            if let Ok(req_entries) = fs::read_dir(&path) {
                                for req in req_entries.flatten() {
                                    let req_name = req.file_name().to_string_lossy().to_string();
                                    if req_name.ends_with(".service") || req_name.ends_with(".target") || req_name.ends_with(".socket") {
                                        if let Some(target_node) = self.dag.get_mut(target_name) {
                                            if !target_node.unit.unit.requires.contains(&req_name) {
                                                target_node.unit.unit.requires.push(req_name);
                                            }
                                        }
                                    }
                                }
                            }
                        } else if let Some(unit_name) = dir_name.strip_suffix(".d") {
                            if let Ok(conf_entries) = fs::read_dir(&path) {
                                for conf in conf_entries.flatten() {
                                    if conf.path().extension().and_then(|s| s.to_str()) == Some("conf") {
                                        if let Ok(dropin_content) = fs::read_to_string(conf.path()) {
                                            if let Some(unit_node) = self.dag.get_mut(unit_name) {
                                                apply_dropin(&mut unit_node.unit, &dropin_content);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Pass 3: Add synthetic Android HAL units if not masked or shadowed
        let hal_mgr = HalManager::new();
        for synthetic in hal_mgr.create_synthetic_units() {
            if self.dag.get(&synthetic.name).is_none() && !masked_units.contains(&synthetic.name) {
                self.dag.insert(synthetic);
            }
        }
    }

    /// Start a target or service unit and all its prerequisite dependencies.
    pub fn start_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let queue = self.dag.resolve_start_queue(unit_name);
        self.dispatch_ready_units(&queue)?;
        Ok(())
    }

    /// Dispatch any units in the queue that have all dependencies satisfied.
    pub fn dispatch_ready_units(&mut self, queue: &[String]) -> io::Result<()> {
        let ready = self.dag.ready_to_spawn(queue);
        for name in ready {
            self.spawn_unit(&name)?;
        }
        Ok(())
    }

    /// Spawn a single unit.
    pub fn spawn_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let Some(node) = self.dag.get(unit_name) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("Unit not found: {}", unit_name)));
        };

        if !node.unit.conditions_met() {
            self.log_msg(&format!("Unit {} condition check failed, skipping", unit_name));
            self.dag.set_state(unit_name, UnitState::Inactive);
            return Ok(());
        }

        match node.unit.kind {
            UnitKind::Target => {
                // Targets are synchronization points, immediately mark active
                self.dag.set_state(unit_name, UnitState::Active);
                self.log_msg(&format!("Reached target: {}", unit_name));
                return Ok(());
            }
            UnitKind::Socket => {
                if let Some(ref sock) = node.unit.socket {
                    let _ = self.sockets.bind_socket(unit_name, sock);
                    self.dag.set_state(unit_name, UnitState::Active);
                    self.log_msg(&format!("Listening on socket: {}", unit_name));
                }
                return Ok(());
            }
            UnitKind::Service => {}
            _ => {
                self.dag.set_state(unit_name, UnitState::Active);
                return Ok(());
            }
        }

        let svc = node.unit.service.clone().unwrap_or_default();
        if svc.exec_start.is_empty() {
            self.dag.set_state(unit_name, UnitState::Active);
            return Ok(());
        }

        let cmd = &svc.exec_start[0];

        // Build environment
        let mut env_map = HashMap::new();
        env_map.insert("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
        env_map.insert("TERM".to_string(), "linux".to_string());
        env_map.insert("NOTIFY_SOCKET".to_string(), "/run/systemd/notify".to_string());

        for (k, v) in &svc.environment {
            env_map.insert(k.clone(), v.clone());
        }

        // Parse environment files
        for (optional, file_path) in &svc.environment_files {
            if let Ok(content) = fs::read_to_string(file_path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        if let Some((k, v)) = trimmed.split_once('=') {
                            env_map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                        }
                    }
                }
            } else if !optional {
                self.log_msg(&format!("Warning: Required EnvironmentFile not found: {}", file_path));
            }
        }

        // Execute ExecStartPre commands synchronously
        for pre in &svc.exec_start_pre {
            match run_command_sync(pre, &env_map, svc.working_directory.as_deref()) {
                Ok(code) if code != 0 && !pre.ignore_failure => {
                    self.log_msg(&format!("Unit {} ExecStartPre failed with code {}", unit_name, code));
                    self.dag.set_state(unit_name, UnitState::Failed);
                    return Ok(());
                }
                Err(e) if !pre.ignore_failure => {
                    self.log_msg(&format!("Unit {} ExecStartPre error: {}", unit_name, e));
                    self.dag.set_state(unit_name, UnitState::Failed);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Socket activation file descriptors
        let socket_fds = self.sockets.sockets_for_service(unit_name);

        let mut exec_pipe = [-1; 2];
        if svc.service_type == ServiceType::Exec {
            unsafe {
                if libc::pipe2(exec_pipe.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            // Child process
            if svc.service_type == ServiceType::Exec {
                unsafe { libc::close(exec_pipe[0]) };
            }

            // Unblock signals
            let mut empty_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut empty_mask);
                libc::sigprocmask(libc::SIG_SETMASK, &empty_mask, std::ptr::null_mut());
                libc::setsid();
            }

            // Change working directory if specified
            if let Some(ref cwd) = svc.working_directory {
                let c_cwd = CString::new(cwd.as_str()).unwrap();
                unsafe { libc::chdir(c_cwd.as_ptr()) };
            }

            // Drop privileges if user / group specified
            if let Some(ref group_name) = svc.group {
                let c_grp = CString::new(group_name.as_str()).unwrap();
                let gr = unsafe { libc::getgrnam(c_grp.as_ptr()) };
                if !gr.is_null() {
                    unsafe { libc::setgid((*gr).gr_gid); }
                } else if let Ok(gid) = group_name.parse::<u32>() {
                    unsafe { libc::setgid(gid); }
                }
            }
            if let Some(ref user_name) = svc.user {
                let c_usr = CString::new(user_name.as_str()).unwrap();
                let pw = unsafe { libc::getpwnam(c_usr.as_ptr()) };
                if !pw.is_null() {
                    unsafe {
                        if svc.group.is_none() {
                            libc::setgid((*pw).pw_gid);
                        }
                        libc::setuid((*pw).pw_uid);
                    }
                } else if let Ok(uid) = user_name.parse::<u32>() {
                    unsafe { libc::setuid(uid); }
                }
            }

            // Apply resource limits
            if let Some(nofile) = svc.limit_nofile {
                let rlim = libc::rlimit {
                    rlim_cur: nofile,
                    rlim_max: nofile,
                };
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
            }

            // Apply OOM score adjust
            if let Some(oom) = svc.oom_score_adjust {
                let _ = MemorySupervisor::apply_oom_score_adj(unsafe { libc::getpid() }, oom);
            }

            // Socket activation fds: pass sockets starting at fd 3 (SD_LISTEN_FDS_START)
            if !socket_fds.is_empty() {
                env_map.insert("LISTEN_FDS".to_string(), socket_fds.len().to_string());
                env_map.insert("LISTEN_PID".to_string(), unsafe { libc::getpid() }.to_string());

                for (idx, &raw_fd) in socket_fds.iter().enumerate() {
                    let target_fd = SD_LISTEN_FDS_START + idx as i32;
                    if raw_fd != target_fd {
                        unsafe {
                            libc::dup2(raw_fd, target_fd);
                            libc::close(raw_fd);
                        }
                    }
                    unsafe {
                        libc::fcntl(target_fd, libc::F_SETFD, 0); // clear FD_CLOEXEC
                    }
                }
            }

            // Build CString args with systemd-compliant argument expansion
            let resolved_bin = resolve_binary(&cmd.binary).unwrap_or_else(|| cmd.binary.clone());
            let mut c_args = Vec::new();
            c_args.push(CString::new(resolved_bin.as_str()).unwrap());
            for arg in expand_command_args(&cmd.args, &env_map) {
                c_args.push(CString::new(arg).unwrap());
            }
            let mut arg_ptrs: Vec<*const libc::c_char> = c_args.iter().map(|s| s.as_ptr()).collect();
            arg_ptrs.push(std::ptr::null());

            // Build CString env
            let mut c_envs = Vec::new();
            for (k, v) in env_map {
                c_envs.push(CString::new(format!("{}={}", k, v)).unwrap());
            }
            let mut env_ptrs: Vec<*const libc::c_char> = c_envs.iter().map(|s| s.as_ptr()).collect();
            env_ptrs.push(std::ptr::null());

            unsafe {
                libc::execve(c_args[0].as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr());
                // If execve fails
                libc::_exit(127);
            }
        }

        // Parent process
        if svc.service_type == ServiceType::Exec {
            unsafe {
                libc::close(exec_pipe[1]);
                let mut dummy = [0u8; 1];
                let _ = libc::read(exec_pipe[0], dummy.as_mut_ptr() as *mut libc::c_void, 1);
                libc::close(exec_pipe[0]);
            }
        }

        self.pid_to_unit.insert(pid, unit_name.to_string());
        self.dag.set_pid(unit_name, Some(pid));

        let initial_state = match svc.service_type {
            ServiceType::Simple => UnitState::Active,
            _ => UnitState::Activating,
        };
        self.dag.set_state(unit_name, initial_state);

        if initial_state == UnitState::Active {
            // Execute ExecStartPost
            for post in &svc.exec_start_post {
                let _ = run_command_sync(post, &env_map, svc.working_directory.as_deref());
            }
        }

        self.log_msg(&format!("Started {}: PID {}", unit_name, pid));
        Ok(())
    }

    /// Stop a running unit.
    pub fn stop_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let Some(node) = self.dag.get(unit_name) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("Unit not found: {}", unit_name)));
        };

        let svc = node.unit.service.clone().unwrap_or_default();
        let mut env_map = HashMap::new();
        env_map.insert("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
        if let Some(pid) = node.pid {
            env_map.insert("MAINPID".to_string(), pid.to_string());
        }

        // Run ExecStop commands
        for stop_cmd in &svc.exec_stop {
            let _ = run_command_sync(stop_cmd, &env_map, svc.working_directory.as_deref());
        }

        if let Some(pid) = node.pid {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            self.dag.set_state(unit_name, UnitState::Deactivating);
            self.log_msg(&format!("Stopping {}: Sent SIGTERM to PID {}", unit_name, pid));
        } else {
            self.dag.set_state(unit_name, UnitState::Inactive);
        }

        Ok(())
    }

    /// Reload a running unit.
    pub fn reload_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let Some(node) = self.dag.get(unit_name) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("Unit not found: {}", unit_name)));
        };

        let svc = node.unit.service.clone().unwrap_or_default();
        let mut env_map = HashMap::new();
        env_map.insert("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
        if let Some(pid) = node.pid {
            env_map.insert("MAINPID".to_string(), pid.to_string());
        }

        for reload_cmd in &svc.exec_reload {
            let _ = run_command_sync(reload_cmd, &env_map, svc.working_directory.as_deref());
        }

        self.log_msg(&format!("Reloaded {}", unit_name));
        Ok(())
    }

    /// Handle sd_notify messages from child processes.
    pub fn handle_notify_message(&mut self, msg: &NotifyMessage, sender_pid: Option<i32>) {
        let unit_name = sender_pid.and_then(|p| self.pid_to_unit.get(&p).cloned())
            .or_else(|| {
                // Fallback: match unambiguous activating notify service
                let activating: Vec<String> = self.dag.all_nodes().iter()
                    .filter(|(_, n)| n.state == UnitState::Activating && n.unit.service.as_ref().map(|s| s.service_type) == Some(ServiceType::Notify))
                    .map(|(k, _)| k.clone())
                    .collect();
                if activating.len() == 1 {
                    Some(activating[0].clone())
                } else {
                    None
                }
            });

        if let Some(ref name) = unit_name {
            if msg.ready {
                self.dag.set_state(name, UnitState::Active);
                self.log_msg(&format!("Service {} reported ready via sd_notify", name));

                // Execute ExecStartPost if configured
                if let Some(node) = self.dag.get(name) {
                    if let Some(ref svc) = node.unit.service {
                        let mut env_map = HashMap::new();
                        env_map.insert("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
                        if let Some(pid) = node.pid {
                            env_map.insert("MAINPID".to_string(), pid.to_string());
                        }
                        for post in &svc.exec_start_post {
                            let _ = run_command_sync(post, &env_map, svc.working_directory.as_deref());
                        }
                    }
                }

                // Dependent units might now be unblocked
                let all_names: Vec<String> = self.dag.all_nodes().keys().cloned().collect();
                let _ = self.dispatch_ready_units(&all_names);
            }
            if let Some(ref status) = msg.status {
                self.log_msg(&format!("Service {} status: {}", name, status));
            }
            if let Some(new_pid) = msg.mainpid {
                self.dag.set_pid(name, Some(new_pid));
                self.pid_to_unit.insert(new_pid, name.clone());
            }
        }
    }

    /// Handle process exits from zombie reaper.
    pub fn handle_process_exits(&mut self, exits: &[ProcessExitInfo]) {
        for exit in exits {
            if let Some(unit_name) = self.pid_to_unit.remove(&exit.pid) {
                let is_success = exit.exited_cleanly && exit.exit_code == 0;
                self.log_msg(&format!(
                    "Unit {} (PID {}) exited: clean={}, code={}",
                    unit_name, exit.pid, exit.exited_cleanly, exit.exit_code
                ));

                let (svc_type, restart_policy, restart_sec) = if let Some(node) = self.dag.get(&unit_name) {
                    let svc = node.unit.service.as_ref();
                    (
                        svc.map(|s| s.service_type).unwrap_or(ServiceType::Simple),
                        svc.map(|s| s.restart).unwrap_or(utim_core::unit::RestartPolicy::No),
                        svc.map(|s| s.restart_sec).unwrap_or(Duration::from_secs(1)),
                    )
                } else {
                    (ServiceType::Simple, utim_core::unit::RestartPolicy::No, Duration::from_secs(1))
                };

                if svc_type == ServiceType::Oneshot {
                    if is_success {
                        self.dag.set_state(&unit_name, UnitState::Active);
                        let all_names: Vec<String> = self.dag.all_nodes().keys().cloned().collect();
                        let _ = self.dispatch_ready_units(&all_names);
                    } else {
                        self.dag.set_state(&unit_name, UnitState::Failed);
                    }
                } else {
                    let should_restart = match restart_policy {
                        utim_core::unit::RestartPolicy::Always => true,
                        utim_core::unit::RestartPolicy::OnFailure => !is_success,
                        utim_core::unit::RestartPolicy::OnSuccess => is_success,
                        _ => false,
                    };

                    if should_restart {
                        self.log_msg(&format!("Scheduling restart for {} in {:?}", unit_name, restart_sec));
                        self.pending_restarts.push((unit_name.clone(), Instant::now() + restart_sec));
                        self.dag.set_state(&unit_name, UnitState::Activating);
                    } else if is_success {
                        self.dag.set_state(&unit_name, UnitState::Inactive);
                    } else {
                        self.dag.set_state(&unit_name, UnitState::Failed);
                    }
                }

                // Check if this was a vendor HAL daemon
                if let Some(hal_name) = self.container.handle_hal_exit(exit.pid) {
                    self.log_msg(&format!("Vendor HAL {} terminated, self-healing watchdog triggered", hal_name));
                }
            }
        }
    }

    /// Process any pending unit restarts whose delay timer has expired.
    pub fn process_pending_restarts(&mut self) {
        let now = Instant::now();
        let mut ready = Vec::new();

        self.pending_restarts.retain(|(name, restart_time)| {
            if now >= *restart_time {
                ready.push(name.clone());
                false
            } else {
                true
            }
        });

        for name in ready {
            let _ = self.spawn_unit(&name);
        }
    }

    pub fn log_msg(&mut self, msg: &str) {
        println!("[UTIM] {}", msg);
        let formatted = format!("[UTIM] {}\n", msg);
        self.log_ring.write_overwrite(formatted.as_bytes());
    }
}

fn resolve_binary(binary: &str) -> Option<String> {
    if binary.starts_with('/') {
        return Some(binary.to_string());
    }
    let default_path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    for p in default_path.split(':') {
        let full = format!("{}/{}", p, binary);
        if Path::new(&full).exists() {
            return Some(full);
        }
    }
    Some(binary.to_string())
}

fn run_command_sync(
    cmd: &ExecCommand,
    env_map: &HashMap<String, String>,
    working_dir: Option<&str>,
) -> io::Result<i32> {
    let binary = resolve_binary(&cmd.binary).unwrap_or_else(|| cmd.binary.clone());
    let args = expand_command_args(&cmd.args, env_map);

    let mut command = std::process::Command::new(binary);
    command.args(&args);
    if let Some(cwd) = working_dir {
        command.current_dir(cwd);
    }
    for (k, v) in env_map {
        command.env(k, v);
    }

    match command.status() {
        Ok(status) => Ok(status.code().unwrap_or(0)),
        Err(e) => {
            if cmd.ignore_failure {
                Ok(0)
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_supervisor_unit_lifecycle() {
        let mut supervisor = Supervisor::new();

        let temp_dir = std::env::temp_dir().join("utim_test_supervisor");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let service_content = r#"
[Unit]
Description=Test Oneshot Service

[Service]
Type=oneshot
ExecStart=/bin/true
"#;
        let unit_path = temp_dir.join("test-oneshot.service");
        fs::write(&unit_path, service_content).unwrap();

        let unit = utim_core::unit::parse_unit("test-oneshot.service", &unit_path, service_content);
        supervisor.dag.insert(unit);

        supervisor.start_unit("test-oneshot.service").unwrap();
        let pid = supervisor.dag.get("test-oneshot.service").unwrap().pid.unwrap();

        // Simulate exit
        let exit_info = ProcessExitInfo {
            pid,
            status: 0,
            exited_cleanly: true,
            exit_code: 0,
            signal: None,
        };
        supervisor.handle_process_exits(&[exit_info]);

        assert_eq!(
            supervisor.dag.get("test-oneshot.service").unwrap().state,
            UnitState::Active
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_load_systemd_units_priority_and_masking() {
        let temp_dir = std::env::temp_dir().join(format!("utim_test_search_paths_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let etc_dir = temp_dir.join("etc/systemd/system");
        let usr_dir = temp_dir.join("usr/lib/systemd/system");
        fs::create_dir_all(&etc_dir).unwrap();
        fs::create_dir_all(&usr_dir).unwrap();

        // 1. Service defined in usr_dir, overridden in etc_dir
        fs::write(usr_dir.join("app.service"), "[Unit]\nDescription=App UsrLib\n[Service]\nExecStart=/bin/app\n").unwrap();
        fs::write(etc_dir.join("app.service"), "[Unit]\nDescription=App Etc\n[Service]\nExecStart=/bin/app\n").unwrap();

        // 2. Service masked in etc_dir via /dev/null symlink
        fs::write(usr_dir.join("masked.service"), "[Unit]\nDescription=Masked Unit\n[Service]\nExecStart=/bin/bad\n").unwrap();
        std::os::unix::fs::symlink("/dev/null", etc_dir.join("masked.service")).unwrap();

        // 3. Drop-in configuration
        let dropin_dir = etc_dir.join("app.service.d");
        fs::create_dir_all(&dropin_dir).unwrap();
        fs::write(dropin_dir.join("10-override.conf"), "[Service]\nRestartSec=15s\n").unwrap();

        // 4. .wants directory
        let wants_dir = etc_dir.join("multi-user.target.wants");
        fs::create_dir_all(&wants_dir).unwrap();
        fs::write(wants_dir.join("app.service"), "").unwrap();

        // Create dummy multi-user.target in usr_dir
        fs::write(usr_dir.join("multi-user.target"), "[Unit]\nDescription=Multi User Target\n").unwrap();

        let mut supervisor = Supervisor::new();
        let search_paths = vec![etc_dir, usr_dir];
        supervisor.load_systemd_units(&search_paths);

        // Verify priority: App Etc won over App UsrLib
        let app_node = supervisor.dag.get("app.service").expect("app.service should be loaded");
        assert_eq!(app_node.unit.unit.description, "App Etc");
        assert_eq!(app_node.unit.service.as_ref().unwrap().restart_sec, std::time::Duration::from_secs(15));

        // Verify masking: masked.service was NOT loaded
        assert!(supervisor.dag.get("masked.service").is_none());

        // Verify .wants: multi-user.target wants app.service
        let target_node = supervisor.dag.get("multi-user.target").expect("target should be loaded");
        assert!(target_node.unit.unit.wants.contains(&"app.service".to_string()));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_supervisor_notify_handling() {
        let mut supervisor = Supervisor::new();

        let unit_content = "[Unit]\nDescription=Notify Daemon\n[Service]\nType=notify\nExecStart=/bin/daemon\n";
        let unit = utim_core::unit::parse_unit("notify-app.service", Path::new("/test/notify-app.service"), unit_content);
        supervisor.dag.insert(unit);

        // Transition to Activating with pid 9999
        let node = supervisor.dag.get_mut("notify-app.service").unwrap();
        node.state = UnitState::Activating;
        node.pid = Some(9999);

        // Case 1: Notify message with matching sender_pid
        let msg = NotifyMessage {
            ready: true,
            status: Some("Ready for requests".to_string()),
            watchdog: false,
            mainpid: None,
            errno: None,
        };
        supervisor.handle_notify_message(&msg, Some(9999));
        assert_eq!(supervisor.dag.get("notify-app.service").unwrap().state, UnitState::Active);

        // Reset to Activating
        let node = supervisor.dag.get_mut("notify-app.service").unwrap();
        node.state = UnitState::Activating;

        // Case 2: Notify message with None sender_pid (e.g., standard client without SCM_CREDENTIALS or proxy)
        // should hit single Activating fallback and transition to Active
        supervisor.handle_notify_message(&msg, None);
        assert_eq!(supervisor.dag.get("notify-app.service").unwrap().state, UnitState::Active);
    }
}
