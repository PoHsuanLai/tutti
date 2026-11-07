//! Watching the sequencer for devices that come and go.
//!
//! The kernel's `System:Announce` port sends an event whenever a client or port
//! starts, exits or changes. A thread subscribes to it and blocks in `poll` on
//! the sequencer's descriptors and a descriptor of its own that wakes it for
//! shutdown. On an event it enumerates again and reports the difference, so it
//! never reads what the event says: a burst of events is one enumeration, and an
//! event it misses is covered by the next.

use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_short, c_uint};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;

use super::client::{SeqClient, check};
use crate::devices::{ChangeSink, Reporter};
use crate::endpoints::Watch;
use crate::error::{Error, Result};
use tutti_alsa_ump_sys as sys;
use tutti_midi_device::OsDevice;

const EAGAIN: i32 = -11;
const THREAD_NAME: &str = "tutti-midi-watch";

/// A subscription to the announce port, and the descriptors to wait on.
struct Announce {
    seq: SeqClient,
    /// The sequencer's descriptors, then the stop descriptor.
    polls: Vec<libc::pollfd>,
    /// The read end of the stop pair; the owner drops the write end to wake us.
    _stop: UnixStream,
}

/// Why `Announce::wait` returned.
enum Woken {
    Announced,
    Stopped,
}

impl Announce {
    fn open(stop: UnixStream) -> Result<Self> {
        let seq = SeqClient::open("tutti-watch", true)?;
        // Write-only and not subscribable: nobody lists this port as a device.
        let port = seq.open_port(
            "tutti-watch",
            sys::SND_SEQ_PORT_CAP_WRITE,
            sys::SND_SEQ_PORT_TYPE_APPLICATION,
        )?;
        // SAFETY: `seq` is open; the arguments are plain integers.
        check(
            unsafe {
                sys::snd_seq_connect_from(
                    seq.raw(),
                    port,
                    sys::SND_SEQ_CLIENT_SYSTEM,
                    sys::SND_SEQ_PORT_SYSTEM_ANNOUNCE,
                )
            },
            "snd_seq_connect_from",
        )?;

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
            fd: stop.as_raw_fd(),
            events,
            revents: 0,
        });
        Ok(Self {
            seq,
            polls,
            _stop: stop,
        })
    }

    /// Blocks until the sequencer announces something or the stop pair closes.
    fn wait(&mut self) -> Result<Woken> {
        loop {
            for p in &mut self.polls {
                p.revents = 0;
            }
            // SAFETY: `polls` is a live array of `polls.len()` `pollfd`s.
            let ready = unsafe {
                libc::poll(
                    self.polls.as_mut_ptr(),
                    self.polls.len() as libc::nfds_t,
                    -1,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            let (stop, seq) = self.polls.split_last().expect("the stop descriptor");
            if stop.revents != 0 {
                return Ok(Woken::Stopped);
            }
            let broken = (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) as c_short;
            if seq.iter().any(|p| p.revents & broken != 0) {
                return Err(Error::MidiPort("the sequencer descriptor failed".into()));
            }
            self.drain();
            return Ok(Woken::Announced);
        }
    }

    /// Reads and discards every queued event; the client is non-blocking.
    fn drain(&self) {
        loop {
            let mut event: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
            // SAFETY: `seq` is open; the event is alsa-lib's, unread.
            let rc = unsafe { sys::snd_seq_ump_event_input(self.seq.raw(), &mut event) };
            if rc < 0 {
                if rc != EAGAIN {
                    tracing::debug!("ALSA announce: {}", super::client::strerror(rc));
                }
                return;
            }
        }
    }
}

/// The running watch. Dropping it closes the stop pair, which wakes the thread,
/// and joins it.
struct AlsaWatch {
    stop: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

impl Watch for AlsaWatch {}

impl Drop for AlsaWatch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Subscribes to the announce port, reports `snapshot()` once, then reports it
/// again after each announcement until the returned watch drops.
///
/// Subscribing comes first, so a device that appears during the first
/// enumeration is announced too. A sequencer that will not open starts no
/// thread: there are no devices to watch, as enumeration reports.
pub(super) fn start(
    sink: ChangeSink,
    snapshot: impl Fn() -> Option<Vec<OsDevice>> + Send + 'static,
) -> Result<Box<dyn Watch>> {
    let (stop_tx, stop_rx) = UnixStream::pair()?;
    let mut announce = match Announce::open(stop_rx) {
        Ok(announce) => announce,
        Err(e) => {
            tracing::debug!("ALSA device watch: {e}");
            return Ok(Box::new(crate::endpoints::NoWatch));
        }
    };
    let mut reporter = Reporter::new(sink);
    if let Some(devices) = snapshot() {
        reporter.seed(devices);
    }
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || {
            loop {
                match announce.wait() {
                    Ok(Woken::Announced) => {
                        if let Some(devices) = snapshot() {
                            reporter.report(devices);
                        }
                    }
                    Ok(Woken::Stopped) => return,
                    Err(e) => {
                        tracing::warn!("ALSA device watch ended: {e}");
                        return;
                    }
                }
            }
        })?;
    Ok(Box::new(AlsaWatch {
        stop: Some(stop_tx),
        thread: Some(thread),
    }))
}
