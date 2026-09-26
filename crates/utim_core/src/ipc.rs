//! IPC Protocol between UTIM daemon and CLI tools (utimctl, deb-systemd-*).
//! Self-contained line-based protocol without external serialization dependencies.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub const DEFAULT_CONTROL_SOCKET: &str = "/run/utim/control.sock";

/// Hardening bounds for the client-side response parser.
pub const MAX_IPC_LINE_BYTES: u64 = 65_536;
pub const MAX_IPC_UNITS: usize = 8192;

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
    /// Reject bytes that would corrupt the line framing. Unit/slice names are
    /// filenames; neither a space nor a newline is legal in one.
    fn check_field(kind: &str, v: &str) -> String {
        assert!(!v.is_empty(), "{kind}: empty field");
        assert!(v.len() <= 255, "{kind}: field too long ({} bytes)", v.len());
        assert!(
            !v.contains(['\n', '\r', ' ']),
            "{kind}: illegal byte in {v:?}"
        );
        v.to_string()
    }

    pub fn serialize(&self) -> String {
        match self {
            IpcRequest::Start(u) => format!("START {}\n", Self::check_field("unit", u)),
            IpcRequest::Stop(u) => format!("STOP {}\n", Self::check_field("unit", u)),
            IpcRequest::Restart(u) => format!("RESTART {}\n", Self::check_field("unit", u)),
            IpcRequest::Reload(u) => format!("RELOAD {}\n", Self::check_field("unit", u)),
            IpcRequest::Status(u) => format!("STATUS {}\n", Self::check_field("unit", u)),
            IpcRequest::ListUnits => "LIST_UNITS\n".to_string(),
            IpcRequest::DaemonReload => "DAEMON_RELOAD\n".to_string(),
            IpcRequest::Enable(u) => format!("ENABLE {}\n", Self::check_field("unit", u)),
            IpcRequest::Disable(u) => format!("DISABLE {}\n", Self::check_field("unit", u)),
            IpcRequest::IsActive(u) => format!("IS_ACTIVE {}\n", Self::check_field("unit", u)),
            IpcRequest::IsEnabled(u) => format!("IS_ENABLED {}\n", Self::check_field("unit", u)),
            IpcRequest::FreezeCgroup(s) => {
                format!("FREEZE_CGROUP {}\n", Self::check_field("slice", s))
            }
            IpcRequest::UnfreezeCgroup(s) => {
                format!("UNFREEZE_CGROUP {}\n", Self::check_field("slice", s))
            }
            IpcRequest::AcquireWakeLock(n) => {
                format!("ACQUIRE_WAKELOCK {}\n", Self::check_field("wakelock", n))
            }
            IpcRequest::ReleaseWakeLock(n) => {
                format!("RELEASE_WAKELOCK {}\n", Self::check_field("wakelock", n))
            }
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

        // Unit-taking commands require a non-empty argument; accepting an
        // empty name would address the wrong unit or panic downstream.
        let unit_arg = |a: &str| {
            if a.is_empty() {
                None
            } else {
                Some(a.to_string())
            }
        };

        match cmd {
            "START" => Some(IpcRequest::Start(unit_arg(args)?)),
            "STOP" => Some(IpcRequest::Stop(unit_arg(args)?)),
            "RESTART" => Some(IpcRequest::Restart(unit_arg(args)?)),
            "RELOAD" => Some(IpcRequest::Reload(unit_arg(args)?)),
            "STATUS" => Some(IpcRequest::Status(unit_arg(args)?)),
            "LIST_UNITS" => Some(IpcRequest::ListUnits),
            "DAEMON_RELOAD" => Some(IpcRequest::DaemonReload),
            "ENABLE" => Some(IpcRequest::Enable(unit_arg(args)?)),
            "DISABLE" => Some(IpcRequest::Disable(unit_arg(args)?)),
            "IS_ACTIVE" => Some(IpcRequest::IsActive(unit_arg(args)?)),
            "IS_ENABLED" => Some(IpcRequest::IsEnabled(unit_arg(args)?)),
            "FREEZE_CGROUP" => Some(IpcRequest::FreezeCgroup(unit_arg(args)?)),
            "UNFREEZE_CGROUP" => Some(IpcRequest::UnfreezeCgroup(unit_arg(args)?)),
            "ACQUIRE_WAKELOCK" => Some(IpcRequest::AcquireWakeLock(unit_arg(args)?)),
            "RELEASE_WAKELOCK" => Some(IpcRequest::ReleaseWakeLock(unit_arg(args)?)),
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
    /// Field separator and record terminator must not survive into a field.
    fn esc(s: &str) -> String {
        s.replace(['|', '\n', '\r'], " ")
    }

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
                format!(
                    "STATUS {}|{}|{}|{}|{}\n",
                    Self::esc(name),
                    Self::esc(state),
                    pid_str,
                    Self::esc(description),
                    Self::esc(details)
                )
            }
            IpcResponse::UnitList(units) => {
                let mut out = String::from("UNITS_BEGIN\n");
                for (name, state, desc) in units {
                    out.push_str(&format!(
                        "{}|{}|{}\n",
                        Self::esc(name),
                        Self::esc(state),
                        Self::esc(desc)
                    ));
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
        // Bound each response line: a malicious/compromised daemon cannot
        // force unbounded client memory growth.
        let mut limited = reader.by_ref().take(MAX_IPC_LINE_BYTES);
        let n = limited.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if n as u64 >= MAX_IPC_LINE_BYTES && !line.ends_with('\n') {
            return Ok(Some(IpcResponse::Err("Response line too long".to_string())));
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
            let mut complete = false;
            loop {
                if list.len() >= MAX_IPC_UNITS {
                    eprintln!(
                        "utimctl: unit list exceeds MAX_IPC_UNITS={MAX_IPC_UNITS}; truncated"
                    );
                    break;
                }
                line.clear();
                let mut limited = reader.by_ref().take(MAX_IPC_LINE_BYTES);
                let n = limited.read_line(&mut line)?;
                if n == 0 {
                    break;
                } // EOF: incomplete
                let item = line.trim();
                if item == "UNITS_END" {
                    complete = true;
                    break;
                }
                let parts: Vec<&str> = item.splitn(3, '|').collect();
                match parts.as_slice() {
                    [name, state, desc] => {
                        list.push(((*name).into(), (*state).into(), (*desc).into()))
                    }
                    _ => eprintln!("utimctl: skipping unparsable unit row: {item:?}"),
                }
            }
            if !complete {
                return Ok(Some(IpcResponse::Err(format!(
                    "unit list truncated after {} entries (no UNITS_END marker)",
                    list.len()
                ))));
            }
            return Ok(Some(IpcResponse::UnitList(list)));
        } else if let Some(rest) = trimmed.strip_prefix("TIME ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 3 {
                if let (Ok(k), Ok(i), Ok(t)) = (
                    parts[0].parse::<f64>(),
                    parts[1].parse::<f64>(),
                    parts[2].parse::<f64>(),
                ) {
                    return Ok(Some(IpcResponse::Time {
                        kernel_sec: k,
                        init_sec: i,
                        total_sec: t,
                    }));
                }
            }
        }

        Ok(Some(IpcResponse::Err(format!(
            "Unknown response: {}",
            trimmed
        ))))
    }
}

/// Send a request to UTIM and receive the response.
/// Read/write timeouts keep a hung daemon from blocking the CLI forever.
pub fn send_ipc_request(socket_path: &Path, req: &IpcRequest) -> std::io::Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
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
            details: "Loaded: /lib/systemd/system/ssh.service Main PID: 42".to_string(),
        };

        let serialized = resp.serialize();
        let mut reader = std::io::Cursor::new(serialized);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn test_status_details_newline_is_flattened_not_framed() {
        // S9: every field uses one escaper; a newline in details becomes a
        // space rather than a record break or a lossy backslash sequence.
        let resp = IpcResponse::Status {
            name: "ssh.service".to_string(),
            state: "active".to_string(),
            pid: Some(42),
            description: "OpenSSH Server".to_string(),
            details: "line one\nline two".to_string(),
        };
        let serialized = resp.serialize();
        assert_eq!(serialized.lines().count(), 1);
        let mut reader = std::io::Cursor::new(serialized);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        match parsed {
            IpcResponse::Status { details, .. } => {
                assert_eq!(details, "line one line two");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn test_status_pipe_in_name_does_not_shift_fields() {
        // S9: a `|` in a unit name is escaped so the positional parser still
        // sees exactly five fields.
        let resp = IpcResponse::Status {
            name: "a|b.service".to_string(),
            state: "active".to_string(),
            pid: None,
            description: "desc".to_string(),
            details: String::new(),
        };
        let serialized = resp.serialize();
        assert_eq!(serialized.lines().count(), 1);
        let mut reader = std::io::Cursor::new(serialized);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        match parsed {
            IpcResponse::Status { name, state, .. } => {
                assert_eq!(name, "a b.service");
                assert_eq!(state, "active");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    #[should_panic(expected = "illegal byte")]
    fn test_request_space_in_unit_panics_loudly() {
        // S6: a space would silently retarget the unit server-side; panic
        // (abort in the shipped profile) instead of operating on the wrong unit.
        let _ = IpcRequest::Start("a b.service".to_string()).serialize();
    }

    #[test]
    #[should_panic(expected = "illegal byte")]
    fn test_request_newline_in_unit_panics_loudly() {
        // S6: a newline would silently discard the remainder of the record.
        let _ = IpcRequest::Start("a.service\nb.service".to_string()).serialize();
    }

    #[test]
    #[should_panic(expected = "empty field")]
    fn test_request_empty_unit_panics_loudly() {
        let _ = IpcRequest::Stop(String::new()).serialize();
    }

    #[test]
    #[should_panic(expected = "too long")]
    fn test_request_overlong_unit_panics_loudly() {
        let _ = IpcRequest::Status("x".repeat(256)).serialize();
    }

    #[test]
    fn test_unit_list_eof_without_terminator_is_an_error() {
        // S8: EOF before UNITS_END must not be reported as a whole list.
        let raw = "UNITS_BEGIN\na.service|active|Desc A\nb.service|active|Desc B\n";
        let mut reader = std::io::Cursor::new(raw);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        match parsed {
            IpcResponse::Err(msg) => assert!(msg.contains("no UNITS_END marker"), "{msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn test_unit_list_malformed_row_skipped_but_terminated_list_ok() {
        // S8: a garbage row is reported on stderr and skipped; a terminated
        // list still parses.
        let raw = "UNITS_BEGIN\na.service|active|Desc A\nGARBAGE-NO-PIPES\nb.service|inactive|Desc B\nUNITS_END\n";
        let mut reader = std::io::Cursor::new(raw);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        match parsed {
            IpcResponse::UnitList(units) => {
                assert_eq!(units.len(), 2);
                assert_eq!(units[0].0, "a.service");
                assert_eq!(units[1].0, "b.service");
            }
            other => panic!("expected UnitList, got {other:?}"),
        }
    }

    #[test]
    fn test_unit_list_at_cap_without_terminator_is_an_error() {
        // S8: hitting MAX_IPC_UNITS without UNITS_END is truncation, not success.
        let mut raw = String::from("UNITS_BEGIN\n");
        for i in 0..MAX_IPC_UNITS {
            raw.push_str(&format!("u{i}.service|active|d\n"));
        }
        let mut reader = std::io::Cursor::new(raw);
        let parsed = IpcResponse::deserialize(&mut reader).unwrap().unwrap();
        match parsed {
            IpcResponse::Err(msg) => assert!(msg.contains("truncated"), "{msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
