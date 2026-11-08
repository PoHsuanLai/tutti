//! Watching the sequencer for devices that come and go.
//!
//! The kernel's `System:Announce` port sends an event whenever a client or port
//! starts, exits or changes. A thread subscribes to it and blocks in `poll` on
//! the sequencer's descriptors and a descriptor of its own that wakes it for
//! shutdown. On an event it enumerates again and reports the difference, so it
//! reads from the event only which client or port left, so a handle the kernel
//! hands to a new device is told from the one it had. A burst of events is one
//! enumeration, and an event it misses is covered by the next. A read of the
//! queue that follows the enumeration and finds events enumerates again
//! (`settle`), so the exits it reports belong to the enumeration they are
//! reported with.
//!
//! Enumeration runs on the watch's own client. A client opened to enumerate would
//! announce its own start, port and exit to this very port, and the watch would
//! wake itself for ever.

use std::collections::{BTreeMap, BTreeSet};
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
/// The input FIFO overran: announcements were lost.
const ENOSPC: i32 = -28;
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

/// A client or port starting or changing, from an announcement. The kernel
/// posts a change when a client sets its info (`snd_seq_ioctl_set_client_info`,
/// which `snd_seq_set_client_name` calls) or a port's (`set_port_info`), and a
/// new client does that right after it starts, so a change after a start
/// belongs to the new owner and a change with no start is a rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Live {
    Started(c_int),
    PortStarted(c_int, c_int),
    Changed(c_int),
    PortChanged(c_int, c_int),
}

fn client_of(d: Departed) -> c_int {
    match d {
        Departed::Client(c) | Departed::Port(c, _) => c,
    }
}

impl Live {
    fn client(self) -> c_int {
        match self {
            Self::Started(c)
            | Self::PortStarted(c, _)
            | Self::Changed(c)
            | Self::PortChanged(c, _) => c,
        }
    }

    fn starts(self) -> bool {
        matches!(self, Self::Started(_) | Self::PortStarted(..))
    }
}

/// What one read of the announce queue found.
#[derive(Debug, Default)]
struct Drained {
    /// What left the sequencer.
    departed: Vec<Departed>,
    /// What started or changed, in the order it was announced.
    lives: Vec<Live>,
    /// Whether the queue held any event at all, or a read failed: a failed read
    /// is not a quiet one.
    any: bool,
    /// Whether the queue overran, so announcements were lost.
    overrun: bool,
}

impl Drained {
    /// Records a failed read other than `-EAGAIN`. The kernel reports an
    /// overrun of the input FIFO as `-ENOSPC` (alsa-lib's `snd_seq_event_input`
    /// documents it; the kernel's `snd_seq_read` clears the FIFO after it).
    fn failed(&mut self, rc: c_int) {
        self.any = true;
        self.overrun |= rc == ENOSPC;
    }
}

/// What a settle found.
#[derive(Debug)]
struct Settled<T> {
    /// The last look.
    seen: T,
    /// Every departure read on the way.
    departed: Vec<Departed>,
    /// Every start and change read on the way.
    lives: Vec<Live>,
    /// Whether any read overran.
    overrun: bool,
}

/// Looks at the sequencer until a look is followed by a read that finds
/// nothing, and returns that look with every departure read on the way.
///
/// An exit read from the queue may already be in a look taken before the read:
/// the kernel queues it, a client takes the number, and the look sees the new
/// client while the exit is still unread. A read that follows a look and finds
/// nothing proves no event was queued before the look, so that look and the
/// departures read before it agree, and a departure names a device the previous
/// settled look held. A read that finds events looks again, up to
/// [`MAX_SETTLE`] times: a sequencer that never quiets reports the last look
/// with what was read. An exit the kernel posts after the final read (it frees
/// the number first) is what [`Owed`] covers.
fn settle<T>(
    first: Drained,
    mut drain: impl FnMut() -> Drained,
    look: impl Fn() -> T,
) -> Settled<T> {
    let mut departed = first.departed;
    let mut lives = first.lives;
    let mut overrun = first.overrun;
    let mut seen = look();
    let mut quiet = false;
    for _ in 0..MAX_SETTLE {
        let more = drain();
        if !more.any {
            quiet = true;
            break;
        }
        departed.extend(more.departed);
        lives.extend(more.lives);
        overrun |= more.overrun;
        seen = look();
    }
    if !quiet {
        tracing::debug!("ALSA announce did not quiet in {MAX_SETTLE} looks");
    }
    Settled {
        seen,
        departed,
        lives,
        overrun,
    }
}

/// What a report left owing for one client number that is not yet known to be a
/// replacement.
#[derive(Debug, Default)]
struct Tentative {
    ports: Vec<(c_int, c_int)>,
    client: bool,
}

/// Exits the kernel owes the watch: it frees a client number and only then
/// posts the exit, so a new client can take the number, and a look can show it,
/// before the old owner's exit is read. When a report finds a number's owner
/// replaced or gone without having read that owner's exits, it records one of
/// each, and the exit that arrives later pays the debt instead of reaching the
/// number's new owner. A replacement that is identical to its predecessor is
/// not seen as one, so its exits are read as the departure they are.
///
/// A look that shows another identity on a number, or none, is a replacement or
/// a rename (or a port whose capabilities changed) of the owner it had, and
/// only the announcements tell which: the new owner's start comes first (and
/// confirms the debt), a rename only has a change, which cancels it, and an
/// exit that arrives pays it. Until one of them is read the debt is tentative.
///
/// Debts are only as good as the announcements: an overrun may have lost the
/// exits they wait for, so [`forget`](Self::forget) clears them all.
#[derive(Debug, Default)]
struct Owed {
    ports: BTreeMap<(c_int, c_int), u32>,
    clients: BTreeMap<c_int, u32>,
    tentative: BTreeMap<c_int, Tentative>,
}

impl Owed {
    /// Settles the tentative debts the starts and changes read decide.
    fn resolve(&mut self, lives: &[Live]) {
        for live in lives {
            let Some(t) = self.tentative.remove(&live.client()) else {
                continue;
            };
            if live.starts() {
                continue;
            }
            for key in t.ports {
                if let Some(n) = self.ports.get_mut(&key) {
                    *n = n.saturating_sub(1);
                }
            }
            if t.client
                && let Some(n) = self.clients.get_mut(&live.client())
            {
                *n = n.saturating_sub(1);
            }
        }
    }

    /// The departures that are not an owed exit arriving.
    fn pay(&mut self, departed: Vec<Departed>) -> Vec<Departed> {
        departed
            .into_iter()
            .filter(|d| {
                let debt = match *d {
                    Departed::Port(c, p) => self.ports.get_mut(&(c, p)),
                    Departed::Client(c) => self.clients.get_mut(&c),
                };
                match debt {
                    Some(n) if *n > 0 => {
                        *n -= 1;
                        // The owner exited: the debt was no rename.
                        self.tentative.remove(&client_of(*d));
                        false
                    }
                    _ => true,
                }
            })
            .collect()
    }

    /// Records the exits a report leaves owing. `kept` is what it was told
    /// departed and `lives` what started or changed, `gone` the handles it
    /// reported gone, `present` the handles of the look it reported.
    fn owe(
        &mut self,
        kept: &[Departed],
        lives: &[Live],
        gone: &[EndpointId],
        present: &BTreeSet<EndpointId>,
    ) {
        // Per client: whether every debt recorded for it is tentative.
        let mut clients: BTreeMap<c_int, bool> = BTreeMap::new();
        for &handle in gone {
            let (client, port) = unpack_id(handle);
            let named = kept.iter().any(|d| d.names(handle));
            let replaced = present.contains(&handle);
            let started = lives.iter().any(|l| l.client() == client && l.starts());
            let changed = lives.iter().any(|l| l.client() == client && !l.starts());
            // Another identity or gone, with no exit and no start but a change:
            // a rename, or a port whose capabilities changed. The owner did not
            // exit, so nothing is owed.
            if !named && !started && changed {
                continue;
            }
            let tentative = !named && !started;
            if !named {
                *self.ports.entry((client, port)).or_default() += 1;
                if tentative {
                    self.tentative
                        .entry(client)
                        .or_default()
                        .ports
                        .push((client, port));
                }
            }
            // A port that closed on a client that lives owes no client exit.
            let client_gone = !present.iter().any(|&h| unpack_id(h).0 == client);
            if (replaced || client_gone) && !kept.contains(&Departed::Client(client)) {
                let all = clients.entry(client).or_insert(true);
                *all &= tentative;
            }
        }
        for (client, tentative) in clients {
            *self.clients.entry(client).or_default() += 1;
            if tentative {
                self.tentative.entry(client).or_default().client = true;
            }
        }
    }

    fn forget(&mut self) {
        self.ports.clear();
        self.clients.clear();
        self.tentative.clear();
    }
}

/// One wake of the watch: settles, pays what is owed, reports, and records what
/// the report left owing.
fn step(
    reporter: &mut Reporter,
    owed: &mut Owed,
    first: Drained,
    drain: impl FnMut() -> Drained,
    snapshot: impl Fn() -> Vec<OsDevice>,
) {
    let Settled {
        seen: present,
        departed,
        lives,
        overrun,
    } = settle(first, drain, snapshot);
    let kept = if overrun {
        owed.forget();
        departed
    } else {
        owed.resolve(&lives);
        owed.pay(departed)
    };
    let handles: BTreeSet<EndpointId> = present.iter().map(|d| d.handle).collect();
    let gone = reporter.report(present, |id| kept.iter().any(|d| d.names(id)));
    owed.owe(&kept, &lives, &gone, &handles);
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
                // makes the settle look again and clears what is owed. A
                // descriptor that keeps failing ends the drain.
                tracing::debug!("ALSA announce: {}", super::client::strerror(rc));
                found.failed(rc);
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
            match kind {
                sys::SND_SEQ_EVENT_CLIENT_EXIT => found.departed.push(Departed::Client(client)),
                sys::SND_SEQ_EVENT_CLIENT_START => found.lives.push(Live::Started(client)),
                sys::SND_SEQ_EVENT_CLIENT_CHANGE => found.lives.push(Live::Changed(client)),
                sys::SND_SEQ_EVENT_PORT_START => found.lives.push(Live::PortStarted(client, port)),
                sys::SND_SEQ_EVENT_PORT_CHANGE => found.lives.push(Live::PortChanged(client, port)),
                sys::SND_SEQ_EVENT_PORT_EXIT => found.departed.push(Departed::Port(client, port)),
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
    let mut owed = Owed::default();
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    match announce.wait() {
                        Ok(Woken::Announced(first)) => {
                            #[cfg(test)]
                            WAKES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            step(
                                &mut reporter,
                                &mut owed,
                                first,
                                || announce.drain(),
                                || snapshot(&announce.seq),
                            );
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
