//! [`Bell`]: how a plugin's call on any thread makes the main loop run now.
//!
//! The main loop sleeps in one wait over the control socket, the plugin's own
//! descriptors and the next tick. A plugin's `request_process` may be called
//! from its audio thread, which can neither take a lock nor write the control
//! socket, yet its answer (`ProcessRequested`) must leave the server at once,
//! not on the next tick. [`Bell::ring`] sets a flag and, on unix, writes one
//! byte to a non-blocking socket pair whose read end the wait includes
//! ([`Bell::fd`]): it takes no lock, allocates nothing and never waits. Windows
//! has no such descriptor; its wait naps in slices of a millisecond and checks
//! [`Bell::rung`] between them.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The process's bell. See the module docs.
pub(crate) struct Bell {
    rung: AtomicBool,
    #[cfg(unix)]
    tx: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    rx: std::os::unix::net::UnixStream,
}

/// The one bell of this server process: one plugin is served, and the plugin's
/// host callbacks reach it through no other path.
pub(crate) fn bell() -> &'static Bell {
    static BELL: OnceLock<Bell> = OnceLock::new();
    BELL.get_or_init(Bell::new)
}

impl Bell {
    fn new() -> Self {
        #[cfg(unix)]
        {
            let (tx, rx) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
            tx.set_nonblocking(true).expect("a non-blocking bell");
            rx.set_nonblocking(true).expect("a non-blocking bell");
            Self {
                rung: AtomicBool::new(false),
                tx,
                rx,
            }
        }
        #[cfg(not(unix))]
        Self {
            rung: AtomicBool::new(false),
        }
    }

    /// Makes the main loop's wait end. Any thread, the audio thread included.
    pub(crate) fn ring(&self) {
        self.rung.store(true, Ordering::Release);
        #[cfg(unix)]
        {
            use std::io::Write;
            // A full socket already holds a byte the loop has not read: the
            // wait ends either way.
            let _ = (&self.tx).write(&[1]);
        }
    }

    /// Whether the bell has rung since the last [`take`](Self::take).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn rung(&self) -> bool {
        self.rung.load(Ordering::Acquire)
    }

    /// Whether it rang since the last call, clearing it. The main loop's.
    pub(crate) fn take(&self) -> bool {
        self.rung.swap(false, Ordering::AcqRel)
    }

    /// The descriptor the main loop's wait watches for readability.
    #[cfg(unix)]
    pub(crate) fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.rx.as_raw_fd()
    }

    /// Reads the bytes [`ring`](Self::ring) wrote, so the descriptor is quiet
    /// until the next ring.
    #[cfg(unix)]
    pub(crate) fn drain(&self) {
        use std::io::Read;
        let mut scratch = [0u8; 64];
        while matches!((&self.rx).read(&mut scratch), Ok(n) if n > 0) {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// **A ring ends a wait on the bell's descriptor at once, and a drain
    /// quiets it**: the wait the main loop makes returns on a ring from
    /// another thread, and not again until the next.
    #[test]
    fn a_ring_from_another_thread_ends_a_wait_on_the_descriptor() {
        use std::os::fd::BorrowedFd;
        let bell = bell();
        bell.drain();
        let _ = bell.take();
        // SAFETY: the descriptor is the bell's own, open for the process's life.
        let fd = unsafe { BorrowedFd::borrow_raw(bell.fd()) };
        assert!(!tutti_plugin_ipc_ffi::wait_readable(fd, 0).expect("poll"));
        std::thread::spawn(|| bell().ring()).join().expect("ringer");
        assert!(tutti_plugin_ipc_ffi::wait_readable(fd, 1000).expect("poll"));
        assert!(bell.rung());
        bell.drain();
        assert!(!tutti_plugin_ipc_ffi::wait_readable(fd, 0).expect("poll"));
        assert!(bell.take());
        assert!(!bell.take());
    }
}
