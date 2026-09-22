//! End-to-end integration test suite for UTIM PID 1, Systemd Compatibility, MPG, and MMPS.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Duration;

use utim_core::dag::UnitDag;
use utim_core::ipc::{send_ipc_request, IpcRequest, IpcResponse};
use utim_core::mmps::MemorySupervisor;
use utim_core::mpg::MobilePowerGovernor;
use utim_core::unit::{parse_unit, ServiceType};

#[test]
fn test_complex_service_parsing() {
    let complex_unit = r#"
[Unit]
Description=PipeWire Multimedia Service
After=android-hal-audio.service
Requires=android-hal-audio.service
Wants=wireplumber.service
Conflicts=pulseaudio.service

[Service]
Type=notify
ExecStartPre=-/usr/bin/mkdir -p /run/user/1000/pipewire
ExecStart=/usr/bin/pipewire -c /etc/pipewire/pipewire.conf
Restart=on-failure
RestartSec=500ms
LimitMEMLOCK=67108864
OOMScoreAdjust=-800
Environment="PIPEWIRE_DEBUG=2" "XDG_RUNTIME_DIR=/run/user/1000"

[Install]
WantedBy=default.target
"#;

    let unit = parse_unit(
        "pipewire.service",
        Path::new("/etc/systemd/system/pipewire.service"),
        complex_unit,
    );
    assert_eq!(unit.name, "pipewire.service");
    assert_eq!(unit.unit.after, vec!["android-hal-audio.service"]);
    assert_eq!(unit.unit.requires, vec!["android-hal-audio.service"]);
    assert_eq!(unit.unit.conflicts, vec!["pulseaudio.service"]);

    let svc = unit.service.expect("Service section exists");
    assert_eq!(svc.service_type, ServiceType::Notify);
    assert_eq!(svc.restart_sec, Duration::from_millis(500));
    assert_eq!(svc.limit_memlock, Some(67108864));
    assert_eq!(svc.oom_score_adjust, Some(-800));
    assert_eq!(
        svc.environment.get("PIPEWIRE_DEBUG").map(|s| s.as_str()),
        Some("2")
    );

    assert_eq!(svc.exec_start_pre.len(), 1);
    assert!(svc.exec_start_pre[0].ignore_failure);
    assert_eq!(svc.exec_start_pre[0].binary, "/usr/bin/mkdir");

    assert_eq!(svc.exec_start.len(), 1);
    assert_eq!(svc.exec_start[0].binary, "/usr/bin/pipewire");
    assert_eq!(
        svc.exec_start[0].args,
        vec!["-c", "/etc/pipewire/pipewire.conf"]
    );

    assert_eq!(unit.install.wanted_by, vec!["default.target"]);
}

#[test]
fn test_dag_scheduler_ordering_and_dependency() {
    let mut dag = UnitDag::new();

    let hal_audio = parse_unit(
        "android-hal-audio.service",
        Path::new("/hal/android-hal-audio.service"),
        "[Unit]\nDescription=Android Audio HAL\n",
    );
    let pipewire = parse_unit(
        "pipewire.service",
        Path::new("/lib/pipewire.service"),
        "[Unit]\nDescription=PipeWire\nAfter=android-hal-audio.service\nRequires=android-hal-audio.service\n",
    );
    let wireplumber = parse_unit(
        "wireplumber.service",
        Path::new("/lib/wireplumber.service"),
        "[Unit]\nDescription=WirePlumber\nAfter=pipewire.service\nRequires=pipewire.service\n",
    );

    dag.insert(hal_audio);
    dag.insert(pipewire);
    dag.insert(wireplumber);

    let queue = dag.resolve_start_queue("wireplumber.service");
    assert_eq!(queue.len(), 3);

    let idx_hal = queue
        .iter()
        .position(|x| x == "android-hal-audio.service")
        .unwrap();
    let idx_pw = queue.iter().position(|x| x == "pipewire.service").unwrap();
    let idx_wp = queue
        .iter()
        .position(|x| x == "wireplumber.service")
        .unwrap();

    assert!(idx_hal < idx_pw);
    assert!(idx_pw < idx_wp);
}

#[test]
fn test_mpg_and_mmps_integration() {
    let temp_dir = std::env::temp_dir().join("utim_e2e_mpg_mmps");
    let _ = fs::remove_dir_all(&temp_dir);

    let power_dir = temp_dir.join("sys_power");
    let cgroup_dir = temp_dir.join("cgroup");
    let battery_dir = temp_dir.join("battery");

    fs::create_dir_all(&power_dir).unwrap();
    fs::create_dir_all(cgroup_dir.join("user.slice")).unwrap();
    fs::create_dir_all(&battery_dir).unwrap();

    fs::write(power_dir.join("wake_lock"), "").unwrap();
    fs::write(power_dir.join("wake_unlock"), "").unwrap();
    fs::write(power_dir.join("autosleep"), "off\n").unwrap();
    fs::write(cgroup_dir.join("user.slice").join("cgroup.freeze"), "0\n").unwrap();

    let mut mpg =
        MobilePowerGovernor::with_paths(power_dir.clone(), cgroup_dir.clone(), battery_dir);

    // Screen off event: freeze background applications
    mpg.set_cgroup_freeze("user.slice", true).unwrap();
    assert!(mpg.is_cgroup_frozen("user.slice").unwrap());

    // Music playback: PipeWire holds partial wakelock
    mpg.acquire_wake_lock("pipewire-playback").unwrap();
    assert!(mpg.has_active_wake_locks());
    assert_eq!(
        fs::read_to_string(power_dir.join("wake_lock")).unwrap(),
        "pipewire-playback"
    );

    // Music stopped: wakelock released -> allow deep autosleep
    mpg.release_wake_lock("pipewire-playback").unwrap();
    assert!(!mpg.has_active_wake_locks());
    mpg.configure_autosleep(true).unwrap();
    assert_eq!(
        fs::read_to_string(power_dir.join("autosleep")).unwrap(),
        "mem\n"
    );

    // Screen on event: unfreeze user apps in < 150us
    mpg.set_cgroup_freeze("user.slice", false).unwrap();
    assert!(!mpg.is_cgroup_frozen("user.slice").unwrap());

    // MMPS PSI evaluation
    let psi_file = temp_dir.join("proc_pressure_mem");
    fs::write(
        &psi_file,
        "some avg10=45.00 avg60=25.00 avg300=10.00 total=999\nfull avg10=28.00 avg60=15.00 avg300=5.00 total=888\n",
    ).unwrap();

    let mmps = MemorySupervisor::with_path(psi_file);
    assert_eq!(
        mmps.evaluate_pressure_level(),
        utim_core::mmps::MemoryPressureLevel::Critical
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_ipc_roundtrip_server_client() {
    let temp_sock = std::env::temp_dir().join(format!("utim_ipc_test_{}.sock", std::process::id()));
    let _ = fs::remove_file(&temp_sock);

    let listener = UnixListener::bind(&temp_sock).unwrap();
    listener.set_nonblocking(true).unwrap();

    // Client request in background thread
    let sock_clone = temp_sock.clone();
    let client_handle = std::thread::spawn(move || {
        let req = IpcRequest::Start("sshd.service".to_string());
        send_ipc_request(&sock_clone, &req)
    });

    // Server accept
    let mut stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("Accept error: {}", e),
        }
    };

    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();

    let req = IpcRequest::deserialize(&line).unwrap();
    assert_eq!(req, IpcRequest::Start("sshd.service".to_string()));

    let resp = IpcResponse::Ok("Started sshd.service".to_string());
    stream.write_all(resp.serialize().as_bytes()).unwrap();
    stream.flush().unwrap();

    let client_resp = client_handle.join().unwrap().unwrap();
    assert_eq!(
        client_resp,
        IpcResponse::Ok("Started sshd.service".to_string())
    );

    let _ = fs::remove_file(&temp_sock);
}

fn get_workspace_binary(name: &str) -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bin_path = manifest_dir.join("../../target/debug").join(name);
    if bin_path.exists() {
        return bin_path;
    }
    std::path::PathBuf::from(name)
}

#[test]
fn test_deb_systemd_helper_cli() {
    let temp_dir = std::env::temp_dir().join(format!("utim_test_dsh_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);

    let chroot = temp_dir.join("rootfs");
    let lib_systemd = chroot.join("usr/lib/systemd/system");
    fs::create_dir_all(&lib_systemd).unwrap();

    let unit_content = r#"
[Unit]
Description=Test Service

[Service]
ExecStart=/bin/true

[Install]
WantedBy=multi-user.target
"#;

    let unit_path = lib_systemd.join("test-helper.service");
    fs::write(&unit_path, unit_content).unwrap();

    let dsh_bin = get_workspace_binary("deb-systemd-helper");

    // 1. Enable unit with --root
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "enable",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute deb-systemd-helper");
    assert!(status.success(), "enable should succeed");

    // Verify symlink created in etc/systemd/system/multi-user.target.wants/
    let symlink = chroot.join("etc/systemd/system/multi-user.target.wants/test-helper.service");
    assert!(symlink.exists(), "wants symlink should exist");

    // Verify state file created in var/lib/systemd/deb-systemd-helper-enabled/
    let state_file =
        chroot.join("var/lib/systemd/deb-systemd-helper-enabled/test-helper.service.dsh-also");
    assert!(state_file.exists(), "dsh-also state file should exist");

    // 2. is-enabled returns 0 (enabled)
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "is-enabled",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute is-enabled");
    assert!(status.success(), "is-enabled should exit 0 when enabled");

    // 3. Disable unit
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "disable",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute disable");
    assert!(status.success(), "disable should succeed");
    assert!(!symlink.exists(), "wants symlink should be removed");
    assert!(!state_file.exists(), "state file should be removed");

    // 4. is-enabled returns 1 (disabled)
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "is-enabled",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute is-enabled on disabled unit");
    assert_eq!(
        status.code(),
        Some(1),
        "is-enabled should exit 1 when disabled"
    );

    // 5. Mask unit
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "mask",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute mask");
    assert!(status.success(), "mask should succeed");
    let mask_symlink = chroot.join("etc/systemd/system/test-helper.service");
    assert!(mask_symlink.is_symlink());
    assert_eq!(
        fs::read_link(&mask_symlink).unwrap(),
        Path::new("/dev/null")
    );

    // 6. Unmask unit
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "unmask",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute unmask");
    assert!(status.success(), "unmask should succeed");
    assert!(!mask_symlink.exists());

    // 7. Test --no-enable flag
    let status = std::process::Command::new(&dsh_bin)
        .args([
            "--root",
            chroot.to_str().unwrap(),
            "--no-enable",
            "enable",
            "test-helper.service",
        ])
        .status()
        .expect("Failed to execute enable with --no-enable");
    assert!(status.success(), "--no-enable enable should succeed");
    assert!(state_file.exists(), "state file should exist");
    assert!(
        !symlink.exists(),
        "wants symlink should NOT exist when --no-enable is set"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_deb_systemd_invoke_cli() {
    let dsi_bin = get_workspace_binary("deb-systemd-invoke");

    // 1. Offline / chroot check: when DEB_SYSTEMD_SYSTEM_DIR points to nonexistent directory,
    // deb-systemd-invoke must immediately exit with code 0 per Debian specification.
    let status = std::process::Command::new(&dsi_bin)
        .env("DEB_SYSTEMD_SYSTEM_DIR", "/nonexistent/test/systemd/dir")
        .args(["start", "ssh.service"])
        .status()
        .expect("Failed to execute deb-systemd-invoke");
    assert_eq!(
        status.code(),
        Some(0),
        "deb-systemd-invoke must exit 0 in offline chroot environment"
    );
}

#[test]
fn test_reboot_and_poweroff_ipc_handling() {
    let temp_sock =
        std::env::temp_dir().join(format!("utim_ipc_rb_test_{}.sock", std::process::id()));
    let _ = fs::remove_file(&temp_sock);

    let listener = UnixListener::bind(&temp_sock).unwrap();
    listener.set_nonblocking(true).unwrap();

    let sock_clone = temp_sock.clone();
    let client_handle = std::thread::spawn(move || {
        let resp_reboot = send_ipc_request(&sock_clone, &IpcRequest::Reboot);
        let resp_poweroff = send_ipc_request(&sock_clone, &IpcRequest::Poweroff);
        (resp_reboot, resp_poweroff)
    });

    // Server responds to Reboot
    let mut stream1 = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("Accept error: {}", e),
        }
    };
    let mut reader1 = BufReader::new(stream1.try_clone().unwrap());
    let mut line1 = String::new();
    reader1.read_line(&mut line1).unwrap();
    assert_eq!(IpcRequest::deserialize(&line1).unwrap(), IpcRequest::Reboot);
    stream1
        .write_all(
            IpcResponse::Ok("Rebooting system...".to_string())
                .serialize()
                .as_bytes(),
        )
        .unwrap();
    stream1.flush().unwrap();

    // Server responds to Poweroff
    let mut stream2 = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("Accept error: {}", e),
        }
    };
    let mut reader2 = BufReader::new(stream2.try_clone().unwrap());
    let mut line2 = String::new();
    reader2.read_line(&mut line2).unwrap();
    assert_eq!(
        IpcRequest::deserialize(&line2).unwrap(),
        IpcRequest::Poweroff
    );
    stream2
        .write_all(
            IpcResponse::Ok("Powering off system...".to_string())
                .serialize()
                .as_bytes(),
        )
        .unwrap();
    stream2.flush().unwrap();

    let (resp_reboot, resp_poweroff) = client_handle.join().unwrap();
    assert_eq!(
        resp_reboot.unwrap(),
        IpcResponse::Ok("Rebooting system...".to_string())
    );
    assert_eq!(
        resp_poweroff.unwrap(),
        IpcResponse::Ok("Powering off system...".to_string())
    );

    let _ = fs::remove_file(&temp_sock);
}

#[test]
fn test_dag_ready_to_spawn_state_isolation() {
    use utim_core::dag::UnitState;

    let mut dag = UnitDag::new();
    let unit1 = parse_unit(
        "service1.service",
        Path::new("/service1.service"),
        "[Unit]\nDescription=S1\n",
    );
    let unit2 = parse_unit(
        "service2.service",
        Path::new("/service2.service"),
        "[Unit]\nDescription=S2\n",
    );
    dag.insert(unit1);
    dag.insert(unit2);

    let queue = vec![
        "service1.service".to_string(),
        "service2.service".to_string(),
    ];

    // Initially both are Inactive, so ready_to_spawn returns both
    let ready = dag.ready_to_spawn(&queue);
    assert_eq!(ready, vec!["service1.service", "service2.service"]);

    // When service1 transitions to Activating, it must NOT be returned by ready_to_spawn again
    dag.set_state("service1.service", UnitState::Activating);
    let ready_after_activating = dag.ready_to_spawn(&queue);
    assert_eq!(ready_after_activating, vec!["service2.service"]);

    // When service1 becomes Active, it still must NOT be returned
    dag.set_state("service1.service", UnitState::Active);
    let ready_after_active = dag.ready_to_spawn(&queue);
    assert_eq!(ready_after_active, vec!["service2.service"]);
}
