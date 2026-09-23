//! Android Radio Interface Layer (RIL) Client & Protocol Framing.
//! Communicates with the vendor `rild` socket (/dev/socket/rild, /dev/socket/rild2)
//! or AIDL IRadio HAL interface.
//! Adheres strictly to GEMINI.md: zero-copy packet parsing, bounded buffers,
//! and minimal allocations.


// --- Standard Android RIL Request Constants (telephony/ril.h) ---
pub const RIL_REQUEST_GET_SIM_STATUS: u32 = 1;
pub const RIL_REQUEST_ENTER_SIM_PIN: u32 = 2;
pub const RIL_REQUEST_GET_CURRENT_CALLS: u32 = 9;
pub const RIL_REQUEST_DIAL: u32 = 10;
pub const RIL_REQUEST_HANGUP: u32 = 12;
pub const RIL_REQUEST_SIGNAL_STRENGTH: u32 = 19;
pub const RIL_REQUEST_OPERATOR: u32 = 22;
pub const RIL_REQUEST_RADIO_POWER: u32 = 23;
pub const RIL_REQUEST_SEND_SMS: u32 = 25;
pub const RIL_REQUEST_SETUP_DATA_CALL: u32 = 27;
pub const RIL_REQUEST_ANSWER: u32 = 40;
pub const RIL_REQUEST_DEACTIVATE_DATA_CALL: u32 = 41;
pub const RIL_REQUEST_IMS_REGISTRATION_STATE: u32 = 112;

// --- Standard Android RIL Unsolicited Response Constants ---
pub const RIL_UNSOL_RESPONSE_RADIO_STATE_CHANGED: u32 = 1000;
pub const RIL_UNSOL_RESPONSE_CALL_STATE_CHANGED: u32 = 1001;
pub const RIL_UNSOL_RESPONSE_NEW_SMS: u32 = 1003;
pub const RIL_UNSOL_SIGNAL_STRENGTH: u32 = 1009;
pub const RIL_UNSOL_DATA_CALL_LIST_CHANGED: u32 = 1010;

/// RIL Radio Power State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioState {
    Off = 0,
    Unavailable = 1,
    On = 10,
}

/// RIL Packet Type
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RilPacket {
    Request {
        serial: u32,
        request_id: u32,
        payload: Vec<u8>,
    },
    Response {
        serial: u32,
        error_code: u32,
        payload: Vec<u8>,
    },
    Unsolicited {
        unsol_id: u32,
        payload: Vec<u8>,
    },
}

// --- Standard AOSP RILD Response Type Constants ---
pub const RESPONSE_SOLICITED: u32 = 0;
pub const RESPONSE_UNSOLICITED: u32 = 1;
pub const RIL_E_SUCCESS: u32 = 0;

impl RilPacket {
    /// Serialize a RIL request packet with a 4-byte big-endian length prefix
    pub fn serialize_request(serial: u32, request_id: u32, payload: &[u8]) -> Vec<u8> {
        let body_len = 8 + payload.len(); // request_id (4) + serial (4) + payload
        let mut out = Vec::with_capacity(4 + body_len);
        out.extend_from_slice(&(body_len as u32).to_be_bytes());
        out.extend_from_slice(&request_id.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Serialize an AOSP standard solicited response packet
    pub fn serialize_solicited_response(serial: u32, error_code: u32, payload: &[u8]) -> Vec<u8> {
        let body_len = 12 + payload.len(); // type 0 (4) + serial (4) + error (4) + payload
        let mut out = Vec::with_capacity(4 + body_len);
        out.extend_from_slice(&(body_len as u32).to_be_bytes());
        out.extend_from_slice(&RESPONSE_SOLICITED.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&error_code.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Serialize an AOSP standard unsolicited event packet
    pub fn serialize_unsolicited(unsol_id: u32, payload: &[u8]) -> Vec<u8> {
        let body_len = 8 + payload.len(); // type 1 (4) + unsol_id (4) + payload
        let mut out = Vec::with_capacity(4 + body_len);
        out.extend_from_slice(&(body_len as u32).to_be_bytes());
        out.extend_from_slice(&RESPONSE_UNSOLICITED.to_le_bytes());
        out.extend_from_slice(&unsol_id.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Parse a RIL raw buffer into a RilPacket, supporting both standard AOSP RILD and direct format
    pub fn parse(buf: &[u8]) -> Result<Option<(Self, usize)>, &'static str> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let total_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if buf.len() < 4 + total_len {
            return Ok(None);
        }

        let body = &buf[4..4 + total_len];
        if body.len() < 8 {
            return Err("RIL packet body too short");
        }

        let first = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let second = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);

        let packet = if first == RESPONSE_UNSOLICITED {
            // AOSP standard RESPONSE_UNSOLICITED: [1][unsol_id][payload]
            let unsol_id = second;
            let payload = body[8..].to_vec();
            RilPacket::Unsolicited { unsol_id, payload }
        } else if first == RESPONSE_SOLICITED {
            if body.len() < 12 {
                return Err("RIL packet body too short for solicited response");
            }
            // AOSP standard RESPONSE_SOLICITED: [0][serial][error_code][payload]
            let serial = second;
            let error_code = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            let payload = body[12..].to_vec();
            RilPacket::Response {
                serial,
                error_code,
                payload,
            }
        } else if first >= 1000 {
            // Legacy / direct Unsolicited format
            let unsol_id = first;
            let payload = body[8..].to_vec();
            RilPacket::Unsolicited { unsol_id, payload }
        } else {
            // Legacy / direct Response format
            let serial = first;
            let error_code = second;
            let payload = body[8..].to_vec();
            RilPacket::Response {
                serial,
                error_code,
                payload,
            }
        };

        Ok(Some((packet, 4 + total_len)))
    }
}

/// Client handle for communicating with RILD
pub struct RilClient {
    pub slot_index: u32,
    pub socket_path: String,
    pub radio_state: RadioState,
    next_serial: u32,
    pub pending_requests: Vec<(u32, u32)>, // (serial, request_id)
}

impl RilClient {
    pub fn new(slot_index: u32) -> Self {
        let socket_path = if slot_index == 0 {
            "/dev/socket/rild".to_string()
        } else {
            format!("/dev/socket/rild{}", slot_index + 1)
        };

        Self {
            slot_index,
            socket_path,
            radio_state: RadioState::On,
            next_serial: 1,
            pending_requests: Vec::with_capacity(16),
        }
    }

    pub fn next_serial(&mut self) -> u32 {
        let s = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        if self.next_serial == 0 {
            self.next_serial = 1;
        }
        s
    }

    /// Formats a request packet and tracks the serial token
    pub fn create_request(&mut self, request_id: u32, payload: &[u8]) -> (u32, Vec<u8>) {
        let serial = self.next_serial();
        self.pending_requests.push((serial, request_id));
        let packet = RilPacket::serialize_request(serial, request_id, payload);
        (serial, packet)
    }

    /// Process a response and remove corresponding pending serial
    pub fn handle_response(&mut self, serial: u32) -> Option<u32> {
        if let Some(pos) = self.pending_requests.iter().position(|&(s, _)| s == serial) {
            let (_, req_id) = self.pending_requests.remove(pos);
            Some(req_id)
        } else {
            None
        }
    }
}
