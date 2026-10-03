//! Universal Treble Telephony & Cellular Modem Subsystem (Phase 4 Milestone 4.2).
//! Integrates Android Radio Interface Layer (RIL) communication (/dev/socket/rild),
//! ModemManager / oFono D-Bus bridge, voice call state machine, VoLTE / VoNR reporting,
//! 3GPP PDU SMS encoder/decoder, 4G/5G mobile data setup, Dual-SIM support,
//! protected cgroup slicing (oom_score_adj = -800), and deep suspend wake management.

pub mod data;
pub mod modem;
pub mod ril_client;
pub mod sms;
pub mod voice;
pub mod wake;

pub use data::{
    ApnAuthType, ApnProfile, DataCallSession, DataCallState, MobileDataManager, PdpProtocol,
};
pub use modem::{
    ModemManagerBridge, ModemState, RadioAccessTechnology, SimCardState, SimSlot,
    TELEPHONY_CGROUP_PATH, TELEPHONY_OOM_SCORE_ADJ,
};
pub use ril_client::{
    RadioState, RilClient, RilPacket, RESPONSE_SOLICITED, RESPONSE_UNSOLICITED, RIL_E_SUCCESS,
    RIL_REQUEST_ANSWER, RIL_REQUEST_DEACTIVATE_DATA_CALL, RIL_REQUEST_DIAL,
    RIL_REQUEST_GET_CURRENT_CALLS, RIL_REQUEST_GET_SIM_STATUS, RIL_REQUEST_HANGUP,
    RIL_REQUEST_OPERATOR, RIL_REQUEST_RADIO_POWER, RIL_REQUEST_SEND_SMS,
    RIL_REQUEST_SETUP_DATA_CALL, RIL_REQUEST_SIGNAL_STRENGTH,
    RIL_UNSOL_RESPONSE_CALL_STATE_CHANGED, RIL_UNSOL_RESPONSE_NEW_SMS,
    RIL_UNSOL_RESPONSE_RADIO_STATE_CHANGED, RIL_UNSOL_SIGNAL_STRENGTH,
};
pub use sms::{
    decode_address_semi_octets, decode_gsm7, encode_address_semi_octets, encode_gsm7,
    encode_sms_deliver_pdu, encode_sms_submit_pdu, pack_7bit, pack_7bit_with_bit_offset,
    parse_sms_deliver_pdu, unpack_7bit, unpack_7bit_from_bit_offset, SmsEncoding, SmsMessage,
    SmsReassembler,
};
pub use voice::{CallState, ImsRegistrationStatus, VoiceCall, VoiceCallManager};
pub use wake::{TelephonyWakeManager, WakeReason, TELEPHONY_WAKE_LOCK};

/// Full Phase 4 Telephony Subsystem Bring-Up Status
#[derive(Debug, Clone, PartialEq)]
pub struct TelephonyBringupStatus {
    pub dual_sim_supported: bool,
    pub primary_data_slot: u32,
    pub volte_registered: bool,
    pub vonr_supported: bool,
    pub mobile_data_active: bool,
    pub oom_score_adj_compliant: bool,
}

impl TelephonyBringupStatus {
    pub fn is_ready(&self) -> bool {
        self.volte_registered && self.oom_score_adj_compliant
    }
}
