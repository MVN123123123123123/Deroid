//! Interactive shell session identity.
//!
//! UTIM (PID 1) and UTLC (the Wayland compositor) legitimately run as root, but
//! the commands typed into the shell UI are executed as the unprivileged mobile
//! session account, exactly like a desktop session. The prompt must therefore
//! describe the *child's* identity, not the compositor's: a hardcoded
//! `root@host:~#` prompt made perfectly normal `apt`/`dpkg` refusals
//! ("requested operation requires superuser privilege") look like a privilege
//! bug on a privileged shell instead of the honest unprivileged shell it was.
//!
//! Identity is resolved exactly once into fixed-size buffers and cached in a
//! [`OnceLock`], so the render loop and the spawn paths share one source of
//! truth and allocate nothing. Only POSIX `getpwuid_r`/`uname` are used; no
//! external NSS crate is pulled in.

use std::ffi::CString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::OnceLock;

/// UID the session commands run as once root privileges are dropped.
pub const SESSION_UID: libc::uid_t = 1000;
/// GID the session commands run as once root privileges are dropped.
pub const SESSION_GID: libc::gid_t = 1000;
/// Home directory of the unprivileged session account.
pub const SESSION_HOME: &str = "/home/user";
/// XDG runtime directory owned by the session account.
pub const SESSION_RUNTIME_DIR: &str = "/run/user/1000";
/// Supplementary groups: session, render, video, audio, input, plugdev, cdrom.
pub const SESSION_GROUPS: [libc::gid_t; 7] = [1000, 24, 27, 29, 44, 105, 107];

/// Bounded capacities: `LOGIN_NAME_MAX`, `HOST_NAME_MAX` and a capped
/// `PATH_MAX`. All identity data lands in stack or inline storage.
const NAME_CAP: usize = 32;
const HOST_CAP: usize = 64;
const HOME_CAP: usize = 128;
/// `name@host:cwd<suffix> ` with a bounded cwd abbreviation.
const PROMPT_CAP: usize = 160;
/// Working buffer for the reentrant passwd lookup, grown on `ERANGE`.
const PW_BUF_CAP: usize = 4096;

/// Identity of the account that shell commands actually run under.
///
/// Every field is fixed-size inline storage; the accessors never allocate and
/// the struct is only ever built once (see [`session`]).
pub struct Session {
    name: [u8; NAME_CAP],
    name_len: usize,
    home: [u8; HOME_CAP],
    home_len: usize,
    host: [u8; HOST_CAP],
    host_len: usize,
    prompt: [u8; PROMPT_CAP],
    prompt_len: usize,
    uid: libc::uid_t,
    gid: libc::gid_t,
}

impl Session {
    fn probe() -> Self {
        let (uid, gid) = if is_root_process() {
            (SESSION_UID, SESSION_GID)
        } else {
            (unsafe { libc::getuid() }, unsafe { libc::getgid() })
        };

        let mut name = [0u8; NAME_CAP];
        let mut home = [0u8; HOME_CAP];
        let (name_len, home_len) = match passwd_fields(uid, &mut name, &mut home) {
            Some(lens) => lens,
            // No NSS entry: report the real uid rather than inventing a name.
            None => (
                copy_uid_name(uid, &mut name),
                copy_fallback_home(uid, &mut home),
            ),
        };

        let mut host = [0u8; HOST_CAP];
        let host_len = {
            let n = read_hostname(&mut host);
            if n == 0 {
                host[..7].copy_from_slice(b"localhost");
                7
            } else {
                n
            }
        };

        // Commands always start in the session home, so `~` is truthful, and
        // the `#`/`$` marker reflects the uid the child will run as.
        let mut prompt = [0u8; PROMPT_CAP];
        let prompt_len = build_prompt(&mut prompt, &name[..name_len], &host[..host_len], uid);

        Self {
            name,
            name_len,
            home,
            home_len,
            host,
            host_len,
            prompt,
            prompt_len,
            uid,
            gid,
        }
    }

    /// Login name, e.g. `user` (or the numeric uid when NSS has no entry).
    pub fn name(&self) -> &str {
        text(&self.name[..self.name_len], "user")
    }

    /// Home directory the session is chdir'd into and `HOME` is set to.
    pub fn home(&self) -> &str {
        text(&self.home[..self.home_len], SESSION_HOME)
    }

    /// Kernel hostname (`uname().nodename`).
    pub fn host(&self) -> &str {
        text(&self.host[..self.host_len], "localhost")
    }

    /// Full prompt prefix including the trailing space, e.g. `user@phone:~$ `.
    pub fn prompt(&self) -> &str {
        text(&self.prompt[..self.prompt_len], "user@localhost:~$ ")
    }

    /// UID that shell commands run as (root is dropped to [`SESSION_UID`]).
    pub fn uid(&self) -> libc::uid_t {
        self.uid
    }

    /// Primary GID that shell commands run as.
    pub fn gid(&self) -> libc::gid_t {
        self.gid
    }

    /// True when the session itself is privileged. This is *not* the same as
    /// [`is_root_process`], which reports the compositor/UTIM's own identity.
    pub fn is_root(&self) -> bool {
        self.uid == 0
    }
}

/// Cached, process-wide session identity. Cheap after the first call: one
/// `geteuid`, one `getpwuid_r` and one `uname` for the lifetime of the process.
pub fn session() -> &'static Session {
    static IDENTITY: OnceLock<Session> = OnceLock::new();
    IDENTITY.get_or_init(Session::probe)
}

/// Whether *this* process (UTIM / UTLC) is privileged. Distinct from
/// [`Session::is_root`], which describes the account shell commands run under.
pub fn is_root_process() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Hand a child process the session identity: uid, gid and the supplementary
/// group set. Returns `true` when root privileges were dropped, `false` when
/// the caller already runs unprivileged (in which case `cmd` is untouched).
pub fn drop_privileges(cmd: &mut Command) -> bool {
    if !is_root_process() {
        return false;
    }
    cmd.uid(SESSION_UID).gid(SESSION_GID);
    // Supplementary groups can only be set between fork and exec, which is
    // exactly where pre_exec runs, and only while still privileged.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setgroups(SESSION_GROUPS.len(), SESSION_GROUPS.as_ptr()) != 0 {
                // Fail the spawn rather than exec with unknown groups.
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    true
}

/// Create the session home and runtime directories and hand them to the
/// session account. A no-op when unprivileged, where the directories are
/// expected to be owned by the caller already.
pub fn ensure_session_dirs() {
    if !is_root_process() {
        return;
    }
    for (path, mode) in [(SESSION_HOME, 0o755u32), (SESSION_RUNTIME_DIR, 0o700)] {
        if let Err(e) = fs::create_dir_all(path) {
            eprintln!("session: create_dir_all({path}): {e}");
            continue;
        }
        let Ok(c) = CString::new(path) else { continue };
        unsafe {
            if libc::chown(c.as_ptr(), SESSION_UID, SESSION_GID) != 0 {
                eprintln!(
                    "session: chown({path}): {}",
                    std::io::Error::last_os_error()
                );
            }
            if libc::chmod(c.as_ptr(), mode) != 0 {
                eprintln!(
                    "session: chmod({path}): {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

/// Copy printable ASCII into `dst`, dropping control bytes so the result is
/// always valid UTF-8 and cannot smuggle escape sequences into the prompt.
/// Returns the number of bytes written.
fn copy_sanitized(dst: &mut [u8], src: &[u8]) -> usize {
    let mut n = 0;
    for &c in src {
        if n == dst.len() {
            break;
        }
        if c.is_ascii_graphic() || c == b' ' {
            dst[n] = c;
            n += 1;
        }
    }
    n
}

/// Append `src` to `dst` at `at`, saturating at the capacity. Returns the new
/// length so calls can be chained.
fn put(dst: &mut [u8], at: usize, src: &[u8]) -> usize {
    let n = src.len().min(dst.len().saturating_sub(at));
    dst[at..at + n].copy_from_slice(&src[..n]);
    at + n
}

/// Render `name@host:~<marker> ` into `dst` and return the length written.
///
/// `~` is hardcoded because shell commands are always started in the session
/// home, and the `#`/`$` marker follows the uid the command will run as, not
/// the uid of the compositor that spawned it. Saturated writes keep a
/// pathological NSS name from overflowing the buffer.
fn build_prompt(dst: &mut [u8], name: &[u8], host: &[u8], uid: libc::uid_t) -> usize {
    let mut len = 0;
    len = put(dst, len, name);
    len = put(dst, len, b"@");
    len = put(dst, len, host);
    len = put(dst, len, b":~");
    put(dst, len, if uid == 0 { b"# " } else { b"$ " })
}

/// Borrow a sanitized buffer as `str`, falling back when it is empty or not
/// valid UTF-8. Sanitizing already guarantees ASCII, so the fallback is
/// unreachable in practice and exists to keep the render path panic-free.
fn text<'a>(bytes: &'a [u8], fallback: &'static str) -> &'a str {
    match core::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() => s,
        _ => fallback,
    }
}

/// Copy a NUL-terminated C string into `dst` with the same ASCII filter.
///
/// Truncates rather than failing when the string is longer than `dst`; with
/// `LOGIN_NAME_MAX`-sized bounds that cannot happen for a login name, and a
/// shortened path is still more useful than discarding the whole entry.
unsafe fn copy_cstr(dst: &mut [u8], src: *const libc::c_char) -> usize {
    if src.is_null() {
        return 0;
    }
    let mut len = 0usize;
    while len < dst.len() && unsafe { *src.add(len) } != 0 {
        len += 1;
    }
    let raw = unsafe { std::slice::from_raw_parts(src.cast::<u8>(), len) };
    copy_sanitized(dst, raw)
}

/// Resolve `pw_name`/`pw_dir` for `uid` through the reentrant POSIX entry
/// point. Returns the written lengths, or `None` when the uid is unknown to
/// NSS or the entry carries no usable login name.
fn passwd_fields(uid: libc::uid_t, name: &mut [u8], home: &mut [u8]) -> Option<(usize, usize)> {
    // sysconf(_SC_GETPW_R_SIZE_MAX) is a hint, not a guarantee, and querying it
    // would add a failure mode: grow the stack buffer on ERANGE instead, and
    // give up rather than spin if even the largest buffer is refused.
    let mut buf = [0u8; PW_BUF_CAP];
    let mut cap = 256usize.min(buf.len());
    loop {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pw,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                cap,
                &mut result,
            )
        };
        if rc == 0 && !result.is_null() {
            // `*mut c_char` coerces to `*const c_char`; on aarch64 `c_char` is
            // `u8`, so the copy stays byte-exact.
            let name_len = unsafe { copy_cstr(name, pw.pw_name) };
            if name_len == 0 {
                return None;
            }
            let home_len = unsafe { copy_cstr(home, pw.pw_dir) };
            return Some((name_len, home_len));
        }
        if rc == libc::ERANGE && cap < buf.len() {
            cap = (cap * 4).min(buf.len());
            continue;
        }
        return None;
    }
}

/// Fallback login name when NSS has no entry for `uid`.
///
/// `$USER`/`$LOGNAME` are deliberately ignored: the unit file sets them to
/// `root` for the compositor itself, so trusting them would reintroduce the
/// very lie this module exists to remove. The numeric uid is ugly but true.
fn copy_uid_name(uid: libc::uid_t, dst: &mut [u8]) -> usize {
    copy_sanitized(dst, uid.to_string().as_bytes())
}

/// Fallback home directory when NSS has no entry for `uid`.
fn copy_fallback_home(uid: libc::uid_t, dst: &mut [u8]) -> usize {
    let fallback = if uid == 0 { "/root" } else { SESSION_HOME };
    copy_sanitized(dst, fallback.as_bytes())
}

/// Read `uname().nodename`: one syscall, no truncation guessing, no allocation.
fn read_hostname(dst: &mut [u8]) -> usize {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return 0;
    }
    unsafe { copy_cstr(dst, uts.nodename.as_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_matches_identity() {
        let s = session();
        assert!(!s.name().is_empty());
        assert!(s.home().starts_with('/'));
        assert!(!s.host().is_empty());

        let expected = format!(
            "{}@{}:~{} ",
            s.name(),
            s.host(),
            if s.is_root() { "#" } else { "$" }
        );
        assert_eq!(s.prompt(), expected);
        assert!(s.prompt().ends_with(' '));
    }

    #[test]
    fn identity_fields_are_printable_ascii() {
        let s = session();
        for field in [s.name(), s.home(), s.host(), s.prompt()] {
            assert!(field.is_ascii(), "{field:?} is not ASCII");
            assert!(!field.contains('\n') && !field.contains('\r'));
        }
    }

    #[test]
    fn drop_privileges_is_a_noop_when_unprivileged() {
        let mut cmd = Command::new("/bin/true");
        let dropped = drop_privileges(&mut cmd);
        assert_eq!(dropped, is_root_process());
    }

    #[test]
    fn sanitizer_strips_escape_sequences() {
        let mut dst = [0u8; 16];
        let n = copy_sanitized(&mut dst, b"a\nb\x1b[31mc");
        assert_eq!(&dst[..n], b"ab[31mc");
    }

    #[test]
    fn uid_fallback_never_borrows_the_compositor_user() {
        // utlc.service exports USER=root; a missing passwd entry must not
        // resurrect that name for a session that runs as another uid.
        let mut dst = [0u8; NAME_CAP];
        let n = copy_uid_name(1000, &mut dst);
        assert_eq!(&dst[..n], b"1000");
    }

    /// The root-side prompt can never be observed from an unprivileged CI
    /// runner, so pin both markers explicitly.
    #[test]
    fn prompt_marker_follows_the_session_uid() {
        let mut dst = [0u8; PROMPT_CAP];
        let n = build_prompt(&mut dst, b"user", b"treble-qsi", 1000);
        assert_eq!(&dst[..n], b"user@treble-qsi:~$ ");

        let n = build_prompt(&mut dst, b"root", b"treble-qsi", 0);
        assert_eq!(&dst[..n], b"root@treble-qsi:~# ");
    }

    #[test]
    fn prompt_saturates_instead_of_overflowing() {
        let mut dst = [0u8; 12];
        let long_name = [b'a'; NAME_CAP];
        let long_host = [b'b'; HOST_CAP];
        let n = build_prompt(&mut dst, &long_name, &long_host, 1000);
        assert!(n <= dst.len());
        assert_eq!(n, dst.len());
    }

    #[test]
    fn ensure_session_dirs_is_a_noop_when_unprivileged() {
        // S16: the privileged branch (create/chown/chmod with diagnostics)
        // cannot run in CI; pin the unprivileged branch to a silent return.
        if !is_root_process() {
            ensure_session_dirs();
        }
    }
}
