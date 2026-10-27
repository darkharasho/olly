//! PSI trigger registration and a poll(2) wrapper.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub struct PsiTrigger {
    fd: OwnedFd,
}

impl PsiTrigger {
    /// Registers `spec` (e.g. "some 150000 1000000") on a pressure file. The fd then
    /// polls POLLPRI whenever the stall threshold is crossed. Refuses files that are
    /// not on procfs, so a fixture file is never written to.
    pub fn new(path: &Path, spec: &str) -> io::Result<Self> {
        let mut cpath = path.as_os_str().as_bytes().to_vec();
        cpath.push(0);
        // SAFETY: cpath is NUL-terminated.
        let raw = unsafe {
            libc::open(
                cpath.as_ptr() as *const libc::c_char,
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a freshly opened fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if st.f_type != libc::PROC_SUPER_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a procfs pressure file", path.display()),
            ));
        }
        let mut buf = spec.as_bytes().to_vec();
        buf.push(0);
        // SAFETY: buf is valid for buf.len() bytes.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PsiTrigger { fd })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// poll(2) on `fds`; returns which ones have any revents (including errors).
/// `timeout_ms < 0` waits forever. EINTR counts as "nothing ready".
pub fn wait(fds: &[(RawFd, i16)], timeout_ms: i32) -> io::Result<Vec<bool>> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect();
    // SAFETY: pfds is a valid array of pfds.len() pollfd structs.
    let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            return Ok(vec![false; fds.len()]);
        }
        return Err(e);
    }
    Ok(pfds.iter().map(|p| p.revents != 0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eventfd() -> OwnedFd {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn regular_file_is_rejected_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("memory");
        let text = "some avg10=0.00 avg60=0.00 avg300=0.00 total=1\n";
        std::fs::write(&p, text).unwrap();
        let e = PsiTrigger::new(&p, "some 150000 1000000").err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        assert!(PsiTrigger::new(&d.path().join("missing"), "some 1 1").is_err());
    }

    #[test]
    fn wait_times_out_then_sees_ready_fd() {
        let a = eventfd();
        let b = eventfd();
        let fds = [(a.as_raw_fd(), libc::POLLIN), (b.as_raw_fd(), libc::POLLIN)];
        let t = std::time::Instant::now();
        assert_eq!(wait(&fds, 50).unwrap(), vec![false, false]);
        assert!(t.elapsed().as_millis() >= 40);
        let one = 1u64;
        unsafe { libc::write(b.as_raw_fd(), &one as *const u64 as *const libc::c_void, 8) };
        assert_eq!(wait(&fds, 1000).unwrap(), vec![false, true]);
    }
}
