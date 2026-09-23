//! Process reaper and signalfd handler for PID 1.

use std::io;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub struct SignalHandler {
    fd: OwnedFd,
}

impl SignalHandler {
    pub fn new() -> io::Result<Self> {
        let mut mask: libc::sigset_t = unsafe { mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGCHLD);
            libc::sigaddset(&mut mask, libc::SIGTERM);
            libc::sigaddset(&mut mask, libc::SIGINT);
            libc::sigaddset(&mut mask, libc::SIGHUP);
            libc::sigaddset(&mut mask, libc::SIGPWR);

            // Block signals so they are delivered via signalfd instead of calling default handlers
            let ret = libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }

            let sfd = libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
            if sfd < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Self {
                fd: OwnedFd::from_raw_fd(sfd),
            })
        }
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Read pending signals from signalfd.
    pub fn read_signals(&self) -> Vec<libc::signalfd_siginfo> {
        // Small pre-reserve: the hot path delivers 1-2 signals per wakeup.
        let mut signals = Vec::with_capacity(4);
        let mut info: libc::signalfd_siginfo = unsafe { mem::zeroed() };
        let size = mem::size_of::<libc::signalfd_siginfo>();

        loop {
            let ret = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    &mut info as *mut _ as *mut libc::c_void,
                    size,
                )
            };

            if ret == (size as isize) {
                signals.push(info);
            } else {
                break;
            }
        }

        signals
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExitInfo {
    pub pid: i32,
    pub status: i32,
    pub exited_cleanly: bool,
    pub exit_code: i32,
    pub signal: Option<i32>,
}

/// Reap all terminated child processes using waitpid(..., WNOHANG).
/// Prevents zombie processes from accumulating under PID 1.
pub fn reap_zombies() -> Vec<ProcessExitInfo> {
    let mut exits = Vec::new();

    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };

        if pid <= 0 {
            break;
        }

        let exited_cleanly = libc::WIFEXITED(status);
        let exit_code = if exited_cleanly {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };

        let signal = if libc::WIFSIGNALED(status) {
            Some(libc::WTERMSIG(status))
        } else {
            None
        };

        exits.push(ProcessExitInfo {
            pid,
            status,
            exited_cleanly,
            exit_code,
            signal,
        });
    }

    exits
}
