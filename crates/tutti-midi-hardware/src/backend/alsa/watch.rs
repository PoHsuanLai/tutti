//! Watching the sequencer for devices that come and go.
//!
//! The kernel's `System:Announce` port sends an event whenever a client or port
//! starts, exits or changes. A thread subscribes to it and blocks in `poll` on
//! the sequencer's descriptors and a descriptor of its own that wakes it for
//! shutdown. On an event it enumerates again and reports the difference. A
//! burst of events is one enumeration.
//!
//! What the events add is which handle the kernel gave to a *new* client. The
//! kernel frees a client number and posts every `PORT_EXIT` of the old owner
//! under one lock before the number can be taken again (`seq_free_client1`,
//! `snd_seq_delete_all_ports`), and posts the new owner's `CLIENT_START` before
//! it can create a port or set a name (`snd_seq_open`). Only the old owner's
//! `CLIENT_EXIT` can come late. So the watch reads the queue again after each
//! enumeration and enumerates again while it finds events (`settle`): a quiet
//! read proves every start queued before the enumeration has been read. A start
//! read for a handle the watch has already reported is then a new occupant of
//! that number, and the handle is reported removed and added even when the new
//! device is identical. Exits and changes are only events seen: an exit says
//! nothing about the occupant the enumeration shows, and a rename is an
//! identity change the enumeration shows by itself.
//!
//! Enumeration runs on the watch's own client. A client opened to enumerate would
//! announce its own start, port and exit to this very port, and the watch would
//! wake itself for ever.

use std::collections::BTreeSet;
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_short, c_uint};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;

use super::client::{SeqClient, check};
use super::enumerate::{pack_id, unpack_id};
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
/// Looks a settle takes before it reports the last one.
const MAX_SETTLE: usize = 8;
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

/// An announcement the watch reads, with the client (and port) it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    ClientStart(c_int),
    ClientExit(c_int),
    ClientChange(c_int),
    PortStart(c_int, c_int),
    PortExit(c_int, c_int),
    PortChange(c_int, c_int),
}

/// What one read of the announce queue found.
#[derive(Debug, Default)]
struct Drained {
    /// The announcements, in the order the kernel posted them.
    events: Vec<Event>,
    /// Whether the queue held any event at all, or a read failed: a failed read
    /// is not a quiet one (an overrun, `-ENOSPC`, has lost announcements).
    any: bool,
}

impl Drained {
    /// Records a failed read other than `-EAGAIN`.
    fn failed(&mut self) {
        self.any = true;
    }
}

/// What a settle found.
#[derive(Debug)]
struct Settled<T> {
    /// The last look, if one succeeded.
    seen: Option<T>,
    /// Every announcement read on the way.
    events: Vec<Event>,
    /// Whether the last look was followed by a read that found nothing.
    quiet: bool,
}

/// Looks at the sequencer until a look is followed by a read that finds
/// nothing, and returns that look with every announcement read on the way.
///
/// A read that follows a look and finds nothing proves no event was queued
/// before the look, so the look and the announcements read before it agree.
/// A read that finds events looks again, up to [`MAX_SETTLE`] times: a
/// sequencer that never quiets reports the last look, not quiet. A look that
/// fails (`None`: the enumeration was cut short) is not a look, and is retried
/// the same way.
fn settle<T>(
    first: Drained,
    mut drain: impl FnMut() -> Drained,
    look: impl Fn() -> Option<T>,
) -> Settled<T> {
    let mut events = first.events;
    let mut seen = look();
    let mut quiet = false;
    for _ in 0..MAX_SETTLE {
        if seen.is_some() {
            let more = drain();
            if !more.any {
                quiet = true;
                break;
            }
            events.extend(more.events);
        }
        seen = look();
    }
    if !quiet {
        tracing::debug!("ALSA announce did not quiet in {MAX_SETTLE} looks");
    }
    Settled {
        seen,
        events,
        quiet,
    }
}

/// What the watch keeps between wakes: the reporter, and what it needs to tell
/// a new occupant of a number from the one already reported.
#[derive(Debug)]
struct Tracker {
    reporter: Reporter,
    /// The handles of the last look reported.
    last: BTreeSet<EndpointId>,
    /// Handles whose own start may not have been read yet (the last settle did
    /// not quiet): the first start read for one confirms it and is no new
    /// occupant.
    unconfirmed: BTreeSet<EndpointId>,
    /// Announcements read while no look succeeded; they precede the next look.
    pending: Vec<Event>,
}

impl Tracker {
    fn new(reporter: Reporter) -> Self {
        Self {
            reporter,
            last: BTreeSet::new(),
            unconfirmed: BTreeSet::new(),
            pending: Vec::new(),
        }
    }

    /// Lists what is there as already present. The announcements read until a
    /// quiet read are those of the devices it lists (their starts), so none is
    /// held for later.
    fn seed(&mut self, drain: impl FnMut() -> Drained, look: impl Fn() -> Option<Vec<OsDevice>>) {
        let settled = settle(Drained::default(), drain, look);
        let Some(present) = settled.seen else {
            tracing::debug!("ALSA device watch: no complete first enumeration");
            return;
        };
        self.last = present.iter().map(|d| d.handle).collect();
        self.unconfirmed = if settled.quiet {
            BTreeSet::new()
        } else {
            self.last.clone()
        };
        self.reporter.seed(present);
    }

    /// One wake: settles, then reports the look with the handles `first` and
    /// the later reads say have a new occupant.
    fn step(
        &mut self,
        first: Drained,
        drain: impl FnMut() -> Drained,
        look: impl Fn() -> Option<Vec<OsDevice>>,
    ) {
        let Settled {
            seen,
            mut events,
            quiet,
        } = settle(first, drain, look);
        events.splice(0..0, self.pending.drain(..));
        let Some(present) = seen else {
            tracing::debug!("ALSA device watch: no complete enumeration; keeping the last");
            self.pending = events;
            return;
        };
        let replaced = self.replaced(&events);
        self.last = present.iter().map(|d| d.handle).collect();
        self.reporter.report(present, |id| replaced.contains(&id));
        self.unconfirmed = if quiet {
            BTreeSet::new()
        } else {
            self.last.clone()
        };
    }

    /// The reported handles that a start in `events` gives a new occupant.
    ///
    /// A client's `CLIENT_START` names a new owner of its number; a
    /// `PORT_START` after it in the stream is that owner's own port. A
    /// `PORT_START` with no `CLIENT_START` before it names a port made again on
    /// a client that lives. Either is a new occupant of a handle already
    /// reported, unless the handle is unconfirmed, whose first start is its own.
    fn replaced(&self, events: &[Event]) -> BTreeSet<EndpointId> {
        let mut unconfirmed = self.unconfirmed.clone();
        let mut replaced = BTreeSet::new();
        let mut touch = |handle: EndpointId| {
            if !unconfirmed.remove(&handle) {
                replaced.insert(handle);
            }
        };
        let mut fresh = BTreeSet::new();
        let mut created = BTreeSet::new();
        for &event in events {
            match event {
                Event::ClientStart(n) => {
                    fresh.insert(n);
                    created.retain(|&(c, _)| c != n);
                    self.last
                        .iter()
                        .filter(|&&h| unpack_id(h).0 == n)
                        .for_each(|&h| touch(h));
                }
                Event::PortStart(n, p) => {
                    let own = fresh.contains(&n) && created.insert((n, p));
                    let handle = pack_id(n, p);
                    if !own && self.last.contains(&handle) {
                        touch(handle);
                    }
                }
                Event::ClientExit(_)
                | Event::ClientChange(_)
                | Event::PortExit(..)
                | Event::PortChange(..) => {}
            }
        }
        replaced
    }
}

/// Why `Announce::wait` returned.
enum Woken {
    /// The sequencer announced something.
    Announced(Drained),
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

    /// Reads every queued event; the client is non-blocking.
    fn drain(&self) -> Drained {
        let mut found = Drained::default();
        let mut errors = ErrorRun::default();
        loop {
            let mut event: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
            // SAFETY: `seq` is open; the event is alsa-lib's, read before the
            // next call.
            let rc = unsafe { sys::snd_seq_ump_event_input(self.seq.raw(), &mut event) };
            if rc < 0 {
                if rc == EAGAIN {
                    return found;
                }
                // An overrun drops announcements and is not a quiet read: it
                // makes the settle look again. A descriptor that keeps failing
                // ends the drain.
                tracing::debug!("ALSA announce: {}", super::client::strerror(rc));
                found.failed();
                if errors.failed() {
                    return found;
                }
                continue;
            }
            errors.read();
            if event.is_null() {
                continue;
            }
            found.any = true;
            // SAFETY: non-null, and valid until the next `input`; copied out.
            let (kind, data) = unsafe { ((*event).type_, (*event).ump[0].to_ne_bytes()) };
            let (client, port) = (c_int::from(data[0]), c_int::from(data[1]));
            let event = match kind {
                sys::SND_SEQ_EVENT_CLIENT_START => Some(Event::ClientStart(client)),
                sys::SND_SEQ_EVENT_CLIENT_EXIT => Some(Event::ClientExit(client)),
                sys::SND_SEQ_EVENT_CLIENT_CHANGE => Some(Event::ClientChange(client)),
                sys::SND_SEQ_EVENT_PORT_START => Some(Event::PortStart(client, port)),
                sys::SND_SEQ_EVENT_PORT_EXIT => Some(Event::PortExit(client, port)),
                sys::SND_SEQ_EVENT_PORT_CHANGE => Some(Event::PortChange(client, port)),
                _ => None,
            };
            found.events.extend(event);
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
    snapshot: impl Fn(&SeqClient) -> Option<Vec<OsDevice>> + Send + 'static,
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
    let mut tracker = Tracker::new(Reporter::new(sink));
    tracker.seed(|| announce.drain(), || snapshot(&announce.seq));
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match announce.wait() {
                        Ok(Woken::Announced(first)) => {
                            #[cfg(test)]
                            WAKES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            tracker.step(first, || announce.drain(), || snapshot(&announce.seq));
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
