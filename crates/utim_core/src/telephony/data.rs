//! Mobile Cellular Data (4G LTE / 5G NR) Connection Management.
//! Handles APN configuration, data call activation/deactivation via RIL,
//! and network interface configuration (rmnet_data0 / ccmni0).
//! Conforms strictly to GEMINI.md systems discipline.

use super::ril_client::{RilClient, RIL_REQUEST_DEACTIVATE_DATA_CALL, RIL_REQUEST_SETUP_DATA_CALL};

/// APN Authentication Protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApnAuthType {
    None = 0,
    Pap = 1,
    Chap = 2,
    PapOrChap = 3,
}

/// PDP Data Protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpProtocol {
    Ipv4,
    Ipv6,
    DualIpv4Ipv6,
}

impl PdpProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ipv4 => "IP",
            Self::Ipv6 => "IPV6",
            Self::DualIpv4Ipv6 => "IPV4V6",
        }
    }
}

/// APN Profile Definition
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApnProfile {
    pub apn: String,
    pub user: String,
    pub pass: String,
    pub auth_type: ApnAuthType,
    pub protocol: PdpProtocol,
}

impl ApnProfile {
    pub fn default_lte() -> Self {
        Self {
            apn: "internet".to_string(),
            user: String::new(),
            pass: String::new(),
            auth_type: ApnAuthType::None,
            protocol: PdpProtocol::DualIpv4Ipv6,
        }
    }
}

/// Mobile Data Call Connection State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataCallState {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Failed,
}

/// Active Data Call Session
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataCallSession {
    pub cid: u32,
    pub state: DataCallState,
    pub ifname: String,
    pub ipv4_addr: String,
    pub ipv4_gateway: String,
    pub dns_servers: Vec<String>,
    pub mtu: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Cellular Mobile Data Manager
pub struct MobileDataManager {
    pub active_session: Option<DataCallSession>,
    pub apn_profile: ApnProfile,
    next_cid: u32,
}

impl MobileDataManager {
    pub fn new(apn: ApnProfile) -> Self {
        Self {
            active_session: None,
            apn_profile: apn,
            next_cid: 1,
        }
    }

    /// Initiate mobile data call setup via RIL
    pub fn setup_data_call(&mut self, ril: &mut RilClient) -> (u32, Vec<u8>) {
        let cid = self.next_cid;
        self.next_cid += 1;

        // Formulate RIL_REQUEST_SETUP_DATA_CALL payload
        let mut payload = Vec::with_capacity(128);
        payload.extend_from_slice(&(cid).to_le_bytes());
        payload.extend_from_slice(self.apn_profile.apn.as_bytes());
        payload.push(0);
        payload.extend_from_slice(self.apn_profile.protocol.as_str().as_bytes());
        payload.push(0);

        let (_serial, packet) = ril.create_request(RIL_REQUEST_SETUP_DATA_CALL, &payload);

        let session = DataCallSession {
            cid,
            state: DataCallState::Connecting,
            ifname: if ril.slot_index == 0 {
                "rmnet_data0".to_string()
            } else {
                "rmnet_data1".to_string()
            },
            ipv4_addr: String::new(),
            ipv4_gateway: String::new(),
            dns_servers: Vec::new(),
            mtu: 1500,
            rx_bytes: 0,
            tx_bytes: 0,
        };
        self.active_session = Some(session);

        (cid, packet)
    }

    /// Complete data call setup after RIL response
    pub fn on_data_call_connected(&mut self, ip: &str, gateway: &str, dns: &[&str], mtu: u32) {
        if let Some(ref mut session) = self.active_session {
            session.state = DataCallState::Connected;
            session.ipv4_addr = ip.to_string();
            session.ipv4_gateway = gateway.to_string();
            session.dns_servers = dns.iter().map(|&s| s.to_string()).collect();
            session.mtu = mtu;
        }
    }

    /// Deactivate active mobile data call
    pub fn deactivate_data_call(&mut self, ril: &mut RilClient) -> Result<Vec<u8>, &'static str> {
        let session = self
            .active_session
            .as_mut()
            .ok_or("No active data call session")?;

        session.state = DataCallState::Disconnecting;
        let cid_bytes = session.cid.to_le_bytes();
        let (_serial, packet) = ril.create_request(RIL_REQUEST_DEACTIVATE_DATA_CALL, &cid_bytes);
        Ok(packet)
    }

    /// Finalize data call termination
    pub fn on_data_call_disconnected(&mut self) {
        if let Some(ref mut session) = self.active_session {
            session.state = DataCallState::Disconnected;
        }
    }

    /// Check if mobile data is currently active and routed
    pub fn is_data_active(&self) -> bool {
        self.active_session
            .as_ref()
            .is_some_and(|s| s.state == DataCallState::Connected)
    }
}
