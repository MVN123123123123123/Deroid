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
                listener.into_raw_fd()
            };

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
    pub fn all_fds(&self) -> Vec<RawFd> {
        self.active_sockets.iter().map(|s| s.raw_fd).collect()
    }

    #[allow(dead_code)]
    pub fn find_by_fd(&self, fd: RawFd) -> Option<&ActiveSocket> {
        self.active_sockets.iter().find(|s| s.raw_fd == fd)
    }
}
