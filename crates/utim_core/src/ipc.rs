//! IPC Protocol between UTIM daemon and CLI tools (utimctl, deb-systemd-*).
//! Self-contained line-based protocol without external serialization dependencies.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub const DEFAULT_CONTROL_SOCKET: &str = "/run/utim/control.sock";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcRequest {
    Start(String),
    Stop(String),
    Restart(String),
    Reload(String),
    Status(String),
    ListUnits,
    DaemonReload,
    Enable(String),
    Disable(String),
    IsActive(String),
    IsEnabled(String),
    FreezeCgroup(String),
    UnfreezeCgroup(String),
    AcquireWakeLock(String),
    ReleaseWakeLock(String),
    SetOomScore(i32, i32),
    AnalyzeTime,
    Reboot,
    Poweroff,
}

impl IpcRequest {
    pub fn serialize(&self) -> String {
        match self {
            IpcRequest::Start(u) => format!("START {}\n", u),
            IpcRequest::Stop(u) => format!("STOP {}\n", u),
            IpcRequest::Restart(u) => format!("RESTART {}\n", u),
            IpcRequest::Reload(u) => format!("RELOAD {}\n", u),
            IpcRequest::Status(u) => format!("STATUS {}\n", u),
            IpcRequest::ListUnits => "LIST_UNITS\n".to_string(),
            IpcRequest::DaemonReload => "DAEMON_RELOAD\n".to_string(),
            IpcRequest::Enable(u) => format!("ENABLE {}\n", u),
            IpcRequest::Disable(u) => format!("DISABLE {}\n", u),
            IpcRequest::IsActive(u) => format!("IS_ACTIVE {}\n", u),
            IpcRequest::IsEnabled(u) => format!("IS_ENABLED {}\n", u),
            IpcRequest::FreezeCgroup(s) => format!("FREEZE_CGROUP {}\n", s),
            IpcRequest::UnfreezeCgroup(s) => format!("UNFREEZE_CGROUP {}\n", s),
            IpcRequest::AcquireWakeLock(n) => format!("ACQUIRE_WAKELOCK {}\n", n),
            IpcRequest::ReleaseWakeLock(n) => format!("RELEASE_WAKELOCK {}\n", n),
            IpcRequest::SetOomScore(pid, score) => format!("SET_OOM_SCORE {} {}\n", pid, score),
            IpcRequest::AnalyzeTime => "ANALYZE_TIME\n".to_string(),
            IpcRequest::Reboot => "REBOOT\n".to_string(),
            IpcRequest::Poweroff => "POWEROFF\n".to_string(),
        }
    }

    pub fn deserialize(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        let (cmd, args) = match trimmed.split_once(' ') {
            Some((c, a)) => (c, a.trim()),
            None => (trimmed, ""),
        };

        match cmd {
            "START" => Some(IpcRequest::Start(args.to_string())),
            "STOP" => Some(IpcRequest::Stop(args.to_string())),
            "RESTART" => Some(IpcRequest::Restart(args.to_string())),
            "RELOAD" => Some(IpcRequest::Reload(args.to_string())),
            "STATUS" => Some(IpcRequest::Status(args.to_string())),
            "LIST_UNITS" => Some(IpcRequest::ListUnits),
            "DAEMON_RELOAD" => Some(IpcRequest::DaemonReload),
            "ENABLE" => Some(IpcRequest::Enable(args.to_string())),
            "DISABLE" => Some(IpcRequest::Disable(args.to_string())),
            "IS_ACTIVE" => Some(IpcRequest::IsActive(args.to_string())),
            "IS_ENABLED" => Some(IpcRequest::IsEnabled(args.to_string())),
            "FREEZE_CGROUP" => Some(IpcRequest::FreezeCgroup(args.to_string())),
            "UNFREEZE_CGROUP" => Some(IpcRequest::UnfreezeCgroup(args.to_string())),
            "ACQUIRE_WAKELOCK" => Some(IpcRequest::AcquireWakeLock(args.to_string())),
            "RELEASE_WAKELOCK" => Some(IpcRequest::ReleaseWakeLock(args.to_string())),
            "SET_OOM_SCORE" => {
                let parts: Vec<&str> = args.split_whitespace().collect();
                if parts.len() >= 2 {
                    let pid = parts[0].parse().ok()?;
                    let score = parts[1].parse().ok()?;
                    Some(IpcRequest::SetOomScore(pid, score))
                } else {
                    None
                }
            }
            "ANALYZE_TIME" => Some(IpcRequest::AnalyzeTime),
            "REBOOT" => Some(IpcRequest::Reboot),
            "POWEROFF" => Some(IpcRequest::Poweroff),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum IpcResponse {
    Ok(String),
    Err(String),
    Status {
        name: String,
        state: String,
        pid: Option<i32>,
        description: String,
        details: String,
    },
    UnitList(Vec<(String, String, String)>), // name, state, description
    Time {
        kernel_sec: f64,
        init_sec: f64,
        total_sec: f64,
    },
}

impl IpcResponse {
    pub fn serialize(&self) -> String {
        match self {
            IpcResponse::Ok(msg) => format!("OK {}\n", msg),
            IpcResponse::Err(msg) => format!("ERR {}\n", msg),
            IpcResponse::Status {
                name,
                state,
                pid,
                description,
                details,
            } => {
                let pid_str = pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-1".to_string());
                let escaped_desc = description.replace('|', " ");
                let escaped_details = details.replace('\n', "\\n");
                format!(
                    "STATUS {}|{}|{}|{}|{}\n",
                    name, state, pid_str, escaped_desc, escaped_details
                )
            }
            IpcResponse::UnitList(units) => {
                let mut out = String::from("UNITS_BEGIN\n");
                for (name, state, desc) in units {
                    out.push_str(&format!("{}|{}|{}\n", name, state, desc.replace('\n', " ")));
                }
                out.push_str("UNITS_END\n");
                out
            }
            IpcResponse::Time {
                kernel_sec,
                init_sec,
                total_sec,
            } => {
                format!("TIME {} {} {}\n", kernel_sec, init_sec, total_sec)
            }
        }
    }

    pub fn deserialize<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Self>> {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }

        let trimmed = line.trim();
        if let Some(msg) = trimmed.strip_prefix("OK ") {
            return Ok(Some(IpcResponse::Ok(msg.to_string())));
        } else if trimmed == "OK" {
            return Ok(Some(IpcResponse::Ok(String::new())));
        } else if let Some(msg) = trimmed.strip_prefix("ERR ") {
            return Ok(Some(IpcResponse::Err(msg.to_string())));
        } else if let Some(rest) = trimmed.strip_prefix("STATUS ") {
            let parts: Vec<&str> = rest.splitn(5, '|').collect();
            if parts.len() >= 4 {
                let name = parts[0].to_string();
                let state = parts[1].to_string();
                let pid = parts[2].parse::<i32>().ok().filter(|&p| p >= 0);
                let description = parts[3].to_string();
                let details = if parts.len() == 5 {
                    parts[4].replace("\\n", "\n")
                } else {
                    String::new()
                };
                return Ok(Some(IpcResponse::Status {
                    name,
                    state,
                    pid,
                    description,
                    details,
                }));
            }
        } else if trimmed == "UNITS_BEGIN" {
            let mut list = Vec::new();
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    break;
                }
                let item = line.trim();
                if item == "UNITS_END" {
                    break;
                }
                let parts: Vec<&str> = item.split('|').collect();
                if parts.len() >= 3 {
                    list.push((
                        parts[0].to_string(),
                        parts[1].to_string(),
                        parts[2].to_string(),
                    ));
                }
            }
            return Ok(Some(IpcResponse::UnitList(list)));
        } else if let Some(rest) = trimmed.strip_prefix("TIME ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 3 {
                let k = parts[0].parse().unwrap_or(0.0);
                let i = parts[1].parse().unwrap_or(0.0);
                let t = parts[2].parse().unwrap_or(0.0);
                return Ok(Some(IpcResponse::Time {
                    kernel_sec: k,
                    init_sec: i,
                    total_sec: t,
                }));
            }
        }

        Ok(Some(IpcResponse::Err(format!(
            "Unknown response: {}",
            trimmed
        ))))
    }
}

/// Send a request to UTIM and receive the response.
pub fn send_ipc_request(socket_path: &Path, req: &IpcRequest) -> std::io::Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(req.serialize().as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    IpcResponse::deserialize(&mut reader)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Connection closed by server",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_request_serialization() {
        let req = IpcRequest::Start("ssh.service".to_string());
        assert_eq!(req.serialize(), "START ssh.service\n");
        assert_eq!(IpcRequest::deserialize("START ssh.service\n"), Some(req));

        let req_oom = IpcRequest::SetOomScore(1234, -500);
        assert_eq!(req_oom.serialize(), "SET_OOM_SCORE 1234 -500\n");
        assert_eq!(
            IpcRequest::deserialize("SET_OOM_SCORE 1234 -500"),
            Some(req_oom)
        );
    }

    #[test]
    fn test_ipc_response_serialization() {
        let resp = IpcResponse::Status {
            name: "ssh.service".to_string(),
            state: "active".to_string(),
            pid: Some(42),
            description: "OpenSSH Server".to_string(),
            details: "Loaded: /lib/systemd/system/ssh.service\nMain PID: 42".to_string(),
        };

        let serialized = resp.serialize();
        let mut reader = std::io::Cursor::new(serialized);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        assert_eq!(resp, parsed);
    }
}
