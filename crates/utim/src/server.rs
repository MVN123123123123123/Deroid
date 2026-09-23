//! UTIM Control Socket Server (/run/utim/control.sock).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

pub struct ControlServer {
    listener: UnixListener,
}

impl ControlServer {
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.exists() {
            let _ = fs::remove_file(path);
        }

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;

        // Restrict control socket to root or system group
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o660));

        Ok(Self { listener })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept()?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;
        Ok(stream)
    }

    /// Accept a connection together with the kernel-verified peer credentials
    /// (SO_PEERCRED). Used to gate privileged verbs (reboot, poweroff,
    /// oom-score) to root.
    pub fn accept_with_cred(&self) -> io::Result<(UnixStream, libc::ucred)> {
        let (stream, _) = self.listener.accept()?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;
        let cred = peer_cred(&stream)?;
        Ok((stream, cred))
    }
}

/// Query SO_PEERCRED for a connected Unix socket: kernel-verified pid/uid/gid.
pub fn peer_cred(stream: &UnixStream) -> io::Result<libc::ucred> {
    use std::os::unix::io::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred)
}
}
