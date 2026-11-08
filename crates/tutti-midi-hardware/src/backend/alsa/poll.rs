//! Waiting on a sequencer client and a stop descriptor, and how many read
//! errors in a row a reader puts up with.
//!
//! The input pump and the device watch both read a non-blocking client until it
//! is empty, then block here. The sequencer's descriptors are level-triggered,
//! so an event that arrives between the last read and the wait wakes it.

use std::io;
use std::os::fd::RawFd;
use std::os::raw::{c_short, c_uint};
use std::time::Duration;

use super::client::{SeqClient, check};
use crate::error::Result;
use tutti_alsa_ump_sys as sys;

/// Read errors in a row a reader puts up with before it stops.
pub(super) const MAX_DRAIN_ERRORS: usize = 8;

/// Read errors in a row, which end a reader at [`MAX_DRAIN_ERRORS`]. A read that
/// succeeds starts the count again, so scattered overruns in a long burst do
/// not end the reader with events unread.
#[derive(Debug, Default)]
pub(super) struct ErrorRun(usize);

impl ErrorRun {
    /// Counts an error; true when the run has reached the limit.
    pub(super) fn failed(&mut self) -> bool {
        self.0 += 1;
        self.0 >= MAX_DRAIN_ERRORS
    }

    /// A read succeeded.
    pub(super) fn read(&mut self) {
        self.0 = 0;
    }
}

/// Why [`PollSet::wait`] returned.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Wait {
    /// The sequencer has input.
    Ready,
    /// The time given to `wait` passed with nothing ready.
    Timeout,
    /// The stop descriptor is readable or closed.
    Stopped,
    /// A sequencer descriptor reports an error or hang-up.
    Broken,
}

/// The sequencer client's descriptors, then the stop descriptor.
pub(super) struct PollSet {
    polls: Vec<libc::pollfd>,
}

impl PollSet {
    /// The descriptors of `seq` for input, and `stop`, which the owner of the
    /// other end closes to wake the waiter. `stop` must outlive the set.
    pub(super) fn new(seq: &SeqClient, stop: RawFd) -> Result<Self> {
        let events = libc::POLLIN as c_short;
        // SAFETY: `seq` is open.
        let count = check(
            unsafe { sys::snd_seq_poll_descriptors_count(seq.raw(), events) },
            "snd_seq_poll_descriptors_count",
        )?;
        let mut polls = vec![
            libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0
            };
            count as usize
        ];
        // SAFETY: `polls` is `count` `pollfd`s, and alsa-lib writes at most
        // `space` of them.
        check(
            unsafe {
                sys::snd_seq_poll_descriptors(
                    seq.raw(),
                    polls.as_mut_ptr().cast(),
                    count as c_uint,
                    events,
                )
            },
            "snd_seq_poll_descriptors",
        )?;
        polls.push(libc::pollfd {
            fd: stop,
            events,
            revents: 0,
        });
        Ok(Self { polls })
    }

    /// Blocks until the sequencer has input, the stop descriptor fires, or
    /// `timeout` passes (`None` waits for ever).
    pub(super) fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Wait> {
        let timeout_ms = timeout.map_or(-1, |t| {
            i32::try_from(t.as_millis().max(1)).unwrap_or(i32::MAX)
        });
        loop {
            for p in &mut self.polls {
                p.revents = 0;
            }
            // SAFETY: `polls` is a live array of `polls.len()` `pollfd`s.
            let ready = unsafe {
                libc::poll(
                    self.polls.as_mut_ptr(),
                    self.polls.len() as libc::nfds_t,
                    timeout_ms,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(Wait::Timeout);
            }
            let (stop, seq) = self.polls.split_last().expect("the stop descriptor");
            if stop.revents != 0 {
                return Ok(Wait::Stopped);
            }
            let broken = (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) as c_short;
            if seq.iter().any(|p| p.revents & broken != 0) {
                return Ok(Wait::Broken);
            }
            return Ok(Wait::Ready);
        }
    }
}
