//! Telephony Voice Call Management & VoLTE / VoNR Integration.
//! State machine for incoming/outgoing mobile voice calls, multiparty handling,
//! and carrier IMS registration reporting.
//! Conforms strictly to GEMINI.md: zero redundant dependencies, predictable state machine.

use super::ril_client::{RilClient, RIL_REQUEST_ANSWER, RIL_REQUEST_DIAL, RIL_REQUEST_HANGUP};

/// Call State (3GPP TS 27.007 + Android RIL)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Active = 0,
    Holding = 1,
    Dialing = 2,
    Alerting = 3,
    Incoming = 4,
    Waiting = 5,
    Terminated = 6,
}

/// Active or Pending Voice Call
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceCall {
    pub call_id: u32,
    pub index: u32,
    pub state: CallState,
    pub number: String,
    pub is_incoming: bool,
    pub is_multiparty: bool,
    pub is_emergency: bool,
}

/// Carrier IMS Registration Status for VoLTE and VoNR
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImsRegistrationStatus {
    pub registered: bool,
    pub voice_over_lte_supported: bool,
    pub voice_over_nr_supported: bool,
    pub sms_over_ims_supported: bool,
}

impl Default for ImsRegistrationStatus {
    fn default() -> Self {
        Self {
            registered: true,
            voice_over_lte_supported: true,
            voice_over_nr_supported: true,
            sms_over_ims_supported: true,
        }
    }
}

/// Voice Call Manager
pub struct VoiceCallManager {
    pub calls: Vec<VoiceCall>,
    pub ims_status: ImsRegistrationStatus,
    next_call_id: u32,
}

impl Default for VoiceCallManager {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceCallManager {
    pub fn new() -> Self {
        Self {
            calls: Vec::with_capacity(4),
            ims_status: ImsRegistrationStatus::default(),
            next_call_id: 1,
        }
    }

    /// Check if a phone number is a recognized emergency number
    pub fn is_emergency_number(number: &str) -> bool {
        let clean = number.trim();
        matches!(clean, "112" | "911" | "999" | "000" | "110" | "119" | "118")
    }

    fn allocate_call_index(&self) -> u32 {
        let mut idx = 1;
        while self.calls.iter().any(|c| c.index == idx) {
            idx += 1;
        }
        idx
    }

    /// Initiate an outgoing voice call
    pub fn dial(&mut self, number: &str, ril: &mut RilClient) -> (u32, Vec<u8>) {
        let id = self.next_call_id;
        self.next_call_id += 1;

        let is_emergency = Self::is_emergency_number(number);

        let call = VoiceCall {
            call_id: id,
            index: self.allocate_call_index(),
            state: CallState::Dialing,
            number: number.to_string(),
            is_incoming: false,
            is_multiparty: false,
            is_emergency,
        };
        self.calls.push(call);

        // Serialize RIL_REQUEST_DIAL payload: dial string with null terminator
        let mut payload = Vec::with_capacity(number.len() + 4);
        payload.extend_from_slice(number.as_bytes());
        payload.push(0); // null terminator

        let (_serial, packet) = ril.create_request(RIL_REQUEST_DIAL, &payload);
        (id, packet)
    }

    /// Process an incoming call event
    pub fn on_incoming_call(&mut self, number: &str) -> u32 {
        let id = self.next_call_id;
        self.next_call_id += 1;

        let call = VoiceCall {
            call_id: id,
            index: self.allocate_call_index(),
            state: CallState::Incoming,
            number: number.to_string(),
            is_incoming: true,
            is_multiparty: false,
            is_emergency: Self::is_emergency_number(number),
        };
        self.calls.push(call);
        id
    }

    /// Answer an incoming call
    pub fn answer(&mut self, call_id: u32, ril: &mut RilClient) -> Result<Vec<u8>, &'static str> {
        let call = self
            .calls
            .iter_mut()
            .find(|c| c.call_id == call_id)
            .ok_or("Call not found")?;

        if call.state != CallState::Incoming && call.state != CallState::Waiting {
            return Err("Call is not incoming");
        }

        call.state = CallState::Active;
        let (_serial, packet) = ril.create_request(RIL_REQUEST_ANSWER, &[]);
        Ok(packet)
    }

    /// Hang up an active or dialing call
    pub fn hangup(&mut self, call_id: u32, ril: &mut RilClient) -> Result<Vec<u8>, &'static str> {
        let pos = self
            .calls
            .iter()
            .position(|c| c.call_id == call_id)
            .ok_or("Call not found")?;

        let call_index = self.calls[pos].index;
        self.calls.remove(pos);

        // RIL_REQUEST_HANGUP takes 4-byte call index
        let payload = call_index.to_le_bytes();
        let (_serial, packet) = ril.create_request(RIL_REQUEST_HANGUP, &payload);
        Ok(packet)
    }

    /// Remote party hung up or network released call
    pub fn on_call_terminated(&mut self, call_id: u32) {
        if let Some(pos) = self.calls.iter().position(|c| c.call_id == call_id) {
            self.calls.remove(pos);
        }
    }

    /// Update call state to Alerting / Ringing
    pub fn on_remote_alerting(&mut self, call_id: u32) {
        if let Some(call) = self.calls.iter_mut().find(|c| c.call_id == call_id) {
            if call.state == CallState::Dialing {
                call.state = CallState::Alerting;
            }
        }
    }

    /// Update call state to Active
    pub fn on_call_connected(&mut self, call_id: u32) {
        if let Some(call) = self.calls.iter_mut().find(|c| c.call_id == call_id) {
            call.state = CallState::Active;
        }
    }

    /// Check if any call is currently in progress
    pub fn has_active_call(&self) -> bool {
        self.calls.iter().any(|c| c.state != CallState::Terminated)
    }
}
