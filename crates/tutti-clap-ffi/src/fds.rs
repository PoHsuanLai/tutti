//! Which of a set of file descriptors a plugin asked to watch are ready
//! (`clap.posix-fd-support`), by a zero-timeout `poll(2)`.

use clap_sys::ext::posix_fd_support::{
    CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
};

/// Scratch for repeated polls, so a poll allocates only when the watch set
/// grows.
#[derive(Debug, Default)]
pub struct FdPoller {
    polls: Vec<libc::pollfd>,
}

impl FdPoller {
    /// Polls `watched` (descriptor, `CLAP_POSIX_FD_*` interest) and writes the
    /// ready ones with the readiness they asked for into `ready`, replacing
    /// its contents.
    pub fn poll(&mut self, watched: &[(i32, u32)], ready: &mut Vec<(i32, u32)>) {
        ready.clear();
        self.polls.clear();
        if watched.is_empty() {
            return;
        }
        self.polls.extend(watched.iter().map(|&(fd, flags)| {
            let mut events = 0;
            if flags & CLAP_POSIX_FD_READ != 0 {
                events |= libc::POLLIN;
            }
            if flags & CLAP_POSIX_FD_WRITE != 0 {
                events |= libc::POLLOUT;
            }
            libc::pollfd {
                fd,
                events,
                revents: 0,
            }
        }));
        // SAFETY: `polls` is a live, initialised array of `polls.len()`
        // `pollfd`s that `poll` reads and writes only within that length; a
        // zero timeout never blocks. A descriptor that is not open answers
        // `POLLNVAL` rather than faulting.
        let n = unsafe { libc::poll(self.polls.as_mut_ptr(), self.polls.len() as libc::nfds_t, 0) };
        if n <= 0 {
            return;
        }
        ready.extend(
            self.polls
                .iter()
                .zip(watched.iter())
                .filter_map(|(poll, &(fd, flags))| {
                    let mut now = 0;
                    if poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                        now |= CLAP_POSIX_FD_READ;
                    }
                    if poll.revents & libc::POLLOUT != 0 {
                        now |= CLAP_POSIX_FD_WRITE;
                    }
                    if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        now |= CLAP_POSIX_FD_ERROR;
                    }
                    let now = now & flags;
                    (now != 0).then_some((fd, now))
                }),
        );
    }
}

#[cfg(all(test, unix))]
mod ready_fd_tests {
    use super::FdPoller;
    use clap_sys::ext::posix_fd_support::{
        CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
    };

    fn ready_fds(watched: &[(i32, u32)]) -> Vec<(i32, u32)> {
        let mut ready = Vec::new();
        FdPoller::default().poll(watched, &mut ready);
        ready
    }

    fn pipe() -> (i32, i32) {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a writable array of two descriptors, as `pipe(2)` takes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    /// Only a descriptor that is ready is reported, and only with the events
    /// it asked for. A host that reports every registered descriptor on every
    /// call wakes the plugin for nothing, and one that reports `READ` for a
    /// descriptor watched for `WRITE` hands it an event it never asked for.
    ///
    /// Mutations (run): report every watched descriptor (`Some((fd, flags))` for
    /// all) → the first assertion fails. Drop `& flags` → the `WRITE`-only
    /// assertion fails. Map `POLLHUP` to nothing → the hang-up assertion fails.
    #[test]
    fn only_ready_descriptors_are_reported_with_the_events_asked_for() {
        let (r, w) = pipe();
        let all = CLAP_POSIX_FD_READ | CLAP_POSIX_FD_WRITE | CLAP_POSIX_FD_ERROR;

        // Nothing written: the read end is idle, the write end is writable.
        assert_eq!(ready_fds(&[(r, all)]), []);
        assert_eq!(ready_fds(&[(w, all)]), [(w, CLAP_POSIX_FD_WRITE)]);

        // Data waiting: the read end is readable, and a `WRITE`-only watch of
        // it hears nothing.
        // SAFETY: `w` is the open write end and the buffer is one readable byte.
        assert_eq!(unsafe { libc::write(w, [1u8].as_ptr().cast(), 1) }, 1);
        assert_eq!(ready_fds(&[(r, all)]), [(r, CLAP_POSIX_FD_READ)]);
        assert_eq!(ready_fds(&[(r, CLAP_POSIX_FD_WRITE)]), []);

        // Drained, then the write end closed: the read end hangs up, which
        // reads as ready (the read returns end of file).
        let mut byte = 0u8;
        assert_eq!(
            // SAFETY: `r` is the open read end and the buffer is one writable byte.
            unsafe { libc::read(r, (&mut byte as *mut u8).cast(), 1) },
            1
        );
        assert_eq!(ready_fds(&[(r, CLAP_POSIX_FD_READ)]), []);
        // SAFETY: closes the descriptor this test opened, once.
        unsafe { libc::close(w) };
        assert_eq!(ready_fds(&[(r, CLAP_POSIX_FD_READ)]).len(), 1);
        // SAFETY: as above.
        unsafe { libc::close(r) };
        assert_eq!(ready_fds(&[]), []);
    }
}
