//! Integration Test Suite for Phase 4 Milestone 4.2:
//! Telephony, ModemManager RIL Bridge & Mobile Data Integration.
//! Exhaustively validates:
//! - Android Radio Interface Layer (RIL) request/response framing & unsolicited events
//! - Voice call state machine (Dialing, Alerting, Active, Terminated)
//! - Carrier VoLTE / VoNR IMS registration status
//! - SMS PDU 7-bit GSM packing, semi-octet address formatting, and multi-part concatenation reassembly
//! - 4G LTE / 5G NR mobile data setup and cellular interface provisioning (rmnet_data0)
//! - Dual-SIM slot management (SIM 1 / SIM 2)
//! - Protected telephony slice (oom_score_adj = -800)
//! - Wake from deep suspend on incoming phone call or SMS

use utim_core::mpg::MobilePowerGovernor;
use utim_core::telephony::{
    decode_address_semi_octets, decode_gsm7, encode_address_semi_octets, encode_gsm7,
    encode_sms_deliver_pdu, encode_sms_submit_pdu, pack_7bit, parse_sms_deliver_pdu, unpack_7bit,
    CallState, DataCallState, ModemManagerBridge, RilClient, RilPacket, SmsEncoding, SmsMessage,
    SmsReassembler, TelephonyBringupStatus, TelephonyWakeManager, VoiceCallManager, WakeReason,
    RIL_E_SUCCESS,
    RIL_REQUEST_SETUP_DATA_CALL, RIL_UNSOL_RESPONSE_CALL_STATE_CHANGED,
    TELEPHONY_CGROUP_PATH, TELEPHONY_OOM_SCORE_ADJ, TELEPHONY_WAKE_LOCK,
};

#[test]
fn test_milestone_4_2_ril_packet_serialization_and_unsol() {
    let mut ril = RilClient::new(0);
    assert_eq!(ril.socket_path, "/dev/socket/rild");

    // 1. Serialize a RIL request
    let payload = b"AT+CGDCONT=1,\"IP\",\"internet\"\0";
    let (serial, wire_bytes) = ril.create_request(RIL_REQUEST_SETUP_DATA_CALL, payload);
    assert_eq!(serial, 1);
    assert!(wire_bytes.len() >= 12);

    // Verify big-endian length prefix
    let total_len = u32::from_be_bytes([wire_bytes[0], wire_bytes[1], wire_bytes[2], wire_bytes[3]]);
    assert_eq!(total_len as usize, wire_bytes.len() - 4);

    // 2. Standard AOSP RILD Unsolicited Packet (RESPONSE_UNSOLICITED = 1)
    let unsol_wire = RilPacket::serialize_unsolicited(RIL_UNSOL_RESPONSE_CALL_STATE_CHANGED, &[0x42]);
    let (parsed_unsol, read_len) = RilPacket::parse(&unsol_wire).unwrap().unwrap();
    assert_eq!(read_len, unsol_wire.len());
    match parsed_unsol {
        RilPacket::Unsolicited { unsol_id, payload } => {
            assert_eq!(unsol_id, RIL_UNSOL_RESPONSE_CALL_STATE_CHANGED);
            assert_eq!(payload, vec![0x42]);
        }
        _ => panic!("Expected Unsolicited packet"),
    }

    // 3. Standard AOSP RILD Solicited Response Packet (RESPONSE_SOLICITED = 0)
    let resp_wire = RilPacket::serialize_solicited_response(serial, RIL_E_SUCCESS, b"OK\0");
    let (parsed_resp, resp_len) = RilPacket::parse(&resp_wire).unwrap().unwrap();
    assert_eq!(resp_len, resp_wire.len());
    match parsed_resp {
        RilPacket::Response { serial: s, error_code, payload } => {
            assert_eq!(s, serial);
            assert_eq!(error_code, RIL_E_SUCCESS);
            assert_eq!(payload, b"OK\0");
        }
        _ => panic!("Expected Solicited Response packet"),
    }

    // 4. Response handling
    assert!(ril.handle_response(serial).is_some());
    assert!(ril.pending_requests.is_empty());
}

#[test]
fn test_milestone_4_2_voice_call_lifecycle_and_volte() {
    let mut ril = RilClient::new(0);
    let mut voice = VoiceCallManager::new();

    // Verify VoLTE and VoNR out of the box
    assert!(voice.ims_status.registered);
    assert!(voice.ims_status.voice_over_lte_supported);
    assert!(voice.ims_status.voice_over_nr_supported);

    // Emergency number identification
    assert!(VoiceCallManager::is_emergency_number("911"));
    assert!(VoiceCallManager::is_emergency_number("112"));
    assert!(!VoiceCallManager::is_emergency_number("+15551234567"));

    // 1. Dial outgoing call
    let (call_id, packet) = voice.dial("+15559876543", &mut ril);
    assert!(!packet.is_empty());
    assert_eq!(voice.calls.len(), 1);
    assert_eq!(voice.calls[0].state, CallState::Dialing);
    assert_eq!(voice.calls[0].number, "+15559876543");
    assert_eq!(voice.calls[0].index, 1);

    // 2. Alerting / Ringing
    voice.on_remote_alerting(call_id);
    assert_eq!(voice.calls[0].state, CallState::Alerting);

    // 3. Call Connected / Active
    voice.on_call_connected(call_id);
    assert_eq!(voice.calls[0].state, CallState::Active);
    assert!(voice.has_active_call());

    // 4. Incoming second call (Call Waiting)
    let call_id2 = voice.on_incoming_call("+15550002222");
    assert_eq!(voice.calls.len(), 2);
    assert_eq!(voice.calls[1].index, 2);

    // 5. Hang up Call 1 -> RIL packet must specify call index 1
    let hangup_pkt1 = voice.hangup(call_id, &mut ril).unwrap();
    assert!(!hangup_pkt1.is_empty());
    // Hangup payload is call index (4 bytes LE) at body offset 8
    let _body_len = u32::from_be_bytes([hangup_pkt1[0], hangup_pkt1[1], hangup_pkt1[2], hangup_pkt1[3]]) as usize;
    let index1 = u32::from_le_bytes([hangup_pkt1[12], hangup_pkt1[13], hangup_pkt1[14], hangup_pkt1[15]]);
    assert_eq!(index1, 1);

    // 6. Now hang up Call 2 -> RIL packet must preserve call index 2, NOT 1!
    let hangup_pkt2 = voice.hangup(call_id2, &mut ril).unwrap();
    assert!(!hangup_pkt2.is_empty());
    let index2 = u32::from_le_bytes([hangup_pkt2[12], hangup_pkt2[13], hangup_pkt2[14], hangup_pkt2[15]]);
    assert_eq!(index2, 2, "Call index 2 must be preserved when hanging up remaining call");
    assert!(!voice.has_active_call());
}

#[test]
fn test_milestone_4_2_sms_pdu_encoding_and_concatenation() {
    // 1. 7-bit packing & unpacking
    let text = b"Hello Treble";
    let packed = pack_7bit(text);
    let unpacked = unpack_7bit(&packed, text.len());
    assert_eq!(unpacked, text);

    // 2. 3GPP TS 23.038 GSM 7-bit character set & extensions
    let test_gsm = "Hello Treble @ $ _ ^ { } €";
    let encoded_septets = encode_gsm7(test_gsm).expect("GSM 7-bit encoding failed");
    let decoded_gsm = decode_gsm7(&encoded_septets);
    assert_eq!(decoded_gsm, test_gsm);

    // 3. Semi-octet phone number formatting
    let number = "+1234567890";
    let (toa, bcd) = encode_address_semi_octets(number);
    assert_eq!(toa, 0x91); // International
    let decoded_num = decode_address_semi_octets(&bcd, 10, toa);
    assert_eq!(decoded_num, number);

    // 3b. Alphanumeric address decoding (Type of Address: 0xD0 or 0x50)
    let alpha_text = "ALPHANUM";
    let alpha_septets = encode_gsm7(alpha_text).unwrap();
    let alpha_packed = pack_7bit(&alpha_septets);
    let decoded_alpha = decode_address_semi_octets(&alpha_packed, 14, 0xD0);
    assert_eq!(decoded_alpha, alpha_text);

    // 4. SMS-SUBMIT PDU encoding
    let pdu_submit = encode_sms_submit_pdu("+1234567890", "Test message from UTIM GSI: €100");
    assert!(!pdu_submit.is_empty());

    // 5. SMS-DELIVER PDU single-part roundtrip
    let orig_sender = "+15551234567";
    let orig_text = "Standard GSM delivery test: @world";
    let deliver_pdu = encode_sms_deliver_pdu(orig_sender, orig_text, None);
    let parsed_msg = parse_sms_deliver_pdu(&deliver_pdu).expect("parse_sms_deliver_pdu failed");
    assert_eq!(parsed_msg.sender, orig_sender);
    assert_eq!(parsed_msg.body, orig_text);
    assert_eq!(parsed_msg.encoding, SmsEncoding::Gsm7Bit);
    assert_eq!(parsed_msg.concat_ref, None);

    // 6. Multipart SMS with UDH Concatenation Roundtrip & Reassembly
    let mut reassembler = SmsReassembler::new();
    let pdu_part1 = encode_sms_deliver_pdu(orig_sender, "First segment. ", Some((0x42, 2, 1)));
    let pdu_part2 = encode_sms_deliver_pdu(orig_sender, "Second segment.", Some((0x42, 2, 2)));

    let msg1 = parse_sms_deliver_pdu(&pdu_part1).unwrap();
    let msg2 = parse_sms_deliver_pdu(&pdu_part2).unwrap();

    assert_eq!(msg1.concat_ref, Some(0x42));
    assert_eq!(msg1.concat_total, 2);
    assert_eq!(msg1.concat_seq, 1);
    assert_eq!(msg1.body, "First segment. ");

    assert_eq!(msg2.concat_ref, Some(0x42));
    assert_eq!(msg2.concat_total, 2);
    assert_eq!(msg2.concat_seq, 2);
    assert_eq!(msg2.body, "Second segment.");

    // Retransmission deduplication check: simulate network re-sending part 1
    assert!(reassembler.add_segment(msg1.clone()).is_none());
    assert!(reassembler.add_segment(msg1.clone()).is_none(), "Duplicate segment must not complete early");

    // Add part 2: completes and yields exact combined body
    let full = reassembler.add_segment(msg2).expect("Reassembly failed");
    assert_eq!(full.body, "First segment. Second segment.");

    // 7. Interleaved multipart SMS from different senders with same reference ID (0x42)
    let mut reassembler2 = SmsReassembler::new();
    let alice_p1 = SmsMessage {
        sender: "+1111111111".to_string(),
        recipient: String::new(),
        body: "Alice Part 1; ".to_string(),
        encoding: SmsEncoding::Gsm7Bit,
        concat_ref: Some(0x42),
        concat_total: 2,
        concat_seq: 1,
    };
    let bob_p1 = SmsMessage {
        sender: "+2222222222".to_string(),
        recipient: String::new(),
        body: "Bob Part 1; ".to_string(),
        encoding: SmsEncoding::Gsm7Bit,
        concat_ref: Some(0x42),
        concat_total: 2,
        concat_seq: 1,
    };
    let alice_p2 = SmsMessage {
        sender: "+1111111111".to_string(),
        recipient: String::new(),
        body: "Alice Part 2".to_string(),
        encoding: SmsEncoding::Gsm7Bit,
        concat_ref: Some(0x42),
        concat_total: 2,
        concat_seq: 2,
    };
    let bob_p2 = SmsMessage {
        sender: "+2222222222".to_string(),
        recipient: String::new(),
        body: "Bob Part 2".to_string(),
        encoding: SmsEncoding::Gsm7Bit,
        concat_ref: Some(0x42),
        concat_total: 2,
        concat_seq: 2,
    };

    assert!(reassembler2.add_segment(alice_p1).is_none());
    assert!(reassembler2.add_segment(bob_p1).is_none());

    let alice_full = reassembler2.add_segment(alice_p2).expect("Alice reassembly failed");
    assert_eq!(alice_full.sender, "+1111111111");
    assert_eq!(alice_full.body, "Alice Part 1; Alice Part 2");

    let bob_full = reassembler2.add_segment(bob_p2).expect("Bob reassembly failed");
    assert_eq!(bob_full.sender, "+2222222222");
    assert_eq!(bob_full.body, "Bob Part 1; Bob Part 2");
}

#[test]
fn test_milestone_4_2_mobile_data_setup_and_network_interface() {
    let mut ril = RilClient::new(0);
    let mut bridge = ModemManagerBridge::new_dual_sim();
    let slot = bridge.get_slot(0).unwrap();

    // 1. Initial state: Disconnected
    assert!(!slot.data.is_data_active());

    // 2. Trigger data call setup
    let (cid, req_packet) = slot.data.setup_data_call(&mut ril);
    assert_eq!(cid, 1);
    assert!(!req_packet.is_empty());
    assert_eq!(
        slot.data.active_session.as_ref().unwrap().state,
        DataCallState::Connecting
    );

    // 3. Simulated RIL data call response with cellular network configuration
    slot.data.on_data_call_connected(
        "10.142.68.21",
        "10.142.68.1",
        &["8.8.8.8", "8.8.4.4"],
        1500,
    );

    assert!(slot.data.is_data_active());
    let session = slot.data.active_session.as_ref().unwrap();
    assert_eq!(session.ifname, "rmnet_data0");
    assert_eq!(session.ipv4_addr, "10.142.68.21");
    assert_eq!(session.ipv4_gateway, "10.142.68.1");
    assert_eq!(session.mtu, 1500);

    // 4. Deactivate data call
    let deact_packet = slot.data.deactivate_data_call(&mut ril).unwrap();
    assert!(!deact_packet.is_empty());
    slot.data.on_data_call_disconnected();
    assert!(!slot.data.is_data_active());
}

#[test]
fn test_milestone_4_2_dual_sim_and_telephony_slice_protection() {
    let mut bridge = ModemManagerBridge::new_dual_sim();

    // Dual-SIM slots verified
    assert_eq!(bridge.slots.len(), 2);
    assert_eq!(bridge.slots[0].slot_id, 0);
    assert_eq!(bridge.slots[1].slot_id, 1);
    assert_eq!(bridge.primary_data_slot, 0);

    // Switch primary data SIM to slot 1 (SIM 2)
    bridge
        .set_primary_data_slot(1)
        .expect("Switch primary data SIM failed");
    assert_eq!(bridge.primary_data_slot, 1);

    // Check signal strength calculation
    let slot0 = bridge.get_slot(0).unwrap();
    slot0.set_signal_strength(-65); // Strong signal
    assert!(slot0.signal_percent > 70);
    assert!(slot0.signal_bars >= 4);

    // Verify protected slice & OOM score adj (-800)
    assert_eq!(bridge.oom_score_adj, TELEPHONY_OOM_SCORE_ADJ);
    assert_eq!(bridge.oom_score_adj, -800);
    assert_eq!(bridge.protected_slice, TELEPHONY_CGROUP_PATH);
}

#[test]
fn test_milestone_4_2_wake_from_deep_suspend() {
    let mut mpg = MobilePowerGovernor::new();
    let mut wake_mgr = TelephonyWakeManager::new();

    assert!(!wake_mgr.wake_lock_active);
    assert!(!mpg.is_wake_lock_active(TELEPHONY_WAKE_LOCK));

    // Incoming phone call event arrives while phone is suspended
    wake_mgr.on_modem_irq_event(WakeReason::IncomingCall, &mut mpg);

    assert_eq!(wake_mgr.wake_events_count, 1);
    assert_eq!(wake_mgr.last_wake_reason, Some(WakeReason::IncomingCall));
    assert!(
        wake_mgr.wake_lock_active,
        "Wake lock must be held to prevent immediate suspend re-entry"
    );
    assert!(mpg.is_wake_lock_active(TELEPHONY_WAKE_LOCK));

    // Release alert lock after user interacts
    wake_mgr.release_alert_lock(&mut mpg);
    assert!(!wake_mgr.wake_lock_active);
    assert!(!mpg.is_wake_lock_active(TELEPHONY_WAKE_LOCK));

    let status = TelephonyBringupStatus {
        dual_sim_supported: true,
        primary_data_slot: 0,
        volte_registered: true,
        vonr_supported: true,
        mobile_data_active: true,
        oom_score_adj_compliant: true,
    };
    assert!(status.is_ready());
}

#[test]
fn test_milestone_4_2_sms_pdu_truncated_and_edge_cases() {
    // 1. Empty PDU
    assert!(parse_sms_deliver_pdu(&[]).is_err());

    // 2. Truncated SMSC length
    assert!(parse_sms_deliver_pdu(&[0x05, 0x00, 0x01]).is_err());

    // 3. Truncated before/at sender length
    assert!(parse_sms_deliver_pdu(&[0x00]).is_err());
    assert!(parse_sms_deliver_pdu(&[0x00, 0x04]).is_err());
    assert!(parse_sms_deliver_pdu(&[0x00, 0x04, 0x04]).is_err());

    // 4. Truncated sender address bytes
    assert!(parse_sms_deliver_pdu(&[0x00, 0x04, 0x04, 0x91, 0x21]).is_err());

    // 5. Truncated PID / DCS
    assert!(parse_sms_deliver_pdu(&[0x00, 0x04, 0x02, 0x91, 0x21]).is_err());
    assert!(parse_sms_deliver_pdu(&[0x00, 0x04, 0x02, 0x91, 0x21, 0x00]).is_err());

    // 6. Truncated timestamp (SCTS) / UDL
    assert!(parse_sms_deliver_pdu(&[0x00, 0x04, 0x02, 0x91, 0x21, 0x00, 0x00, 0x01, 0x02]).is_err());

    // 7. Truncated UDH header
    let truncated_udh_pdu = [
        0x00, // SMSC len = 0
        0x44, // SMS-DELIVER with TP-UDHI
        0x02, 0x91, 0x21, // Sender
        0x00, // PID
        0x00, // DCS GSM7
        0x62, 0x90, 0x22, 0x12, 0x00, 0x00, 0x00, // SCTS
        0x10, // UDL = 16
        0x0A, // UDH len = 10, but user data ends immediately
    ];
    assert!(parse_sms_deliver_pdu(&truncated_udh_pdu).is_err());
}

#[test]
fn test_milestone_4_2_call_index_allocation_and_collision_prevention() {
    let mut ril = RilClient::new(0);
    let mut voice = VoiceCallManager::new();

    // Call 1 dialed -> index 1
    let (c1, _) = voice.dial("+111", &mut ril);
    assert_eq!(voice.calls[0].index, 1);

    // Call 2 incoming -> index 2
    let c2 = voice.on_incoming_call("+222");
    assert_eq!(voice.calls[1].index, 2);

    // Hang up Call 1 -> remaining call is Call 2 (index 2)
    let _ = voice.hangup(c1, &mut ril);
    assert_eq!(voice.calls.len(), 1);
    assert_eq!(voice.calls[0].call_id, c2);
    assert_eq!(voice.calls[0].index, 2);

    // Now dial Call 3: must be assigned index 1, NOT collide with index 2!
    let (c3, _) = voice.dial("+333", &mut ril);
    assert_eq!(voice.calls.len(), 2);
    let call3 = voice.calls.iter().find(|c| c.call_id == c3).unwrap();
    assert_eq!(call3.index, 1, "Call 3 must take lowest available index (1) avoiding collision with 2");

    // Cleanly hang up both remaining calls
    let pkt2 = voice.hangup(c2, &mut ril).unwrap();
    let idx2 = u32::from_le_bytes([pkt2[12], pkt2[13], pkt2[14], pkt2[15]]);
    assert_eq!(idx2, 2);

    let pkt3 = voice.hangup(c3, &mut ril).unwrap();
    let idx3 = u32::from_le_bytes([pkt3[12], pkt3[13], pkt3[14], pkt3[15]]);
    assert_eq!(idx3, 1);

    assert!(voice.calls.is_empty());
}

#[test]
fn test_milestone_4_2_ril_truncated_solicited_response() {
    // RESPONSE_SOLICITED (0) with only 8 bytes body (missing error_code)
    let mut wire = Vec::new();
    wire.extend_from_slice(&8u32.to_be_bytes()); // body len = 8
    wire.extend_from_slice(&0u32.to_le_bytes()); // RESPONSE_SOLICITED
    wire.extend_from_slice(&42u32.to_le_bytes()); // serial
    // body len is 8 (< 12) -> must return error
    let res = RilPacket::parse(&wire);
    assert!(res.is_err());
}

#[test]
fn test_milestone_4_2_default_implementations() {
    let bridge = ModemManagerBridge::default();
    assert_eq!(bridge.slots.len(), 2);
    assert_eq!(bridge.primary_data_slot, 0);
}
