//! Socket activation engine conforming to sd_listen_fds ABI.

use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use utim_core::unit::SocketSection;

pub const SD_LISTEN_FDS_START: i32 = 3;

#[derive(Clone)]
#[allow(dead_code)]
pub struct ActiveSocket {
    pub name: String,
    pub service_name: String,
    pub raw_fd: RawFd,
}

pub struct SocketActivationManager {
    active_sockets: Vec<ActiveSocket>,
}

impl SocketActivationManager {
    pub fn new() -> Self {
        Self {
            active_sockets: Vec::new(),
        }
    }

    pub fn bind_socket(&mut self, unit_name: &str, socket_sec: &SocketSection) -> io::Result<()> {
        // Re-binding an already-bound socket unit (daemon-reload) reuses the
        // existing FDs instead of leaking a second listener pair.
        if self
            .active_sockets
            .iter()
            .any(|s| s.name == unit_name)
        {
            return Ok(());
        }

        let service_name = socket_sec
            .service
            .clone()
            .unwrap_or_else(|| unit_name.replace(".socket", ".service"));

        for stream in &socket_sec.listen_stream {
            let fd = if stream.starts_with('/') {
                // UNIX domain stream socket
                let path = Path::new(stream);
                if path.exists() {
                    let _ = fs::remove_file(path);
                }
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let listener = UnixListener::bind(path)?;
                listener.set_nonblocking(true)?;
                if let Some(mode) = socket_sec.socket_mode {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
                }
                listener.into_raw_fd()
            } else {
                // TCP stream socket (ip:port or just port)
                let addr = if stream.contains(':') {
                    stream.to_string()
                } else {
                    format!("0.0.0.0:{}", stream)
                };
                let listener = TcpListener::bind(addr)?;
                listener.set_nonblocking(true)?;
                listener.into_raw_fd()
            };

            self.active_sockets.push(ActiveSocket {
                name: unit_name.to_string(),
                service_name: service_name.clone(),
                raw_fd: fd,
            });
        }

        for datagram in &socket_sec.listen_datagram {
            let fd = bind_datagram_fd(datagram, socket_sec.socket_mode)?;
            self.active_sockets.push(ActiveSocket {
                name: unit_name.to_string(),
                service_name: service_name.clone(),
                raw_fd: fd,
            });
        }

        Ok(())
    }

    pub fn sockets_for_service(&self, service_name: &str) -> Vec<RawFd> {
        self.active_sockets
            .iter()
            .filter(|s| s.service_name == service_name)
            .map(|s| s.raw_fd)
            .collect()
    }

    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.active_sockets.len()
    }

    #[allow(dead_code)]
    pub fn all_fds(&self) -> Vec<RawFd> {
        self.active_sockets.iter().map(|s| s.raw_fd).collect()
    }

    #[allow(dead_code)]
    pub fn find_by_fd(&self, fd: RawFd) -> Option<&ActiveSocket> {
        self.active_sockets.iter().find(|s| s.raw_fd == fd)
    }

    /// Drop all sockets belonging to units no longer loaded (daemon-reload
    /// pruning). Returns the removed FDs WITHOUT closing: the caller must
    /// EPOLL_CTL_DEL first, then close, otherwise the DEL operates on a
    /// recycled fd number (P8/B8).
    pub fn prune_removed_units(
        &mut self,
        live_units: &std::collections::HashSet<String>,
    ) -> Vec<RawFd> {
        let mut removed = Vec::new();
        self.active_sockets.retain(|s| {
            if live_units.contains(&s.name) {
                true
            } else {
                removed.push(s.raw_fd);
                false
            }
        });
        removed
    }
}

/// Bind a datagram socket (Unix path or ip:port / port) non-blocking and
/// return the raw FD for socket activation.
fn bind_datagram_fd(spec: &str, mode: Option<u32>) -> io::Result<RawFd> {
    use std::os::unix::net::UnixDatagram;

    if spec.starts_with('/') {
        let path = Path::new(spec);
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let sock = UnixDatagram::bind(path)?;
        sock.set_nonblocking(true)?;
        if let Some(m) = mode {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(m));
        }
        Ok(sock.into_raw_fd())
    } else {
        let addr = if spec.contains(':') {
            spec.to_string()
        } else {
            format!("0.0.0.0:{}", spec)
        };
        let sock = std::net::UdpSocket::bind(addr)?;
        sock.set_nonblocking(true)?;
        Ok(sock.into_raw_fd())
    }
}
