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
use crate::reaper::{reap_zombies, ProcessExitInfo};
use crate::socket_act::{SocketActivationManager, SD_LISTEN_FDS_START};

pub struct Supervisor {
    pub dag: UnitDag,
    pub mpg: MobilePowerGovernor,
    pub mmps: MemorySupervisor,
    pub sockets: SocketActivationManager,
    pub container: AndroidHalContainer,
    pub log_ring: ByteRingBuffer<65536>,
    /// FD for log output (stderr by default). Written with raw libc::write
    /// (O_NONBLOCK where possible), errors ignored — never panics on EPIPE.
    pub log_fd: libc::c_int,
    pub pid_to_unit: HashMap<i32, String>,
    pub pending_restarts: Vec<(String, Instant)>,
    pub restart_queue: HashSet<String>,
    pub pending_units: HashSet<String>,
    pub last_watchdogs: HashMap<String, Instant>,
    pub watchdog_aborted: HashSet<String>,
    pub boot_start_time: Instant,
    pub early_init_duration: Duration,
    pub stopping_since: HashMap<String, Instant>,
    pub stop_timeout: Duration,
    /// Restart rate limiting (StartLimitBurst / StartLimitIntervalSec).
    pub start_stamps: HashMap<String, (u32, Instant)>,
    pub start_burst: u32,
    pub start_interval: Duration,
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
            restart_queue: HashSet::new(),
            pending_units: HashSet::new(),
            last_watchdogs: HashMap::new(),
            watchdog_aborted: HashSet::new(),
            boot_start_time: Instant::now(),
            early_init_duration: Duration::ZERO,
            stopping_since: HashMap::new(),
            stop_timeout: Duration::from_secs(5),
            start_stamps: HashMap::new(),
            start_burst: 5,
            start_interval: Duration::from_secs(10),
            log_fd: 2,
        }
    }

    /// StartLimit burst gate. Returns false when the unit exceeded
    /// start_burst restarts within start_interval (caller must mark Failed).
    fn restart_allowed(&mut self, name: &str) -> bool {
        let now = Instant::now();
        let entry = self
            .start_stamps
            .entry(name.to_string())
            .or_insert((0, now));
        if entry.1.elapsed() > self.start_interval {
            entry.0 = 0;
            entry.1 = now;
        }
        entry.0 += 1;
        entry.0 <= self.start_burst
    }

    /// Load unit files from systemd search paths with strict priority order
    /// (/etc > /run > /usr/lib > /lib), respecting masking and drop-in configurations.
    pub fn load_systemd_units(&mut self, search_paths: &[PathBuf]) {
        let mut masked_units: HashSet<String> = HashSet::new();
        let mut loaded_units_in_pass: HashSet<String> = HashSet::new();

        // Pass 1: Load unit files respecting priority and detecting /dev/null masks.
        // Entries are sorted for deterministic load order across boots.
        for dir in search_paths {
            if let Ok(entries) = fs::read_dir(dir) {
                let mut paths: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
                            matches!(ext, "service" | "target" | "socket" | "timer" | "mount")
                        })
                    })
                    .collect();
                paths.sort();
                for path in paths {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
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

                    // Only insert if not already loaded from a higher priority directory in this pass
                    if loaded_units_in_pass.insert(name.clone()) && path.is_file() {
                        if let Ok(content) = fs::read_to_string(&path) {
                            let parsed = utim_core::unit::parse_unit(&name, &path, &content);
                            if let Some(existing_node) = self.dag.get_mut(&name) {
                                existing_node.unit = parsed;
                            } else {
                                self.dag.insert(parsed);
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
                                    if want_name.ends_with(".service")
                                        || want_name.ends_with(".target")
                                        || want_name.ends_with(".socket")
                                    {
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
                                    if req_name.ends_with(".service")
                                        || req_name.ends_with(".target")
                                        || req_name.ends_with(".socket")
                                    {
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
                                    if conf.path().extension().and_then(|s| s.to_str())
                                        == Some("conf")
                                    {
                                        if let Ok(dropin_content) = fs::read_to_string(conf.path())
                                        {
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

        // Evict units that are now masked (e.g. masked after a previous
        // load via daemon-reload) so masked units can never start.
        for masked in &masked_units {
            self.dag.remove(masked);
            self.pending_units.remove(masked);
        }
    }

    /// Construct full environment map including PATH, TERM, NOTIFY_SOCKET, MAINPID, WATCHDOG_USEC,
    /// Environment variables and EnvironmentFile entries.
    pub fn build_unit_env(
        &self,
        _unit_name: &str,
        node: &utim_core::dag::DagNode,
    ) -> HashMap<String, String> {
        let mut env_map = HashMap::new();
        env_map.insert(
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        );
        env_map.insert("TERM".to_string(), "linux".to_string());
        env_map.insert(
            "NOTIFY_SOCKET".to_string(),
            "/run/systemd/notify".to_string(),
        );

        // NOTE (B4): MAINPID/WATCHDOG_PID are set in the fork child from
        // getpid(). Do not export stale node.pid here.
        if let Some(ref svc) = node.unit.service {
            if svc.watchdog_sec > Duration::ZERO {
                env_map.insert(
                    "WATCHDOG_USEC".to_string(),
                    svc.watchdog_sec.as_micros().to_string(),
                );
            }
            for (k, v) in &svc.environment {
                env_map.insert(k.clone(), v.clone());
            }

            for (optional, file_path) in &svc.environment_files {
                if let Ok(content) = fs::read_to_string(file_path) {
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() && !trimmed.starts_with('#') {
                            if let Some((k, v)) = trimmed.split_once('=') {
                                env_map.insert(
                                    k.trim().to_string(),
                                    v.trim().trim_matches('"').to_string(),
                                );
                            }
                        }
                    }
                } else if !optional {
                    eprintln!(
                        "[UTIM] Warning: Required EnvironmentFile not found: {}",
                        file_path
                    );
                }
            }
        }

        env_map
    }

    /// Start a target or service unit and all its prerequisite dependencies.
    pub fn start_unit(&mut self, unit_name: &str) -> io::Result<()> {
        if self.dag.get(unit_name).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unit not found: {}", unit_name),
            ));
        }
        let queue = self.dag.resolve_start_queue(unit_name);
        for name in &queue {
            if let Some(node) = self.dag.get(name) {
                // B3: only enqueue spawnable states; never cancel a queued
                // restart by dropping a Deactivating unit from pending.
                if matches!(node.state, UnitState::Inactive | UnitState::Failed) {
                    self.pending_units.insert(name.clone());
                }
            }
        }
        self.dispatch_pending_units()?;
        Ok(())
    }

    /// Dispatch any units in the queue that have all dependencies satisfied.
    #[allow(dead_code)]
    pub fn dispatch_ready_units(&mut self, queue: &[String]) -> io::Result<()> {
        for name in queue {
            if let Some(node) = self.dag.get(name) {
                if node.state != UnitState::Active {
                    self.pending_units.insert(name.clone());
                }
            }
        }
        self.dispatch_pending_units()
    }

    /// Dispatch any pending units whose dependencies are satisfied.
    /// Runs a fixed-point loop over self.pending_units until no more units are ready to spawn.
    pub fn dispatch_pending_units(&mut self) -> io::Result<()> {
        loop {
            let candidates: Vec<String> = self.pending_units.iter().cloned().collect();
            let ready = self.dag.ready_to_spawn(&candidates);
            if ready.is_empty() {
                break;
            }
            let mut spawned_any = false;
            for name in &ready {
                if self.pending_units.contains(name) {
                    self.spawn_unit(name)?;
                    // Progress, not membership: only count it if spawn_unit
                    // actually left the pending set, else this can hard-hang.
                    if !self.pending_units.contains(name) {
                        spawned_any = true;
                    }
                }
            }
            if !spawned_any {
                break;
            }
        }
        Ok(())
    }

    /// Spawn a single unit.
    pub fn spawn_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let Some(node) = self.dag.get(unit_name) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unit not found: {}", unit_name),
            ));
        };

        if node.state == UnitState::Active
            || node.state == UnitState::Activating
            || node.state == UnitState::Deactivating
        {
            self.pending_units.remove(unit_name);
            return Ok(());
        }

        if !node.unit.conditions_met() {
            self.log_msg(&format!(
                "Unit {} condition check failed, skipping",
                unit_name
            ));
            self.dag.set_state(unit_name, UnitState::Inactive);
            self.pending_units.remove(unit_name);
            return Ok(());
        }

        match node.unit.kind {
            UnitKind::Target => {
                // Targets are synchronization points, immediately mark active
                self.dag.set_state(unit_name, UnitState::Active);
                self.pending_units.remove(unit_name);
                self.log_msg(&format!("Reached target: {}", unit_name));
                return Ok(());
            }
            UnitKind::Socket => {
                if let Some(ref sock) = node.unit.socket {
                    // C5/B13: a socket that fails to bind must report Failed,
                    // never Active.
                    if let Err(e) = self.sockets.bind_socket(unit_name, sock) {
                        self.log_msg(&format!("Socket unit {} failed to bind: {}", unit_name, e));
                        self.dag.set_state(unit_name, UnitState::Failed);
                        self.pending_units.remove(unit_name);
                        return Ok(());
                    }
                    self.dag.set_state(unit_name, UnitState::Active);
                    self.pending_units.remove(unit_name);
                    self.log_msg(&format!("Listening on socket: {}", unit_name));
                }
                return Ok(());
            }
            UnitKind::Service => {}
            _ => {
                self.dag.set_state(unit_name, UnitState::Active);
                self.pending_units.remove(unit_name);
                return Ok(());
            }
        }

        let svc = node.unit.service.clone().unwrap_or_default();
        if svc.exec_start.is_empty() {
            // B6: a unit with no usable ExecStart must be Failed, never Active.
            self.log_msg(&format!(
                "Unit {} has no usable ExecStart (parse failed?); marking failed",
                unit_name
            ));
            self.dag.set_state(unit_name, UnitState::Failed);
            self.pending_units.remove(unit_name);
            return Ok(());
        }

        let cmd = &svc.exec_start[0];
        let env_map = self.build_unit_env(unit_name, node);

        // Execute ExecStartPre commands synchronously
        for pre in &svc.exec_start_pre {
            match run_command_sync(pre, &env_map, svc.working_directory.as_deref()) {
                Ok(code) if code != 0 && !pre.ignore_failure => {
                    self.log_msg(&format!(
                        "Unit {} ExecStartPre failed with code {}",
                        unit_name, code
                    ));
                    self.dag.set_state(unit_name, UnitState::Failed);
                    self.pending_units.remove(unit_name);
                    return Ok(());
                }
                Err(e) if !pre.ignore_failure => {
                    self.log_msg(&format!("Unit {} ExecStartPre error: {}", unit_name, e));
                    self.dag.set_state(unit_name, UnitState::Failed);
                    self.pending_units.remove(unit_name);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Socket activation file descriptors
        let socket_fds = self.sockets.sockets_for_service(unit_name);

        // Failure-report pipe for Simple and Exec types: child writes errno
        // pre-execve (or on execve failure); parent distinguishes "failed to
        // start" from "started then exited". Forking/Notify/Oneshot keep the
        // legacy fire-and-observe semantics.
        let report_failures = matches!(svc.service_type, ServiceType::Simple | ServiceType::Exec);
        let mut exec_pipe = [-1; 2];
        if report_failures {
            unsafe {
                if libc::pipe2(exec_pipe.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            // C4: fork failure must not leak the exec pipe fds.
            if report_failures {
                unsafe {
                    libc::close(exec_pipe[0]);
                    libc::close(exec_pipe[1]);
                }
            }
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            // Child process
            crate::mount::place_in_cgroup(0);
            if report_failures {
                unsafe { libc::close(exec_pipe[0]) };
            }

            // Close every inherited FD except the failure pipe write-end and
            // the socket-activation FDs (dup2'ed below). Without this the
            // child leaks the control/notify listeners, epoll and signalfd
            // into every service.
            {
                let mut keep = socket_fds.clone();
                if report_failures {
                    keep.push(exec_pipe[1]);
                }
                close_fds_except(&keep);
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
                match CString::new(cwd.as_str()) {
                    Ok(c_cwd) => unsafe {
                        if libc::chdir(c_cwd.as_ptr()) != 0 {
                            libc::_exit(126);
                        }
                    },
                    Err(_) => unsafe { libc::_exit(126) },
                }
            }

            // Fail-closed privilege drop: any configured user/group that
            // cannot be resolved or applied aborts the child instead of
            // silently running as root.
            let mut priv_fail = false;
            let mut target_uid: Option<libc::uid_t> = None;
            let mut target_gid: Option<libc::gid_t> = None;

            if let Some(ref user_name) = svc.user {
                let c_usr = match CString::new(user_name.as_str()) {
                    Ok(c) => c,
                    Err(_) => unsafe { libc::_exit(126) },
                };
                let pw = unsafe { libc::getpwnam(c_usr.as_ptr()) };
                if !pw.is_null() {
                    unsafe {
                        target_uid = Some((*pw).pw_uid);
                        target_gid = Some((*pw).pw_gid);
                        if libc::initgroups(c_usr.as_ptr(), (*pw).pw_gid) != 0 {
                            priv_fail = true;
                        }
                    }
                } else if let Ok(uid) = user_name.parse::<u32>() {
                    target_uid = Some(uid);
                } else {
                    priv_fail = true;
                }
            }

            if let Some(ref group_name) = svc.group {
                let c_grp = match CString::new(group_name.as_str()) {
                    Ok(c) => c,
                    Err(_) => unsafe { libc::_exit(126) },
                };
                let gr = unsafe { libc::getgrnam(c_grp.as_ptr()) };
                if !gr.is_null() {
                    target_gid = Some(unsafe { (*gr).gr_gid });
                } else if let Ok(gid) = group_name.parse::<u32>() {
                    target_gid = Some(gid);
                } else {
                    priv_fail = true;
                }
            }

            if !svc.supplementary_groups.is_empty() {
                let mut gids: Vec<libc::gid_t> = Vec::new();
                let num_existing = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
                if num_existing > 0 {
                    gids.resize(num_existing as usize, 0);
                    unsafe { libc::getgroups(num_existing, gids.as_mut_ptr()) };
                }
                for grp in &svc.supplementary_groups {
                    let c_grp = match CString::new(grp.as_str()) {
                        Ok(c) => c,
                        Err(_) => {
                            priv_fail = true;
                            break;
                        }
                    };
                    let gr = unsafe { libc::getgrnam(c_grp.as_ptr()) };
                    if !gr.is_null() {
                        let gid = unsafe { (*gr).gr_gid };
                        if !gids.contains(&gid) {
                            gids.push(gid);
                        }
                    } else if let Ok(gid) = grp.parse::<libc::gid_t>() {
                        if !gids.contains(&gid) {
                            gids.push(gid);
                        }
                    } else {
                        priv_fail = true;
                        break;
                    }
                }
                if !priv_fail
                    && !gids.is_empty()
                    && unsafe { libc::setgroups(gids.len(), gids.as_ptr()) } != 0
                {
                    priv_fail = true;
                }
            }

            if let Some(gid) = target_gid {
                if unsafe { libc::setgid(gid) } != 0 {
                    priv_fail = true;
                }
            }

            if let Some(uid) = target_uid {
                if unsafe { libc::setuid(uid) } != 0 {
                    priv_fail = true;
                }
            }

            if priv_fail {
                unsafe { libc::_exit(126) };
            }

            // Apply resource limits
            if let Some(nofile) = svc.limit_nofile {
                let rlim = libc::rlimit {
                    rlim_cur: nofile,
                    rlim_max: nofile,
                };
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) };
            }
            if let Some(memlock) = svc.limit_memlock {
                let rlim = libc::rlimit {
                    rlim_cur: memlock,
                    rlim_max: memlock,
                };
                unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
            }
            if let Some(nproc) = svc.limit_nproc {
                let rlim = libc::rlimit {
                    rlim_cur: nproc,
                    rlim_max: nproc,
                };
                unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &rlim) };
            }

            // Apply OOM score adjust
            if let Some(oom) = svc.oom_score_adjust {
                let _ = MemorySupervisor::apply_oom_score_adj(unsafe { libc::getpid() }, oom);
            }

            let mut child_env_map = env_map.clone();
            // B4: MAINPID/WATCHDOG_PID must be authoritative in the child via
            // getpid(), not the stale pre-fork node.pid (usually None -> "0").
            {
                let my_pid = unsafe { libc::getpid() };
                child_env_map.insert("MAINPID".to_string(), my_pid.to_string());
                if svc.watchdog_sec > Duration::ZERO {
                    child_env_map.insert("WATCHDOG_PID".to_string(), my_pid.to_string());
                }
            }

            // Socket activation fds: pass sockets starting at fd 3 (SD_LISTEN_FDS_START)
            if !socket_fds.is_empty() {
                child_env_map.insert("LISTEN_FDS".to_string(), socket_fds.len().to_string());
                child_env_map.insert(
                    "LISTEN_PID".to_string(),
                    unsafe { libc::getpid() }.to_string(),
                );

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

            // Build CString args with systemd-compliant argument expansion.
            // Any NUL byte in the unit file is a hard start failure, not a panic.
            let resolved_bin = resolve_binary(&cmd.binary).unwrap_or_else(|| cmd.binary.clone());
            let mut c_args = Vec::new();
            let bin_cstr = match CString::new(resolved_bin.as_str()) {
                Ok(c) => c,
                Err(_) => unsafe { libc::_exit(126) },
            };
            c_args.push(bin_cstr);
            for arg in expand_command_args(&cmd.args, &child_env_map) {
                match CString::new(arg) {
                    Ok(c) => c_args.push(c),
                    Err(_) => unsafe { libc::_exit(126) },
                }
            }
            let mut arg_ptrs: Vec<*const libc::c_char> =
                c_args.iter().map(|s| s.as_ptr()).collect();
            arg_ptrs.push(std::ptr::null());

            // Build CString env
            let mut c_envs = Vec::new();
            for (k, v) in child_env_map {
                match CString::new(format!("{}={}", k, v)) {
                    Ok(c) => c_envs.push(c),
                    Err(_) => unsafe { libc::_exit(126) },
                }
            }
            let mut env_ptrs: Vec<*const libc::c_char> =
                c_envs.iter().map(|s| s.as_ptr()).collect();
            env_ptrs.push(std::ptr::null());

            unsafe {
                libc::execve(c_args[0].as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr());
                // If execve fails, write errno to pipe so parent knows it failed
                if report_failures {
                    let err = *libc::__errno_location();
                    libc::write(
                        exec_pipe[1],
                        &err as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>(),
                    );
                    libc::close(exec_pipe[1]);
                }
                libc::_exit(127);
            }
        }

        // Parent process
        crate::mount::place_in_cgroup(pid);
        let mut exec_succeeded = false;
        if report_failures {
            unsafe {
                libc::close(exec_pipe[1]);
                // P3: never block PID 1 forever on post-fork NSS
                // (getpwnam/initgroups). Poll 2s, then treat timeout as
                // unknown (assume started so boot can proceed).
                let mut pfd = libc::pollfd {
                    fd: exec_pipe[0],
                    events: libc::POLLIN,
                    revents: 0,
                };
                let pr = libc::poll(&mut pfd, 1, 2000);
                if pr == 0 {
                    self.log_msg(&format!(
                        "Unit {} exec pipe timeout after 2s; assuming started",
                        unit_name
                    ));
                    libc::close(exec_pipe[0]);
                    exec_succeeded = true;
                } else {
                    let mut err_code: libc::c_int = 0;
                    let n = libc::read(
                        exec_pipe[0],
                        &mut err_code as *mut _ as *mut libc::c_void,
                        std::mem::size_of::<libc::c_int>(),
                    );
                    libc::close(exec_pipe[0]);
                    if n == 0 {
                        // Pipe closed on successful child execve (due to O_CLOEXEC)
                        exec_succeeded = true;
                    } else if n < 0 {
                        let e = io::Error::last_os_error();
                        self.log_msg(&format!("Unit {} exec pipe read error: {}", unit_name, e));
                        exec_succeeded = false;
                    } else {
                        exec_succeeded = false;
                        self.log_msg(&format!(
                            "Unit {} execve failed with errno {}",
                            unit_name, err_code
                        ));
                    }
                }
            }
        }

        self.pid_to_unit.insert(pid, unit_name.to_string());
        self.dag.set_pid(unit_name, Some(pid));

        if svc.watchdog_sec > Duration::ZERO {
            self.last_watchdogs
                .insert(unit_name.to_string(), Instant::now());
        }

        let initial_state = match svc.service_type {
            ServiceType::Simple | ServiceType::Exec => {
                self.pending_units.remove(unit_name);
                if exec_succeeded {
                    UnitState::Active
                } else {
                    UnitState::Failed
                }
            }
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
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unit not found: {}", unit_name),
            ));
        };

        self.pending_units.remove(unit_name);
        let svc = node.unit.service.clone().unwrap_or_default();
        let env_map = self.build_unit_env(unit_name, node);

        // Run ExecStop commands
        for stop_cmd in &svc.exec_stop {
            let _ = run_command_sync(stop_cmd, &env_map, svc.working_directory.as_deref());
        }

        if let Some(pid) = node.pid {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            self.dag.set_state(unit_name, UnitState::Deactivating);
            self.stopping_since
                .insert(unit_name.to_string(), Instant::now());
            self.log_msg(&format!(
                "Stopping {}: Sent SIGTERM to PID {}",
                unit_name, pid
            ));
        } else {
            self.dag.set_state(unit_name, UnitState::Inactive);
            self.stopping_since.remove(unit_name);
        }

        self.last_watchdogs.remove(unit_name);
        self.watchdog_aborted.remove(unit_name);
        Ok(())
    }

    /// Reload a running unit.
    pub fn reload_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let Some(node) = self.dag.get(unit_name) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unit not found: {}", unit_name),
            ));
        };

        let svc = node.unit.service.clone().unwrap_or_default();
        let env_map = self.build_unit_env(unit_name, node);

        for reload_cmd in &svc.exec_reload {
            let _ = run_command_sync(reload_cmd, &env_map, svc.working_directory.as_deref());
        }

        self.log_msg(&format!("Reloaded {}", unit_name));
        Ok(())
    }

    /// Deterministic unit restart with explicit deactivation sequencing and restart queueing.
    pub fn restart_unit(&mut self, unit_name: &str) -> io::Result<()> {
        let Some(node) = self.dag.get(unit_name) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unit not found: {}", unit_name),
            ));
        };

        if node.state == UnitState::Deactivating {
            self.restart_queue.insert(unit_name.to_string());
            return Ok(());
        }

        if node.state == UnitState::Active || node.state == UnitState::Activating {
            if node.pid.is_some() {
                self.restart_queue.insert(unit_name.to_string());
                self.stop_unit(unit_name)?;
            } else {
                self.stop_unit(unit_name)?;
                self.start_unit(unit_name)?;
            }
        } else {
            self.start_unit(unit_name)?;
        }
        Ok(())
    }

    /// Handle sd_notify messages from child processes.
    /// Identity comes from SO_PASSCRED (sender_pid) only. MAINPID in the
    /// payload is a hint and is accepted only when the sender already owns
    /// the unit (C3). No single-activating fallback: untrusted datagrams
    /// must not complete another unit's startup.
    pub fn handle_notify_message(&mut self, msg: &NotifyMessage, sender_pid: Option<i32>) {
        let unit_name = sender_pid.and_then(|p| self.pid_to_unit.get(&p).cloned());

        if let Some(ref name) = unit_name {
            if msg.watchdog {
                self.last_watchdogs.insert(name.clone(), Instant::now());
                self.watchdog_aborted.remove(name);
            }
            if msg.ready {
                self.dag.set_state(name, UnitState::Active);
                self.pending_units.remove(name);
                self.log_msg(&format!("Service {} reported ready via sd_notify", name));

                // Execute ExecStartPost if configured
                if let Some(node) = self.dag.get(name) {
                    if let Some(ref svc) = node.unit.service {
                        let env_map = self.build_unit_env(name, node);
                        for post in &svc.exec_start_post {
                            let _ =
                                run_command_sync(post, &env_map, svc.working_directory.as_deref());
                        }
                    }
                }

                // Dependent units might now be unblocked
                let _ = self.dispatch_pending_units();
            }
            if let Some(ref status) = msg.status {
                self.log_msg(&format!("Service {} status: {}", name, status));
            }
            if let Some(err) = msg.errno {
                self.log_msg(&format!("Service {} reported errno: {}", name, err));
            }
            if let Some(new_pid) = msg.mainpid {
                // C3: only the unit's own currently-tracked process may
                // re-parent via MAINPID. sender_pid must already map to this
                // unit, and the claimed pid must be valid.
                if new_pid <= 1 {
                    return;
                }
                let owned = sender_pid
                    .and_then(|p| self.pid_to_unit.get(&p))
                    .is_some_and(|n| n == name);
                if !owned {
                    return;
                }
                // Remove previous PID mapping for unit_name to avoid stale lookup or terminating new daemon
                self.pid_to_unit.retain(|_pid, n| n != name);
                self.dag.set_pid(name, Some(new_pid));
                self.pid_to_unit.insert(new_pid, name.clone());
            }
        }
    }

    pub fn handle_process_exits(&mut self, exits: &[ProcessExitInfo]) {
        for exit in exits {
            // Check if this was a vendor HAL daemon (tracked in container.running_hal_pids)
            if let Some(hal_name) = self.container.handle_hal_exit(exit.pid) {
                self.log_msg(&format!(
                    "Vendor HAL {} terminated, self-healing watchdog triggered",
                    hal_name
                ));
            }

            if let Some(unit_name) = self.pid_to_unit.remove(&exit.pid) {
                let is_success = exit.exited_cleanly && exit.exit_code == 0;
                self.log_msg(&format!(
                    "Unit {} (PID {}) exited: clean={}, code={}",
                    unit_name, exit.pid, exit.exited_cleanly, exit.exit_code
                ));

                // Always clear watchdog and clear PID (unless re-adopted below)
                self.last_watchdogs.remove(&unit_name);
                let was_watchdog = self.watchdog_aborted.remove(&unit_name);
                self.dag.set_pid(&unit_name, None);

                // Explicit stop check: if unit was deactivating, transition directly to Inactive
                // and do NOT resurrect even if Restart=always
                if let Some(node) = self.dag.get(&unit_name) {
                    if node.state == UnitState::Deactivating {
                        self.pending_units.remove(&unit_name);
                        self.stopping_since.remove(&unit_name);
                        if self.restart_queue.remove(&unit_name) {
                            self.log_msg(&format!(
                                "Unit {} stopped, executing queued restart",
                                unit_name
                            ));
                            // C4: dedup + rate-limit queued restarts as well.
                            if !self.restart_allowed(&unit_name) {
                                self.log_msg(&format!(
                                    "Unit {} hit StartLimitBurst={}; entering failed state",
                                    unit_name, self.start_burst
                                ));
                                self.dag.set_state(&unit_name, UnitState::Failed);
                            } else {
                                if !self.pending_restarts.iter().any(|(n, _)| n == &unit_name) {
                                    self.pending_restarts
                                        .push((unit_name.clone(), Instant::now()));
                                }
                                self.dag.set_state(&unit_name, UnitState::Activating);
                            }
                        } else {
                            self.dag.set_state(&unit_name, UnitState::Inactive);
                            self.log_msg(&format!(
                                "Unit {} stopped cleanly after deactivation request",
                                unit_name
                            ));
                        }
                        continue;
                    }
                }

                let (svc_type, restart_policy, restart_sec, pid_file) =
                    if let Some(node) = self.dag.get(&unit_name) {
                        let svc = node.unit.service.as_ref();
                        (
                            svc.map(|s| s.service_type).unwrap_or(ServiceType::Simple),
                            svc.map(|s| s.restart)
                                .unwrap_or(utim_core::unit::RestartPolicy::No),
                            svc.map(|s| s.restart_sec).unwrap_or(Duration::from_secs(1)),
                            svc.and_then(|s| s.pid_file.clone()),
                        )
                    } else {
                        (
                            ServiceType::Simple,
                            utim_core::unit::RestartPolicy::No,
                            Duration::from_secs(1),
                            None,
                        )
                    };

                // Handle Type=forking grandchild adoption
                if svc_type == ServiceType::Forking && is_success {
                    let mut adopted_pid = None;
                    if let Some(ref pf) = pid_file {
                        if let Ok(content) = fs::read_to_string(pf) {
                            if let Ok(parsed) = content.trim().parse::<i32>() {
                                if parsed > 0 {
                                    adopted_pid = Some(parsed);
                                }
                            }
                        }
                    }

                    if let Some(new_pid) = adopted_pid {
                        self.dag.set_pid(&unit_name, Some(new_pid));
                        self.pid_to_unit.insert(new_pid, unit_name.clone());
                        self.dag.set_state(&unit_name, UnitState::Active);
                        self.pending_units.remove(&unit_name);
                        self.log_msg(&format!(
                            "Forking service {} adopted PID {} from PID file",
                            unit_name, new_pid
                        ));
                    } else {
                        // No PID file entry: the daemon double-forked without
                        // leaving an identity. Do NOT fabricate Active; the
                        // exit will be reaped as a start failure below.
                        self.dag.set_state(&unit_name, UnitState::Failed);
                        self.pending_units.remove(&unit_name);
                        self.log_msg(&format!(
                            "Forking service {} exited without a PID file entry; marking Failed",
                            unit_name
                        ));
                    }

                    if let Some(node) = self.dag.get(&unit_name) {
                        if let Some(ref svc) = node.unit.service {
                            let env_map = self.build_unit_env(&unit_name, node);
                            for post in &svc.exec_start_post {
                                let _ = run_command_sync(
                                    post,
                                    &env_map,
                                    svc.working_directory.as_deref(),
                                );
                            }
                        }
                    }

                    let _ = self.dispatch_pending_units();
                    continue;
                }

                if svc_type == ServiceType::Oneshot {
                    self.pending_units.remove(&unit_name);
                    if is_success {
                        self.dag.set_state(&unit_name, UnitState::Active);
                        let _ = self.dispatch_pending_units();
                    } else {
                        self.dag.set_state(&unit_name, UnitState::Failed);
                    }
                } else {
                    self.pending_units.remove(&unit_name);
                    let is_clean_signal = exit.signal.is_some_and(|sig| {
                        sig == libc::SIGINT || sig == libc::SIGTERM || sig == libc::SIGHUP
                    });
                    let should_restart = match restart_policy {
                        utim_core::unit::RestartPolicy::Always => true,
                        utim_core::unit::RestartPolicy::OnFailure => !is_success,
                        utim_core::unit::RestartPolicy::OnSuccess => is_success,
                        utim_core::unit::RestartPolicy::OnAbnormal => {
                            was_watchdog || (!exit.exited_cleanly && !is_clean_signal)
                        }
                        utim_core::unit::RestartPolicy::OnAbort => {
                            !exit.exited_cleanly && !is_clean_signal
                        }
                        _ => false,
                    };

                    if should_restart {
                        // C4: rate-limit restarts, floor backoff at 100ms,
                        // dedup pending entries so a duplicate cannot orphan
                        // a live process via set_pid overwrite.
                        if !self.restart_allowed(&unit_name) {
                            self.log_msg(&format!(
                                "Unit {} hit StartLimitBurst={}; entering failed state",
                                unit_name, self.start_burst
                            ));
                            self.dag.set_state(&unit_name, UnitState::Failed);
                        } else {
                            let delay = restart_sec.max(Duration::from_millis(100));
                            self.log_msg(&format!(
                                "Scheduling restart for {} in {:?}",
                                unit_name, delay
                            ));
                            if !self.pending_restarts.iter().any(|(n, _)| n == &unit_name) {
                                self.pending_restarts
                                    .push((unit_name.clone(), Instant::now() + delay));
                            }
                            self.dag.set_state(&unit_name, UnitState::Activating);
                        }
                    } else if is_success {
                        self.dag.set_state(&unit_name, UnitState::Inactive);
                    } else {
                        self.dag.set_state(&unit_name, UnitState::Failed);
                    }
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
            self.dag.set_state(&name, UnitState::Inactive);
            self.pending_units.insert(name.clone());
            let _ = self.spawn_unit(&name);
            let _ = self.dispatch_pending_units();
        }
    }

    /// Calculate the next epoll timeout in milliseconds based on pending restarts,
    /// stopping timeouts, watchdogs, and PSI pressure checks.
    pub fn next_deadline_ms(&self, last_psi_check: &Instant) -> i32 {
        let now = Instant::now();
        let mut min_timeout_ms = 5000i64;

        // PSI check deadline (5s interval)
        let psi_elapsed = last_psi_check.elapsed().as_millis().min(i64::MAX as u128) as i64;
        let psi_remaining = 5000i64.saturating_sub(psi_elapsed).max(50);
        min_timeout_ms = min_timeout_ms.min(psi_remaining);

        // Pending delayed restarts
        for (_, restart_at) in &self.pending_restarts {
            if *restart_at > now {
                let rem = restart_at
                    .checked_duration_since(now)
                    .unwrap_or(Duration::ZERO)
                    .as_millis()
                    .min(5000) as i64;
                min_timeout_ms = min_timeout_ms.min(rem.max(10));
            } else {
                min_timeout_ms = min_timeout_ms.min(10);
            }
        }

        // Stopping timeouts (SIGTERM -> SIGKILL escalation)
        for since in self.stopping_since.values() {
            let elapsed = now
                .checked_duration_since(*since)
                .unwrap_or(Duration::ZERO)
                .as_millis()
                .min(i64::MAX as u128) as i64;
            let timeout = self.stop_timeout.as_millis().min(i64::MAX as u128) as i64;
            let rem = timeout.saturating_sub(elapsed).max(50);
            min_timeout_ms = min_timeout_ms.min(rem);
        }

        // Watchdog deadlines
        for (name, last_ping) in &self.last_watchdogs {
            if let Some(node) = self.dag.get(name) {
                if let Some(ref svc) = node.unit.service {
                    if !svc.watchdog_sec.is_zero() {
                        let wd_ms = svc.watchdog_sec.as_millis().min(i64::MAX as u128) as i64;
                        let elapsed = now
                            .checked_duration_since(*last_ping)
                            .unwrap_or(Duration::ZERO)
                            .as_millis()
                            .min(i64::MAX as u128) as i64;
                        let rem = wd_ms.saturating_sub(elapsed).max(50);
                        min_timeout_ms = min_timeout_ms.min(rem);
                    }
                }
            }
        }

        min_timeout_ms.clamp(50, 5000) as i32
    }

    /// Escalate stops that ignored SIGTERM: after `stop_timeout`, SIGKILL.
    /// Called once per event-loop iteration (event-driven, no extra wakeups).
    pub fn process_stop_timeouts(&mut self) {
        let now = Instant::now();
        let mut escalate = Vec::new();
        for (name, since) in &self.stopping_since {
            if let Some(node) = self.dag.get(name) {
                if node.state == UnitState::Deactivating
                    && now.checked_duration_since(*since).unwrap_or(Duration::ZERO)
                        >= self.stop_timeout
                {
                    escalate.push((name.clone(), node.pid));
                }
            }
        }
        for (name, pid) in escalate {
            // Refresh the deadline so a SIGKILL-ignoring (D-state) process
            // is logged once per interval rather than every iteration.
            self.stopping_since.insert(name.clone(), now);
            if let Some(p) = pid {
                self.log_msg(&format!(
                    "Unit {} (PID {}) ignored SIGTERM; sending SIGKILL",
                    name, p
                ));
                unsafe {
                    libc::kill(p, libc::SIGKILL);
                }
            } else {
                self.stopping_since.remove(&name);
                self.dag.set_state(&name, UnitState::Inactive);
            }
        }
        // Drop tracking for units that left Deactivating by another path.
        self.stopping_since
            .retain(|name, _| match self.dag.get(name) {
                Some(n) => n.state == UnitState::Deactivating,
                None => false,
            });
    }

    /// Check watchdog timers for active services and restart any that timed out.
    /// First timeout sends SIGABRT (core dump for diagnostics); a second
    /// consecutive timeout escalates to SIGKILL. A fresh WATCHDOG=1 ping
    /// clears the escalation (see `handle_notify_message`).
    pub fn check_watchdogs(&mut self) {
        let now = Instant::now();
        let mut expired = Vec::new();
        for (name, last_ping) in &self.last_watchdogs {
            if let Some(node) = self.dag.get(name) {
                if node.state == UnitState::Active {
                    if let Some(ref svc) = node.unit.service {
                        if svc.watchdog_sec > Duration::ZERO
                            && now
                                .checked_duration_since(*last_ping)
                                .is_some_and(|d| d > svc.watchdog_sec)
                        {
                            expired.push((name.clone(), node.pid));
                        }
                    }
                }
            }
        }
        for (name, pid) in expired {
            let already_aborted = self.watchdog_aborted.contains(&name);
            if already_aborted {
                self.log_msg(&format!(
                    "Watchdog timeout for service {} persists; sending SIGKILL...",
                    name
                ));
                self.last_watchdogs.remove(&name);
                self.watchdog_aborted.remove(&name);
                if let Some(p) = pid {
                    unsafe {
                        libc::kill(p, libc::SIGKILL);
                    }
                }
            } else {
                self.log_msg(&format!(
                    "Watchdog timeout for service {}! Sending SIGABRT...",
                    name
                ));
                // Grant one more interval before SIGKILL escalation.
                self.last_watchdogs.insert(name.clone(), now);
                self.watchdog_aborted.insert(name.clone());
                if let Some(p) = pid {
                    unsafe {
                        libc::kill(p, libc::SIGABRT);
                    }
                }
            }
        }
    }

    /// Graceful teardown of all supervised units: SIGTERM, bounded timeout, SIGKILL, sync.
    pub fn shutdown_all_units(&mut self) {
        self.log_msg("Initiating graceful teardown of supervised units...");
        let active_procs: Vec<(i32, String)> = self
            .pid_to_unit
            .iter()
            .map(|(&p, n)| (p, n.clone()))
            .collect();
        for (pid, ref name) in &active_procs {
            self.log_msg(&format!("Sending SIGTERM to {} (PID {})", name, pid));
            unsafe {
                libc::kill(*pid, libc::SIGTERM);
            }
        }

        let start = Instant::now();
        while !self.pid_to_unit.is_empty() && start.elapsed() < Duration::from_millis(1000) {
            let exits = reap_zombies();
            if !exits.is_empty() {
                self.handle_process_exits(&exits);
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let remaining_procs: Vec<(i32, String)> = self
            .pid_to_unit
            .iter()
            .map(|(&p, n)| (p, n.clone()))
            .collect();
        for (pid, ref name) in &remaining_procs {
            self.log_msg(&format!(
                "Unit {} (PID {}) did not exit within timeout; sending SIGKILL",
                name, pid
            ));
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }

        let exits = reap_zombies();
        if !exits.is_empty() {
            self.handle_process_exits(&exits);
        }
    }

    pub fn log_msg(&mut self, msg: &str) {
        // Never use println!: with panic="abort" an EPIPE (closed log
        // consumer) would SIGABRT PID 1 and kill the namespace. Use raw
        // libc::write to log_fd and ignore all errors (EPIPE/EAGAIN).
        let formatted = format!("[UTIM] {}\n", msg);
        self.log_ring.write_overwrite(formatted.as_bytes());
        unsafe {
            libc::write(
                self.log_fd,
                formatted.as_ptr() as *const libc::c_void,
                formatted.len(),
            );
        }
    }
}

/// Close all open file descriptors >= 3 except those in `keep`.
/// Called in the fork child before execve so services never inherit the
/// supervisor's epoll/signalfd/listeners. Uses /proc/self/fd when available
/// with an _SC_OPEN_MAX fallback (no allocation on failure paths of note).
fn close_fds_except(keep: &[i32]) {
    // 1. Try close_range syscall first (Linux 5.9+)
    #[cfg(target_os = "linux")]
    {
        let mut sorted_keep = keep.to_vec();
        sorted_keep.sort_unstable();
        sorted_keep.retain(|&fd| fd >= 3);
        sorted_keep.dedup();

        let mut start: u32 = 3;
        let mut ok = true;
        for &k in &sorted_keep {
            let k = k as u32;
            if k > start {
                let ret = unsafe { libc::syscall(libc::SYS_close_range, start, k - 1, 0) };
                if ret != 0 {
                    ok = false;
                    break;
                }
            }
            start = k.saturating_add(1);
        }
        if ok {
            let ret = unsafe { libc::syscall(libc::SYS_close_range, start, !0u32, 0) };
            if ret == 0 {
                return;
            }
        }
    }

    // 2. Iterate /proc/self/fd using libc opendir to know the dirfd and avoid double-close
    let c_proc_fd = b"/proc/self/fd\0";
    let dir = unsafe { libc::opendir(c_proc_fd.as_ptr() as *const libc::c_char) };
    if !dir.is_null() {
        let dir_fd = unsafe { libc::dirfd(dir) };
        let mut to_close = [0i32; 1024];
        let mut count = 0;

        loop {
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                break;
            }
            let d_name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if let Ok(s) = d_name.to_str() {
                if let Ok(n) = s.parse::<i32>() {
                    if n >= 3 && n != dir_fd && !keep.contains(&n) {
                        if count < to_close.len() {
                            to_close[count] = n;
                            count += 1;
                        } else {
                            unsafe { libc::close(n) };
                        }
                    }
                }
            }
        }
        unsafe { libc::closedir(dir) };

        for &fd in &to_close[..count] {
            unsafe { libc::close(fd) };
        }
    } else {
        let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        let max = if max > 0 { max.min(4096) } else { 1024 };
        for fd in 3..max {
            let fd = fd as i32;
            if !keep.contains(&fd) {
                unsafe { libc::close(fd) };
            }
        }
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
        Ok(status) => {
            // A signal death is NOT success: surface it as failure so
            // ExecStartPre with `!ignore_failure` correctly fails the unit.
            match status.code() {
                Some(code) => Ok(code),
                None => Ok(128),
            }
        }
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
        let pid = supervisor
            .dag
            .get("test-oneshot.service")
            .unwrap()
            .pid
            .unwrap();

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
        let temp_dir =
            std::env::temp_dir().join(format!("utim_test_search_paths_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let etc_dir = temp_dir.join("etc/systemd/system");
        let usr_dir = temp_dir.join("usr/lib/systemd/system");
        fs::create_dir_all(&etc_dir).unwrap();
        fs::create_dir_all(&usr_dir).unwrap();

        // 1. Service defined in usr_dir, overridden in etc_dir
        fs::write(
            usr_dir.join("app.service"),
            "[Unit]\nDescription=App UsrLib\n[Service]\nExecStart=/bin/app\n",
        )
        .unwrap();
        fs::write(
            etc_dir.join("app.service"),
            "[Unit]\nDescription=App Etc\n[Service]\nExecStart=/bin/app\n",
        )
        .unwrap();

        // 2. Service masked in etc_dir via /dev/null symlink
        fs::write(
            usr_dir.join("masked.service"),
            "[Unit]\nDescription=Masked Unit\n[Service]\nExecStart=/bin/bad\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("/dev/null", etc_dir.join("masked.service")).unwrap();

        // 3. Drop-in configuration
        let dropin_dir = etc_dir.join("app.service.d");
        fs::create_dir_all(&dropin_dir).unwrap();
        fs::write(
            dropin_dir.join("10-override.conf"),
            "[Service]\nRestartSec=15s\n",
        )
        .unwrap();

        // 4. .wants directory
        let wants_dir = etc_dir.join("multi-user.target.wants");
        fs::create_dir_all(&wants_dir).unwrap();
        fs::write(wants_dir.join("app.service"), "").unwrap();

        // Create dummy multi-user.target in usr_dir
        fs::write(
            usr_dir.join("multi-user.target"),
            "[Unit]\nDescription=Multi User Target\n",
        )
        .unwrap();

        let mut supervisor = Supervisor::new();
        let search_paths = vec![etc_dir, usr_dir];
        supervisor.load_systemd_units(&search_paths);

        // Verify priority: App Etc won over App UsrLib
        let app_node = supervisor
            .dag
            .get("app.service")
            .expect("app.service should be loaded");
        assert_eq!(app_node.unit.unit.description, "App Etc");
        assert_eq!(
            app_node.unit.service.as_ref().unwrap().restart_sec,
            std::time::Duration::from_secs(15)
        );

        // Verify masking: masked.service was NOT loaded
        assert!(supervisor.dag.get("masked.service").is_none());

        // Verify .wants: multi-user.target wants app.service
        let target_node = supervisor
            .dag
            .get("multi-user.target")
            .expect("target should be loaded");
        assert!(target_node
            .unit
            .unit
            .wants
            .contains(&"app.service".to_string()));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_supervisor_notify_handling() {
        let mut supervisor = Supervisor::new();

        let unit_content =
            "[Unit]\nDescription=Notify Daemon\n[Service]\nType=notify\nExecStart=/bin/daemon\n";
        let unit = utim_core::unit::parse_unit(
            "notify-app.service",
            Path::new("/test/notify-app.service"),
            unit_content,
        );
        supervisor.dag.insert(unit);

        // Transition to Activating with pid 9999 (kernel-credential identity).
        let node = supervisor.dag.get_mut("notify-app.service").unwrap();
        node.state = UnitState::Activating;
        node.pid = Some(9999);
        supervisor
            .pid_to_unit
            .insert(9999, "notify-app.service".to_string());

        // Case 1: Notify message with matching sender_pid
        let msg = NotifyMessage {
            ready: true,
            status: Some("Ready for requests".to_string()),
            watchdog: false,
            mainpid: None,
            errno: None,
        };
        supervisor.handle_notify_message(&msg, Some(9999));
        assert_eq!(
            supervisor.dag.get("notify-app.service").unwrap().state,
            UnitState::Active
        );

        // Reset to Activating
        let node = supervisor.dag.get_mut("notify-app.service").unwrap();
        node.state = UnitState::Activating;

        // Case 2: Notify message with None sender_pid must NOT activate:
        // identity comes from SO_PASSCRED only (C3). Untrusted datagrams
        // cannot complete another unit's startup.
        supervisor.handle_notify_message(&msg, None);
        assert_eq!(
            supervisor.dag.get("notify-app.service").unwrap().state,
            UnitState::Activating
        );
    }

    #[test]
    fn test_exec_type_active_transition() {
        let mut supervisor = Supervisor::new();
        let content =
            "[Unit]\nDescription=Exec Service\n[Service]\nType=exec\nExecStart=/bin/true\n";
        let unit = utim_core::unit::parse_unit(
            "exec-app.service",
            Path::new("/test/exec-app.service"),
            content,
        );
        supervisor.dag.insert(unit);

        supervisor.start_unit("exec-app.service").unwrap();
        assert_eq!(
            supervisor.dag.get("exec-app.service").unwrap().state,
            UnitState::Active
        );

        if let Some(pid) = supervisor.dag.get("exec-app.service").unwrap().pid {
            let exits = [ProcessExitInfo {
                pid,
                status: 0,
                exited_cleanly: true,
                exit_code: 0,
                signal: None,
            }];
            supervisor.handle_process_exits(&exits);
        }
    }

    #[test]
    fn test_explicit_stop_cancels_restart_always() {
        let mut supervisor = Supervisor::new();
        let content = "[Unit]\nDescription=Restart Always Daemon\n[Service]\nType=simple\nRestart=always\nExecStart=/bin/sleep 60\n";
        let unit = utim_core::unit::parse_unit(
            "always.service",
            Path::new("/test/always.service"),
            content,
        );
        supervisor.dag.insert(unit);

        supervisor.start_unit("always.service").unwrap();
        let pid = supervisor.dag.get("always.service").unwrap().pid.unwrap();
        assert_eq!(
            supervisor.dag.get("always.service").unwrap().state,
            UnitState::Active
        );

        // Explicit stop
        supervisor.stop_unit("always.service").unwrap();
        assert_eq!(
            supervisor.dag.get("always.service").unwrap().state,
            UnitState::Deactivating
        );

        // Process exit arrives
        let exits = [ProcessExitInfo {
            pid,
            status: 0,
            exited_cleanly: true,
            exit_code: 0,
            signal: None,
        }];
        supervisor.handle_process_exits(&exits);

        // State must be Inactive, and no pending restart scheduled
        assert_eq!(
            supervisor.dag.get("always.service").unwrap().state,
            UnitState::Inactive
        );
        assert!(supervisor.pending_restarts.is_empty());
    }

    #[test]
    fn test_exec_type_failure_detection() {
        let mut supervisor = Supervisor::new();
        let content = "[Unit]\nDescription=Failed Exec Service\n[Service]\nType=exec\nExecStart=/nonexistent/binary/path_12345\n";
        let unit = utim_core::unit::parse_unit(
            "failed-exec.service",
            Path::new("/test/failed-exec.service"),
            content,
        );
        supervisor.dag.insert(unit);

        supervisor.start_unit("failed-exec.service").unwrap();
        // State must be Failed, NOT Active!
        assert_eq!(
            supervisor.dag.get("failed-exec.service").unwrap().state,
            UnitState::Failed
        );
    }

    #[test]
    fn test_condition_check_skipped_no_infinite_loop() {
        let mut supervisor = Supervisor::new();
        let content = "[Unit]\nDescription=Condition Skipped\nConditionPathExists=/nonexistent/marker_file_987\n[Service]\nExecStart=/bin/true\n";
        let unit = utim_core::unit::parse_unit(
            "cond-skip.service",
            Path::new("/test/cond-skip.service"),
            content,
        );
        supervisor.dag.insert(unit);

        // Must complete immediately without infinite loop
        supervisor.start_unit("cond-skip.service").unwrap();
        assert_eq!(
            supervisor.dag.get("cond-skip.service").unwrap().state,
            UnitState::Inactive
        );
        assert!(supervisor.pending_units.is_empty());
    }

    #[test]
    fn test_restart_unit_without_pid() {
        let mut supervisor = Supervisor::new();
        let target_unit = utim_core::unit::parse_unit(
            "dummy.target",
            Path::new("/test/dummy.target"),
            "[Unit]\nDescription=Dummy Target\n",
        );
        supervisor.dag.insert(target_unit);

        supervisor.start_unit("dummy.target").unwrap();
        assert_eq!(
            supervisor.dag.get("dummy.target").unwrap().state,
            UnitState::Active
        );

        // Restarting a target must not hang waiting for a PID exit
        supervisor.restart_unit("dummy.target").unwrap();
        assert_eq!(
            supervisor.dag.get("dummy.target").unwrap().state,
            UnitState::Active
        );
        assert!(supervisor.restart_queue.is_empty());
    }

    #[test]
    fn test_process_exit_clears_pid() {
        let mut supervisor = Supervisor::new();
        let content =
            "[Unit]\nDescription=Oneshot Clean\n[Service]\nType=oneshot\nExecStart=/bin/true\n";
        let unit = utim_core::unit::parse_unit(
            "oneshot-clean.service",
            Path::new("/test/oneshot-clean.service"),
            content,
        );
        supervisor.dag.insert(unit);

        supervisor.start_unit("oneshot-clean.service").unwrap();
        let pid = supervisor
            .dag
            .get("oneshot-clean.service")
            .unwrap()
            .pid
            .unwrap();

        let exits = [ProcessExitInfo {
            pid,
            status: 0,
            exited_cleanly: true,
            exit_code: 0,
            signal: None,
        }];
        supervisor.handle_process_exits(&exits);

        let node = supervisor.dag.get("oneshot-clean.service").unwrap();
        assert_eq!(node.state, UnitState::Active);
        // PID must be cleared to None to prevent dangling PID and signal targeting!
        assert_eq!(node.pid, None);
    }

    #[test]
    fn test_restart_policy_on_abnormal_and_on_abort() {
        let mut supervisor = Supervisor::new();
        let content_abnormal = "[Unit]\nDescription=Abnormal\n[Service]\nRestart=on-abnormal\nExecStart=/bin/sleep 10\n";
        let unit_abnormal = utim_core::unit::parse_unit(
            "abnormal.service",
            Path::new("/test/abnormal.service"),
            content_abnormal,
        );
        supervisor.dag.insert(unit_abnormal);

        let content_abort =
            "[Unit]\nDescription=Abort\n[Service]\nRestart=on-abort\nExecStart=/bin/sleep 10\n";
        let unit_abort = utim_core::unit::parse_unit(
            "abort.service",
            Path::new("/test/abort.service"),
            content_abort,
        );
        supervisor.dag.insert(unit_abort);

        // 1. Clean exit (code 0) -> no restart for either
        supervisor
            .dag
            .set_state("abnormal.service", UnitState::Active);
        supervisor.dag.set_pid("abnormal.service", Some(1001));
        supervisor
            .pid_to_unit
            .insert(1001, "abnormal.service".to_string());
        supervisor.handle_process_exits(&[ProcessExitInfo {
            pid: 1001,
            status: 0,
            exited_cleanly: true,
            exit_code: 0,
            signal: None,
        }]);
        assert_eq!(
            supervisor.dag.get("abnormal.service").unwrap().state,
            UnitState::Inactive
        );
        assert!(supervisor.pending_restarts.is_empty());

        // 2. Non-zero exit (exit 1) -> no restart for on-abnormal or on-abort
        supervisor
            .dag
            .set_state("abnormal.service", UnitState::Active);
        supervisor.dag.set_pid("abnormal.service", Some(1002));
        supervisor
            .pid_to_unit
            .insert(1002, "abnormal.service".to_string());
        supervisor.handle_process_exits(&[ProcessExitInfo {
            pid: 1002,
            status: 256,
            exited_cleanly: true,
            exit_code: 1,
            signal: None,
        }]);
        assert_eq!(
            supervisor.dag.get("abnormal.service").unwrap().state,
            UnitState::Failed
        );
        assert!(supervisor.pending_restarts.is_empty());

        // 3. Clean signal (SIGTERM) -> no restart
        supervisor
            .dag
            .set_state("abnormal.service", UnitState::Active);
        supervisor.dag.set_pid("abnormal.service", Some(1003));
        supervisor
            .pid_to_unit
            .insert(1003, "abnormal.service".to_string());
        supervisor.handle_process_exits(&[ProcessExitInfo {
            pid: 1003,
            status: libc::SIGTERM,
            exited_cleanly: false,
            exit_code: -1,
            signal: Some(libc::SIGTERM),
        }]);
        assert_eq!(
            supervisor.dag.get("abnormal.service").unwrap().state,
            UnitState::Failed
        );
        assert!(supervisor.pending_restarts.is_empty());

        // 4. Abnormal signal (SIGABRT) -> restart scheduled for on-abnormal
        supervisor
            .dag
            .set_state("abnormal.service", UnitState::Active);
        supervisor.dag.set_pid("abnormal.service", Some(1004));
        supervisor
            .pid_to_unit
            .insert(1004, "abnormal.service".to_string());
        supervisor.handle_process_exits(&[ProcessExitInfo {
            pid: 1004,
            status: libc::SIGABRT,
            exited_cleanly: false,
            exit_code: -1,
            signal: Some(libc::SIGABRT),
        }]);
        assert_eq!(
            supervisor.dag.get("abnormal.service").unwrap().state,
            UnitState::Activating
        );
        assert_eq!(supervisor.pending_restarts.len(), 1);
        supervisor.pending_restarts.clear();

        // 5. Abnormal signal (SIGSEGV) -> restart scheduled for on-abort
        supervisor.dag.set_state("abort.service", UnitState::Active);
        supervisor.dag.set_pid("abort.service", Some(1005));
        supervisor
            .pid_to_unit
            .insert(1005, "abort.service".to_string());
        supervisor.handle_process_exits(&[ProcessExitInfo {
            pid: 1005,
            status: libc::SIGSEGV,
            exited_cleanly: false,
            exit_code: -1,
            signal: Some(libc::SIGSEGV),
        }]);
        assert_eq!(
            supervisor.dag.get("abort.service").unwrap().state,
            UnitState::Activating
        );
        assert_eq!(supervisor.pending_restarts.len(), 1);
        supervisor.pending_restarts.clear();

        // 6. Watchdog timeout abort -> restart scheduled for on-abnormal
        supervisor
            .dag
            .set_state("abnormal.service", UnitState::Active);
        supervisor.dag.set_pid("abnormal.service", Some(1006));
        supervisor
            .pid_to_unit
            .insert(1006, "abnormal.service".to_string());
        supervisor
            .watchdog_aborted
            .insert("abnormal.service".to_string());
        supervisor.handle_process_exits(&[ProcessExitInfo {
            pid: 1006,
            status: libc::SIGKILL,
            exited_cleanly: false,
            exit_code: -1,
            signal: Some(libc::SIGKILL),
        }]);
        assert_eq!(
            supervisor.dag.get("abnormal.service").unwrap().state,
            UnitState::Activating
        );
        assert_eq!(supervisor.pending_restarts.len(), 1);
    }
}
