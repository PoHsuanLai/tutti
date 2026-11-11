//! Reading UMP from a sequencer port.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::client::{QueueClock, SeqClient, check};
use super::enumerate::unpack_id;
use super::poll::{ErrorRun, PollSet, Wait};
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::Result;
use tutti_alsa_ump_sys as sys;
use tutti_midi_device::EndpointId;
use tutti_ump::split_stream;

/// `-EAGAIN`. Not an error — "nothing queued yet".
const EAGAIN: i32 = -11;

/// Times the pump came back from `poll`, for the test that an idle pump sleeps.
/// The count is per process, so the test relies on nextest running each test in
/// its own process.
#[cfg(test)]
pub(super) static PUMP_WAKES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// While set, the pump's reads fail with `-EIO` instead of reaching the
/// sequencer: the error path has no other way to be reached on a healthy
/// sequencer. Per process, like [`PUMP_WAKES`].
#[cfg(test)]
pub(super) static INJECT_READ_ERROR: AtomicBool = AtomicBool::new(false);

/// `-EIO`, what the injected failure reads as.
#[cfg(test)]
const EIO: i32 = -5;

/// How long dropping an input waits for its pump to end before it lets the
/// thread go. A pump ends within a poll wake of the stop; only a read that never
/// returns outlasts this, and a drop that waited for it would hang the caller
/// (the session's control thread) with it.
const JOIN_BOUND: Duration = Duration::from_secs(1);

/// How long a pairing of the queue's clock with the OS's is trusted: the two
/// run apart by parts per million, and a pair this old has drifted by
/// microseconds at most.
const PAIRING_AGE: Duration = Duration::from_secs(1);

/// While non-zero, the pump sleeps this many milliseconds after each wake, before
/// it reads: a pump that is late, which is what a timestamp from the sequencer
/// exists to correct. Per process, like [`PUMP_WAKES`].
#[cfg(test)]
pub(super) static PUMP_STALL_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// An open ALSA input. Dropping it stops the pump and closes the port.
pub struct AlsaInput {
    /// One end of a socket pair whose other end is in the pump's `poll` set:
    /// closing this one is the shutdown signal, and wakes the pump at once.
    stop: Option<UnixStream>,
    /// Set by the pump when it ends on an error, never on a stop.
    died: Arc<AtomicBool>,
    /// Disconnects when the pump ends, however it ends: the drop's bounded wait.
    ended: mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputConnection for AlsaInput {
    fn is_alive(&self) -> bool {
        !self.died.load(Ordering::Acquire)
    }
}

impl Drop for AlsaInput {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            // Joining matters: the thread owns the `SeqClient`, and letting it
            // outlive this would leave the port subscribed to a device the
            // caller believes it disconnected. But not for ever: a pump stuck in
            // alsa-lib is detached, and says so.
            match self.ended.recv_timeout(JOIN_BOUND) {
                Err(mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                    "ALSA MIDI pump did not stop within {JOIN_BOUND:?}; left running, still subscribed"
                ),
                // Disconnected: the pump has ended (or is ending), so the join
                // is brief.
                _ => {
                    let _ = t.join();
                }
            }
        }
    }
}

/// The error for an id [`unpack_id`] refuses.
pub(super) fn not_minted(id: EndpointId) -> crate::error::Error {
    crate::error::Error::MidiPort(format!("{id:?} is not an ALSA endpoint id"))
}

/// Opens `id` for input and starts pumping its events into `producer`.
///
/// The pump runs on its own thread and stops when the returned connection is
/// dropped, which is also when the port unsubscribes.
///
/// # Errors
///
/// [`Error::Alsa`](crate::Error::Alsa) if the sequencer cannot be opened, the
/// port created, or the subscription made, and
/// [`Error::MidiDevice`](crate::Error::MidiDevice) if the pump thread cannot be
/// spawned.
pub fn open(id: EndpointId, producer: InputProducerHandle) -> Result<Box<dyn InputConnection>> {
    let (src_client, src_port) = unpack_id(id).ok_or_else(|| not_minted(id))?;

    // Non-blocking, so the pump reads until the queue is empty and then waits in
    // `poll`, which the stop descriptor can interrupt. One client per connection
    // — see `SeqClient`.
    let seq = SeqClient::open("tutti-in", true)?;
    let port = seq.open_port(
        "tutti-in",
        // Not subscribable by others: the engine's pump is not a device, and a
        // port nobody else can connect to is never listed as one. This client
        // makes its own subscription below, which needs no such capability.
        sys::SND_SEQ_PORT_CAP_WRITE,
        sys::SND_SEQ_PORT_TYPE_MIDI_GENERIC | sys::SND_SEQ_PORT_TYPE_APPLICATION,
    )?;

    // The kernel stamps each event as it delivers it, so the time an event
    // is given is when it arrived, not when this pump got round to reading it.
    let clock = seq.stamp_port(port)?;

    // SAFETY: `seq` is open; the address came from enumeration.
    check(
        unsafe { sys::snd_seq_connect_from(seq.raw(), port, src_client, src_port) },
        "snd_seq_connect_from",
    )?;

    let (stop_tx, stop_rx) = UnixStream::pair()
        .map_err(|e| crate::error::Error::MidiDevice(format!("MIDI pump stop pair: {e}")))?;
    let polls = PollSet::new(&seq, stop_rx.as_raw_fd())?;
    let died = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&died);
    let (ending, ended) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("tutti-midi-in".to_string())
        .spawn(move || {
            // Dropped when the pump returns or unwinds, which disconnects `ended`.
            let _ending: mpsc::Sender<()> = ending;
            pump(seq, polls, clock, stop_rx, flag, producer);
        })
        .map_err(|e| crate::error::Error::MidiDevice(format!("spawn MIDI pump: {e}")))?;

    Ok(Box::new(AlsaInput {
        stop: Some(stop_tx),
        died,
        ended,
        thread: Some(thread),
    }))
}

/// Read UMP events until the stop pair closes. Reads until the queue is empty,
/// then blocks in `poll`: the descriptors are level-triggered, so an event that
/// arrives between the last read and the `poll` wakes it at once.
///
/// An error ends the pump with `died` set (and a warning): the port goes silent
/// while the connection is still held, and [`InputConnection::is_alive`] is how
/// the holder finds out.
///
/// Takes `seq` **by value** so the client is dropped here, on this thread, when
/// the loop ends — the port unsubscribes exactly when the pump stops.
fn pump(
    seq: SeqClient,
    mut polls: PollSet,
    clock: QueueClock,
    // Held so its descriptor stays open while the pump polls it.
    _stop: UnixStream,
    died: Arc<AtomicBool>,
    mut producer: InputProducerHandle,
) {
    let die = |why: &str| {
        tracing::warn!("ALSA MIDI input stopped: {why}");
        died.store(true, Ordering::Release);
    };
    let mut errors = ErrorRun::default();
    let mut arrivals = Arrivals {
        clock,
        pairing: None,
    };
    loop {
        let mut ev: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
        // SAFETY: `seq` is open for the life of this function; `ev` is a valid
        // out-pointer. On success alsa-lib points it at a buffer it owns, valid
        // until the next call on this client — which is why the words are copied
        // out immediately below and the pointer never escapes.
        let rc = unsafe { sys::snd_seq_ump_event_input(seq.raw(), &mut ev) };
        #[cfg(test)]
        let rc = if INJECT_READ_ERROR.load(Ordering::SeqCst) {
            EIO
        } else {
            rc
        };

        if rc < 0 {
            if rc != EAGAIN {
                // An overrun or a failing read: try again at once, a few times
                // in a row at most (a level-triggered descriptor would spin).
                tracing::debug!("ALSA MIDI input: {}", super::client::strerror(rc));
                if errors.failed() {
                    return die(&super::client::strerror(rc));
                }
                continue;
            }
            // Empty queue is the common case, not a failure.
            let waited = polls.wait(None);
            #[cfg(test)]
            PUMP_WAKES.fetch_add(1, Ordering::SeqCst);
            #[cfg(test)]
            if let Ok(Wait::Ready) = waited {
                let ms = PUMP_STALL_MS.load(Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(ms));
            }
            match waited {
                Ok(Wait::Ready) => continue,
                Ok(Wait::Stopped) => return,
                Ok(Wait::Broken) => return die("the sequencer descriptor failed"),
                Ok(Wait::Timeout) => unreachable!("wait(None) has no timeout"),
                Err(e) => return die(&e.to_string()),
            }
        }
        errors.read();
        if ev.is_null() {
            continue;
        }

        // SAFETY: `rc >= 0` and `ev` non-NULL means alsa-lib wrote a complete
        // event; copying by value ends this function's use of its buffer.
        let (words, stamped) = unsafe { ((*ev).ump, (*ev).real_time()) };
        let now = match stamped {
            Some(t) => arrivals.at(&seq, Duration::new(u64::from(t.tv_sec), t.tv_nsec)),
            // A port that is not stamping: the read is the best there is.
            None => Instant::now(),
        };

        // A `snd_seq_ump_event_t` carries one UMP message in a fixed 4-word
        // field, so the trailing words are padding whenever the message is
        // shorter. `split_stream` reads the length from the type nibble and
        // yields exactly that one message — using it rather than passing all
        // four words into one packet is what keeps a 1-word message from
        // swallowing three words of zeros.
        let decoded = split_stream(&words).next();
        if let Some(event) = decoded
            && !producer.push(event, now)
        {
            tracing::debug!("MIDI input ring full, dropping event");
        }
    }
}

/// A queue time as an OS instant: the queue's clock and the OS's read together
/// once (`pairing`), then each event's age is the queue's time then, subtracted
/// from the pairing's ([`crate::stamp::arrival`]).
struct Arrivals {
    clock: QueueClock,
    /// `(OS instant, queue time)` read at the same moment.
    pairing: Option<(Instant, Duration)>,
}

impl Arrivals {
    /// The OS instant an event stamped `stamped` on the queue arrived at. Reads
    /// a fresh pairing when the held one is older than the event (the event
    /// came after it) or than [`PAIRING_AGE`]; one syscall, not one per event.
    fn at(&mut self, seq: &SeqClient, stamped: Duration) -> Instant {
        let stale = self
            .pairing
            .is_none_or(|(os, queue)| stamped > queue || os.elapsed() > PAIRING_AGE);
        if stale {
            self.pairing = self.clock.now(seq).map(|queue| (Instant::now(), queue));
        }
        match self.pairing {
            Some((os, queue)) => crate::stamp::arrival(os, queue, stamped),
            None => Instant::now(),
        }
    }
}
