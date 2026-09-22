//! sd_notify datagram server emulation (/run/systemd/notify).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;

pub const NOTIFY_SOCKET_PATH: &str = "/run/systemd/notify";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyMessage {
    pub ready: bool,
    pub status: Option<String>,
    pub watchdog: bool,
    pub mainpid: Option<i32>,
    pub errno: Option<i32>,
}

pub struct NotifyServer {
    socket: UnixDatagram,
}

impl NotifyServer {
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.exists() {
            let _ = fs::remove_file(path);
        }

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;

        // Enable SO_PASSCRED so the kernel transmits sender PID via SCM_CREDENTIALS
        let one: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        // Set permissions so unprivileged services can write to notify socket
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o777));

        Ok(Self { socket })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    /// Read incoming datagram messages with kernel-verified sender PID via SCM_CREDENTIALS.
    pub fn recv_messages_with_sender(&self) -> Vec<(NotifyMessage, Option<i32>)> {
        let mut messages = Vec::new();
        let mut buf = [0u8; 4096];
        let mut cmsg_buf = [0u8; 128];

        loop {
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            };

            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_buf.len() as _;

            let ret =
                unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut msg, libc::MSG_DONTWAIT) };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                break;
            }

            let size = ret as usize;
            let msg_str = String::from_utf8_lossy(&buf[..size]);
            if let Some(parsed) = parse_notify_payload(&msg_str) {
                let mut sender_pid = None;
                unsafe {
                    let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
                    while !cmsg.is_null() {
                        if (*cmsg).cmsg_level == libc::SOL_SOCKET
                            && (*cmsg).cmsg_type == libc::SCM_CREDENTIALS
                        {
                            let ucred_ptr = libc::CMSG_DATA(cmsg) as *const libc::ucred;
                            let ucred = *ucred_ptr;
                            if ucred.pid > 0 {
                                sender_pid = Some(ucred.pid);
                            }
                            break;
                        }
                        cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
                    }
                }

                messages.push((parsed, sender_pid));
            }
        }

        messages
    }

    /// Read incoming datagram messages.
    #[allow(dead_code)]
    pub fn recv_messages(&self) -> Vec<NotifyMessage> {
        self.recv_messages_with_sender()
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    }
}

pub fn parse_notify_payload(payload: &str) -> Option<NotifyMessage> {
    let mut ready = false;
    let mut status = None;
    let mut watchdog = false;
    let mut mainpid = None;
    let mut errno = None;

    for line in payload.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some((k, v)) = trimmed.split_once('=') {
            match k {
                "READY" => ready = v == "1",
                "STATUS" => status = Some(v.to_string()),
                "WATCHDOG" => watchdog = v == "1",
                "MAINPID" => mainpid = v.parse::<i32>().ok(),
                "ERRNO" => errno = v.parse::<i32>().ok(),
                _ => {}
            }
        }
    }

    Some(NotifyMessage {
        ready,
        status,
        watchdog,
        mainpid,
        errno,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_notify() {
        let payload = "READY=1\nSTATUS=Listening on port 80\nWATCHDOG=1\nMAINPID=5432\n";
        let msg = parse_notify_payload(payload).unwrap();
        assert!(msg.ready);
        assert_eq!(msg.status.as_deref(), Some("Listening on port 80"));
        assert!(msg.watchdog);
        assert_eq!(msg.mainpid, Some(5432));
    }
}
