//! Waiting on a sequencer client and a stop descriptor.
//!
//! A non-blocking client is read until it is empty, then waited on here. The
//! sequencer's descriptors are level-triggered, so an event that arrives between
//! the last read and the wait wakes it.

use std::io;
use std::os::fd::RawFd;
use std::os::raw::{c_short, c_uint};
use std::time::Duration;

use crate::client::Client;
use crate::error::{Result, check};
use crate::sys;

/// Why [`Poller::wait`] returned.
#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    /// The sequencer has input.
    Ready,
    /// The time given to `wait` passed with nothing ready.
    Timeout,
    /// The stop descriptor is readable or closed.
    Stopped,
    /// A sequencer descriptor reports an error or hang-up.
    Broken,
}

/// A client's descriptors, then a stop descriptor.
///
/// It holds copies of the numbers, not the descriptors: the client must outlive
/// it, and so must the stop descriptor (a closed one polls as an error, which
/// reads as [`Wait::Broken`] or [`Wait::Stopped`], never as memory trouble).
#[derive(Debug)]
pub struct Poller {
    polls: Vec<libc::pollfd>,
}

impl Poller {
    /// The descriptors of `client` for input, and `stop`, which the owner of
    /// the other end closes to wake the waiter.
    ///
    /// # Errors
    ///
    /// The code of the descriptor query that failed.
    pub fn new(client: &Client, stop: RawFd) -> Result<Self> {
        let events = libc::POLLIN;
        // SAFETY: `client` is open.
        let rc = unsafe { sys::snd_seq_poll_descriptors_count(client.raw(), events) };
        let count = check(rc, "snd_seq_poll_descriptors_count")?;
        let space = count as usize;
        let mut polls = vec![
            libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0
            };
            space
        ];
        // SAFETY: `polls` is `space` `pollfd`s, which is `count` of them, and
        // alsa-lib writes at most that many.
        let rc = unsafe {
            sys::snd_seq_poll_descriptors(
                client.raw(),
                polls.as_mut_ptr().cast(),
                c_uint::try_from(space).unwrap_or(c_uint::MAX),
                events,
            )
        };
        check(rc, "snd_seq_poll_descriptors")?;
        Ok(Self::over(polls, stop))
    }

    /// The sequencer's `polls`, then `stop` for input: the one place the
    /// stop descriptor goes last, which [`wait`](Self::wait) reads.
    fn over(mut polls: Vec<libc::pollfd>, stop: RawFd) -> Self {
        polls.push(libc::pollfd {
            fd: stop,
            events: libc::POLLIN,
            revents: 0,
        });
        Self { polls }
    }

    /// Blocks until the sequencer has input, the stop descriptor fires, or
    /// `timeout` passes (`None` waits for ever).
    ///
    /// # Errors
    ///
    /// The OS error of `poll`, but an interrupted wait, which is retried.
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Wait> {
        let timeout_ms = timeout_ms(timeout);
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
            // The stop descriptor is the last, pushed in `new`.
            let Some((stop, seq)) = self.polls.split_last() else {
                return Ok(Wait::Broken);
            };
            if stop.revents != 0 {
                return Ok(Wait::Stopped);
            }
            let broken = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            if seq.iter().any(|p| p.revents & broken != 0) {
                return Ok(Wait::Broken);
            }
            return Ok(Wait::Ready);
        }
    }
}

/// `poll`'s timeout: -1 waits for ever; a time is rounded up to a millisecond
/// (0 would not wait at all) and saturates at `i32::MAX`.
fn timeout_ms(timeout: Option<Duration>) -> i32 {
    timeout.map_or(-1, |t| {
        i32::try_from(t.as_millis().max(1)).unwrap_or(i32::MAX)
    })
}

const _: () = {
    // `c_short` is what `pollfd.events` holds; the event masks fit it.
    let _: c_short = libc::POLLIN;
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutations: `max(1)` -> `max(0)` fails the sub-millisecond case; `None`
    /// -> 0 fails the first.
    #[test]
    fn a_timeout_is_whole_milliseconds_and_never_zero() {
        assert_eq!(timeout_ms(None), -1);
        assert_eq!(timeout_ms(Some(Duration::from_micros(10))), 1);
        assert_eq!(timeout_ms(Some(Duration::from_millis(250))), 250);
        assert_eq!(
            timeout_ms(Some(Duration::from_secs(u64::MAX / 2000))),
            i32::MAX
        );
    }

    /// A socket stands in for the sequencer's descriptor, so the wait is
    /// tested without the sequencer (CI's kernel has no `snd-seq`): nothing
    /// readable times out, the stand-in readable is `Ready`, and a readable
    /// stop descriptor is `Stopped` even with input pending.
    ///
    /// Mutations: reporting `seq` input before `stop` -> the last assert
    /// reads `Ready`; `ready == 0` -> `ready <= 1` reads one ready descriptor as a timeout; pushing the
    /// stop descriptor first in `over` -> the stand-in's input reads `Stopped`.
    #[test]
    fn the_wait_reports_input_timeout_and_stop_with_stop_first() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (mut seq_tx, seq_rx) = UnixStream::pair().unwrap();
        let (mut stop_tx, stop_rx) = UnixStream::pair().unwrap();
        let seq = libc::pollfd {
            fd: seq_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let mut poller = Poller::over(vec![seq], stop_rx.as_raw_fd());
        let short = Some(Duration::from_millis(20));
        assert_eq!(poller.wait(short).unwrap(), Wait::Timeout);
        seq_tx.write_all(b"x").unwrap();
        assert_eq!(poller.wait(short).unwrap(), Wait::Ready);
        stop_tx.write_all(b"x").unwrap();
        assert_eq!(poller.wait(None).unwrap(), Wait::Stopped);
    }
}
