//! The bell on unix: a flag plus a non-blocking socket pair whose read end
//! the main loop's `poll` watches ([`Bell::fd`]). A ring writes one byte, so
//! the wait ends at once.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};

/// The process's bell. See the module docs.
pub(crate) struct Bell {
    rung: AtomicBool,
    tx: UnixStream,
    rx: UnixStream,
}

impl Bell {
    pub(super) fn new() -> Self {
        let (tx, rx) = UnixStream::pair().expect("a socket pair");
        tx.set_nonblocking(true).expect("a non-blocking bell");
        rx.set_nonblocking(true).expect("a non-blocking bell");
        Self {
            rung: AtomicBool::new(false),
            tx,
            rx,
        }
    }

    /// Makes the main loop's wait end. Any thread, the audio thread included.
    pub(crate) fn ring(&self) {
        self.rung.store(true, Ordering::Release);
        // A full socket already holds a byte the loop has not read: the wait
        // ends either way.
        let _ = (&self.tx).write(&[1]);
    }

    /// Whether it rang since the last call, clearing it. The main loop's.
    pub(crate) fn take(&self) -> bool {
        self.rung.swap(false, Ordering::AcqRel)
    }

    /// The descriptor the main loop's wait watches for readability.
    pub(crate) fn fd(&self) -> i32 {
        self.rx.as_raw_fd()
    }

    /// Reads the bytes [`ring`](Self::ring) wrote, so the descriptor is quiet
    /// until the next ring.
    pub(crate) fn drain(&self) {
        let mut scratch = [0u8; 64];
        while matches!((&self.rx).read(&mut scratch), Ok(n) if n > 0) {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::bell;

    /// Whether the bell's descriptor is readable within `ms`.
    fn readable(ms: i32) -> bool {
        use std::os::fd::AsFd;
        tutti_plugin_ipc_ffi::wait_readable(bell().rx.as_fd(), ms).expect("poll")
    }

    /// **A ring ends a wait on the bell's descriptor at once, and a drain
    /// quiets it**: the wait the main loop makes returns on a ring from
    /// another thread, and not again until the next.
    #[test]
    fn a_ring_from_another_thread_ends_a_wait_on_the_descriptor() {
        let b = bell();
        b.drain();
        let _ = b.take();
        assert!(!readable(0));
        std::thread::spawn(|| bell().ring()).join().expect("ringer");
        assert!(readable(1000));
        b.drain();
        assert!(!readable(0));
        assert!(b.take());
        assert!(!b.take());
    }
}
