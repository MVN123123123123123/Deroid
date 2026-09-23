//! Integration Test Suite for Phase 4 Milestone 4.1:
//! Real-Time PipeWire Audio & Android Audio HAL Integration (spa-droid).
//! Exhaustively validates:
//! - Android Audio HAL (HIDL 7.1 / AIDL v1/v3) configuration
//! - Low-latency duplex processing (< 15ms roundtrip latency)
//! - Dynamic audio routing (Speaker, Earpiece, 3.5mm Headset, Bluetooth A2DP / SCO)
//! - Hardware Acoustic Echo Cancellation (AEC) and Noise Suppression (NS) in-call
//! - PipeWire SPA Droid node lifecycle (Playback and Capture)
//! - UTIM Mobile Power Governor (MPG) screen-off music playback wakelock synchronization
//! - Self-healing crash recovery from Audio HAL termination

use utim_core::audio::{
    AndroidAudioHal, AudioBringupStatus, AudioChannelMask, AudioConfig, AudioFormat,
    AudioHalVersion, AudioInputDevice, AudioMode, AudioOutputDevice, AudioPowerManager,
    AudioRouter, AudioStreamType, SpaDirection, SpaDroidNode, SpaNodeState, AUDIO_WAKELOCK_NAME,
};
use utim_core::mpg::MobilePowerGovernor;

#[test]
fn test_milestone_4_1_hal_versions_and_detection() {
    let hidl = AndroidAudioHal::new(AudioHalVersion::HIDL_7_1);
    assert!(!hidl.version.is_aidl());
    assert_eq!(hidl.version.binder_device(), "/dev/hwbinder");
    assert_eq!(
        hidl.version.service_name(),
        "android.hardware.audio@7.0::IDevicesFactory"
    );

    let aidl = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    assert!(aidl.version.is_aidl());
    assert_eq!(aidl.version.binder_device(), "/dev/binder");
    assert_eq!(
        aidl.version.service_name(),
        "android.hardware.audio.core.IModule/default"
    );
}

#[test]
fn test_milestone_4_1_stream_creation_and_low_latency_buffer() {
    let mut hal = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    let config = AudioConfig::low_latency_output();

    // 48kHz, Stereo, 16-bit PCM (2 channels * 2 bytes = 4 bytes per frame)
    assert_eq!(config.sample_rate, 48000);
    assert_eq!(config.channel_mask, AudioChannelMask::Stereo);
    assert_eq!(config.format, AudioFormat::Pcm16Bit);
    assert_eq!(config.frame_size(), 4);
    assert_eq!(config.period_frames, 192);
    assert_eq!(config.buffer_size_bytes(), 768);

    // Buffer processing latency = 192 / 48000 = 4.0 ms (well below 15ms threshold)
    assert!((config.latency_ms() - 4.0).abs() < 0.01);

    let stream_id = hal
        .open_output_stream(
            AudioStreamType::Music,
            config,
            vec![AudioOutputDevice::Speaker],
        )
        .expect("Failed to open output stream");

    let pcm_frame_chunk = [0u8; 768];
    let written = hal
        .write_output(stream_id, &pcm_frame_chunk)
        .expect("Failed to write output buffer");
    assert_eq!(written, 768);

    let stream = hal.streams.iter().find(|s| s.id == stream_id).unwrap();
    assert_eq!(stream.frames_processed, 192);

    hal.close_stream(stream_id).expect("Close stream failed");
    assert!(hal.streams.is_empty());
}

#[test]
fn test_milestone_4_1_dynamic_audio_routing() {
    let mut hal = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    let mut router = AudioRouter::new();

    // 1. Initial default state: Speaker output, Built-in Mic input
    router.evaluate_and_apply_routing(&mut hal);
    assert_eq!(router.state.current_output, AudioOutputDevice::Speaker);
    assert_eq!(router.state.current_input, AudioInputDevice::BuiltinMic);
    assert_eq!(hal.get_parameter("routing"), Some("speaker"));
    assert!(!hal.aec_enabled);
    assert!(!hal.ns_enabled);

    // 2. User plugs in 3.5mm wired headset with microphone
    router.on_wired_headset_event(true, true, &mut hal);
    assert_eq!(router.state.current_output, AudioOutputDevice::WiredHeadset);
    assert_eq!(router.state.current_input, AudioInputDevice::WiredHeadset);
    assert_eq!(hal.get_parameter("routing"), Some("headset"));

    // 3. User unplugs wired headset -> reverts to speaker
    router.on_wired_headset_event(false, false, &mut hal);
    assert_eq!(router.state.current_output, AudioOutputDevice::Speaker);

    // 4. Bluetooth A2DP audio connects
    router.on_bluetooth_a2dp_event(true, &mut hal);
    assert_eq!(router.state.current_output, AudioOutputDevice::BluetoothA2dp);
    assert_eq!(hal.get_parameter("routing"), Some("bt_a2dp"));

    // 5. Phone call begins: audio mode switches to IN_CALL, AEC and NS activate in DSP
    router.on_call_state_changed(true, &mut hal);
    assert_eq!(hal.mode, AudioMode::InCall);
    assert!(hal.aec_enabled, "AEC must be enabled during voice call");
    assert!(hal.ns_enabled, "NS must be enabled during voice call");
    // Without headset or BT SCO, in-call audio routes to Earpiece
    assert_eq!(router.state.current_output, AudioOutputDevice::Earpiece);
    assert_eq!(hal.get_parameter("routing"), Some("earpiece"));

    // 6. User toggles speakerphone during call
    router.set_speakerphone(true, &mut hal);
    assert_eq!(router.state.current_output, AudioOutputDevice::Speaker);
    assert_eq!(hal.get_parameter("routing"), Some("speaker"));

    // 7. Phone call terminates -> mode returns to Normal, AEC/NS disable, routes back to BT A2DP
    router.on_call_state_changed(false, &mut hal);
    assert_eq!(hal.mode, AudioMode::Normal);
    assert!(!hal.aec_enabled);
    assert!(!hal.ns_enabled);
    assert_eq!(router.state.current_output, AudioOutputDevice::BluetoothA2dp);
}

#[test]
fn test_milestone_4_1_spa_droid_node_lifecycle() {
    let mut hal = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    let config = AudioConfig::low_latency_output();

    let mut playback_node = SpaDroidNode::new_playback(42, "alsa-playback.music", config);
    assert_eq!(playback_node.direction, SpaDirection::Playback);
    assert_eq!(playback_node.state, SpaNodeState::Suspended);

    // Activate node: opens stream in HAL
    playback_node
        .activate(&mut hal)
        .expect("Failed to activate playback node");
    assert_eq!(playback_node.state, SpaNodeState::Idle);
    assert!(playback_node.hal_stream_id.is_some());

    // Process playback buffer
    let pcm_buffer = [0u8; 768];
    let written = playback_node
        .process_output_buffer(&pcm_buffer, &mut hal)
        .expect("Playback process failed");
    assert_eq!(written, 768);
    assert_eq!(playback_node.state, SpaNodeState::Running);
    assert_eq!(playback_node.frames_processed, 192);
    assert!(playback_node.last_latency_ms <= 15.0);

    // Pause node
    playback_node
        .pause(&mut hal)
        .expect("Pause playback failed");
    assert_eq!(playback_node.state, SpaNodeState::Idle);

    // Destroy node: cleans up HAL stream
    playback_node
        .destroy(&mut hal)
        .expect("Destroy playback node failed");
    assert_eq!(playback_node.state, SpaNodeState::Inactive);
    assert!(hal.streams.is_empty());
}

#[test]
fn test_milestone_4_1_mpg_power_synchronization() {
    let mut hal = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    let mut mpg = MobilePowerGovernor::new();
    let mut pwr_mgr = AudioPowerManager::new();

    let config = AudioConfig::standard_mobile_output();
    let mut node = SpaDroidNode::new_playback(1, "spa-music-stream", config);
    node.activate(&mut hal).unwrap();

    let mut nodes = vec![node];

    // Node is Idle -> wakelock not held
    pwr_mgr.sync_power_state(&nodes, &mut mpg);
    assert!(!pwr_mgr.wakelock_held);

    // Start playback: node is Running -> wakelock acquired for screen-off playback
    nodes[0].start();
    pwr_mgr.sync_power_state(&nodes, &mut mpg);
    assert!(
        pwr_mgr.wakelock_held,
        "Wakelock must be held during active playback"
    );
    assert!(mpg.is_wake_lock_active(AUDIO_WAKELOCK_NAME));

    // Pause playback -> wakelock released
    nodes[0].pause(&mut hal).unwrap();
    pwr_mgr.sync_power_state(&nodes, &mut mpg);
    assert!(
        !pwr_mgr.wakelock_held,
        "Wakelock must be released when playback stops"
    );
    assert!(!mpg.is_wake_lock_active(AUDIO_WAKELOCK_NAME));
}

#[test]
fn test_milestone_4_1_self_healing_watchdog() {
    let mut hal = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    let mut router = AudioRouter::new();
    let mut pwr_mgr = AudioPowerManager::new();

    let config = AudioConfig::low_latency_output();
    let mut node = SpaDroidNode::new_playback(10, "music-stream", config);
    node.activate(&mut hal).unwrap();
    node.start();

    let mut nodes = vec![node];
    assert_eq!(hal.streams.len(), 1);

    // Simulate Audio HAL crash / audioserver SIGSEGV
    pwr_mgr.handle_hal_crash(&mut hal, &mut router, &mut nodes);

    assert_eq!(pwr_mgr.hal_restarts_detected, 1);
    assert_eq!(pwr_mgr.auto_recovery_count, 1);
    // Verified: streams restored and state remains Running without needing device reboot
    assert_eq!(hal.streams.len(), 1);
    assert_eq!(nodes[0].state, SpaNodeState::Running);

    let status = AudioBringupStatus {
        hal_version: hal.version,
        routing_state: router.state.clone(),
        active_nodes: nodes.len(),
        roundtrip_latency_ms: nodes[0].last_latency_ms,
        mpg_wakelock_active: pwr_mgr.wakelock_held,
        aec_active: hal.aec_enabled,
        ns_active: hal.ns_enabled,
    };
    assert!(status.is_ready());
}

#[test]
fn test_milestone_4_1_hal_crash_during_active_call_restores_in_call_and_aec() {
    let mut hal = AndroidAudioHal::new(AudioHalVersion::AIDL_V1);
    let mut router = AudioRouter::new();
    let mut pwr_mgr = AudioPowerManager::new();

    // 1. Establish an active call routing to Bluetooth SCO
    router.on_bluetooth_sco_event(true, &mut hal);
    router.on_call_state_changed(true, &mut hal);
    assert_eq!(hal.mode, AudioMode::InCall);
    assert!(hal.aec_enabled);
    assert!(hal.ns_enabled);
    assert_eq!(router.state.current_output, AudioOutputDevice::BluetoothSco);

    // 2. Open in-call voice stream
    let config = AudioConfig::voice_call_config();
    let mut voice_node = SpaDroidNode::new_playback(5, "spa-incall-voice", config);
    voice_node.activate(&mut hal).unwrap();
    voice_node.start();
    let mut nodes = vec![voice_node];

    // 3. User disconnects BT SCO, switching route to earpiece simultaneously with HAL crash
    router.on_bluetooth_sco_event(false, &mut hal);
    pwr_mgr.handle_hal_crash(&mut hal, &mut router, &mut nodes);

    // 4. Verify that HAL mode was preserved as InCall and DSP AEC/NS remain enabled
    assert_eq!(hal.mode, AudioMode::InCall, "HAL mode must remain InCall after crash recovery");
    assert!(hal.aec_enabled, "DSP AEC must remain enabled after crash recovery during call");
    assert!(hal.ns_enabled, "DSP NS must remain enabled after crash recovery during call");
    assert_eq!(router.state.current_output, AudioOutputDevice::Earpiece);
    assert_eq!(hal.get_parameter("routing"), Some("earpiece"));
    assert_eq!(nodes[0].state, SpaNodeState::Running);
}

#[test]
fn test_milestone_4_1_default_implementations() {
    let hal = AndroidAudioHal::default();
    assert!(hal.version.is_aidl());
    let router = AudioRouter::default();
    assert_eq!(router.state.current_output, AudioOutputDevice::Speaker);
    let pwr = AudioPowerManager::default();
    assert!(!pwr.wakelock_held);
}
