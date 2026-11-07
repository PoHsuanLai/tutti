//! Watching the sequencer for devices that come and go.
//!
//! The kernel's `System:Announce` port sends an event whenever a client or port
//! starts, exits or changes. A thread subscribes to it and blocks in `poll` on
//! the sequencer's descriptors and a descriptor of its own that wakes it for
//! shutdown. On an event it enumerates again and reports the difference, so it
//! reads from the event only which client or port left, so a handle the kernel
//! hands to a new device is told from the one it had. A burst of events is one
//! enumeration, and an event it misses is covered by the next.
//!
//! Enumeration runs on the watch's own client. A client opened to enumerate would
//! announce its own start, port and exit to this very port, and the watch would
//! wake itself for ever.

use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_short, c_uint};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;

use super::client::{SeqClient, check};
use super::enumerate::unpack_id;
use crate::devices::{ChangeSink, Reporter};
use crate::endpoints::Watch;
use crate::error::{Error, Result};
use tutti_alsa_ump_sys as sys;
use tutti_midi_device::{EndpointId, OsDevice};

/// Announcements the watch thread has acted on, for the idle test.
#[cfg(test)]
pub(super) static WAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const EAGAIN: i32 = -11;
/// Read errors in a row a drain puts up with before it stops.
const MAX_DRAIN_ERRORS: usize = 8;
/// Linux truncates a thread name to 15 bytes.
const THREAD_NAME: &str = "tutti-devwatch";

/// A subscription to the announce port, and the descriptors to wait on.
struct Announce {
    seq: SeqClient,
    /// The sequencer's descriptors, then the stop descriptor.
    polls: Vec<libc::pollfd>,
    /// The read end of the stop pair; the owner drops the write end to wake us.
    _stop: UnixStream,
}

/// Read errors in a row, which end a drain at [`MAX_DRAIN_ERRORS`]. A read that
/// succeeds starts the count again, so scattered overruns in a long burst do
/// not end the drain with events unread.
#[derive(Debug, Default)]
struct ErrorRun(usize);

impl ErrorRun {
    /// Counts an error; true when the run has reached the limit.
    fn failed(&mut self) -> bool {
        self.0 += 1;
        self.0 >= MAX_DRAIN_ERRORS
    }

    /// A read succeeded.
    fn read(&mut self) {
        self.0 = 0;
    }
}

/// Something that left the sequencer, from an announcement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Departed {
    Client(c_int),
    Port(c_int, c_int),
}

impl Departed {
    /// Whether the handle `id` (a packed client and port) named what left.
    fn names(self, id: EndpointId) -> bool {
        let (client, port) = unpack_id(id);
        match self {
            Self::Client(c) => c == client,
            Self::Port(c, p) => (c, p) == (client, port),
        }
    }
}

/// Why `Announce::wait` returned.
enum Woken {
    /// The sequencer announced something; these left it.
    Announced(Vec<Departed>),
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
            return Ok(Woken::Announced(self.drain()));
        }
    }

    /// Reads every queued event, returning what left; the client is
    /// non-blocking.
    fn drain(&self) -> Vec<Departed> {
        let mut departed = Vec::new();
        let mut errors = ErrorRun::default();
        loop {
            let mut event: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
            // SAFETY: `seq` is open; the event is alsa-lib's, read before the
            // next call.
            let rc = unsafe { sys::snd_seq_ump_event_input(self.seq.raw(), &mut event) };
            if rc < 0 {
                if rc == EAGAIN {
                    return departed;
                }
                // An overrun drops announcements; the read goes on past it, and
                // the enumeration that follows every drain finds what changed
                // (a reused number whose exit was dropped is the one thing it
                // cannot). A descriptor that keeps failing ends the drain.
                tracing::debug!("ALSA announce: {}", super::client::strerror(rc));
                if errors.failed() {
                    return departed;
                }
                continue;
            }
            errors.read();
            if event.is_null() {
                continue;
            }
            // SAFETY: non-null, and valid until the next `input`; copied out.
            let (kind, data) = unsafe { ((*event).type_, (*event).ump[0].to_ne_bytes()) };
            let (client, port) = (c_int::from(data[0]), c_int::from(data[1]));
            match kind {
                sys::SND_SEQ_EVENT_CLIENT_EXIT => departed.push(Departed::Client(client)),
                sys::SND_SEQ_EVENT_PORT_EXIT => departed.push(Departed::Port(client, port)),
                _ => {}
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
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the ALSA device watch thread panicked");
        }
    }
}

/// Subscribes to the announce port, reports `snapshot` once, then reports it
/// again after each announcement until the returned watch drops. `snapshot`
/// enumerates on the client it is given: the watch's own.
///
/// Subscribing comes first, so a device that appears during the first
/// enumeration is announced too. A sequencer that will not open starts no
/// thread: there are no devices to watch, as enumeration reports. A watch that
/// later fails, by an error or a panic, tells `sink` ([`ChangeSink::fail`]).
pub(super) fn start(
    sink: ChangeSink,
    snapshot: impl Fn(&SeqClient) -> Vec<OsDevice> + Send + 'static,
) -> Result<Box<dyn Watch>> {
    let (stop_tx, stop_rx) = UnixStream::pair()?;
    let mut announce = match Announce::open(stop_rx) {
        Ok(announce) => announce,
        Err(e) => {
            tracing::debug!("ALSA device watch: {e}");
            return Ok(Box::new(crate::endpoints::NoWatch));
        }
    };
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    reporter.seed(snapshot(&announce.seq));
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match announce.wait() {
                        Ok(Woken::Announced(departed)) => {
                            #[cfg(test)]
                            WAKES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            reporter.report(snapshot(&announce.seq), |id| {
                                departed.iter().any(|d| d.names(id))
                            });
                        }
                        Ok(Woken::Stopped) => return None,
                        Err(e) => return Some(e),
                    }
                }
            }));
            match run {
                Ok(None) => {}
                Ok(Some(why)) => failures.fail(why),
                Err(_) => failures.fail(crate::WatchFailure::Panicked),
            }
        })?;
    Ok(Box::new(AlsaWatch {
        stop: Some(stop_tx),
        thread: Some(thread),
    }))
}

#[cfg(test)]
mod tests;
