//! Reading UMP from a sequencer port.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::{c_short, c_uint};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use super::client::{SeqClient, check};
use super::enumerate::unpack_id;
use crate::InputProducerHandle;
use crate::endpoints::InputConnection;
use crate::error::Result;
use tutti_alsa_ump_sys as sys;
use tutti_midi_device::EndpointId;
use tutti_midi_types::ump::split_ump_stream;

/// `-EAGAIN`. Not an error — "nothing queued yet".
const EAGAIN: i32 = -11;

/// Times the pump came back from `poll`, for the test that an idle pump sleeps.
#[cfg(test)]
pub(super) static PUMP_WAKES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// An open ALSA input. Dropping it stops the pump and closes the port.
pub struct AlsaInput {
    /// One end of a socket pair whose other end is in the pump's `poll` set:
    /// closing this one is the shutdown signal, and wakes the pump at once.
    stop: Option<UnixStream>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputConnection for AlsaInput {}

impl Drop for AlsaInput {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            // Joining matters: the thread owns the `SeqClient`, and letting it
            // outlive this would leave the port subscribed to a device the
            // caller believes it disconnected.
            let _ = t.join();
        }
    }
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
    let (src_client, src_port) = unpack_id(id);

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

    // SAFETY: `seq` is open; the address came from enumeration.
    check(
        unsafe { sys::snd_seq_connect_from(seq.raw(), port, src_client, src_port) },
        "snd_seq_connect_from",
    )?;

    let (stop_tx, stop_rx) = UnixStream::pair()
        .map_err(|e| crate::error::Error::MidiDevice(format!("MIDI pump stop pair: {e}")))?;
    let polls = PollSet::new(&seq, stop_rx.as_raw_fd())?;
    let thread = std::thread::Builder::new()
        .name("tutti-midi-in".to_string())
        .spawn(move || pump(seq, polls, stop_rx, producer))
        .map_err(|e| crate::error::Error::MidiDevice(format!("spawn MIDI pump: {e}")))?;

    Ok(Box::new(AlsaInput {
        stop: Some(stop_tx),
        thread: Some(thread),
    }))
}

/// The descriptors the pump waits on: the sequencer's, then the stop pair's.
struct PollSet {
    polls: Vec<libc::pollfd>,
}

/// Why [`PollSet::wait`] returned.
enum Ready {
    /// The sequencer has input.
    Input,
    /// The stop pair closed.
    Stopped,
}

impl PollSet {
    fn new(seq: &SeqClient, stop: RawFd) -> Result<Self> {
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

    /// Blocks until the sequencer has input or the stop pair closes.
    fn wait(&mut self) -> io::Result<Ready> {
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
                return Err(error);
            }
            #[cfg(test)]
            PUMP_WAKES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (stop, seq) = self.polls.split_last().expect("the stop descriptor");
            if stop.revents != 0 {
                return Ok(Ready::Stopped);
            }
            let broken = (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) as c_short;
            if seq.iter().any(|p| p.revents & broken != 0) {
                return Err(io::Error::other("the sequencer descriptor failed"));
            }
            return Ok(Ready::Input);
        }
    }
}

/// Read UMP events until the stop pair closes. Reads until the queue is empty,
/// then blocks in `poll`: the descriptors are level-triggered, so an event that
/// arrives between the last read and the `poll` wakes it at once.
///
/// Takes `seq` **by value** so the client is dropped here, on this thread, when
/// the loop ends — the port unsubscribes exactly when the pump stops.
fn pump(
    seq: SeqClient,
    mut polls: PollSet,
    // Held so its descriptor stays open while the pump polls it.
    _stop: UnixStream,
    mut producer: InputProducerHandle,
) {
    loop {
        let mut ev: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
        // SAFETY: `seq` is open for the life of this function; `ev` is a valid
        // out-pointer. On success alsa-lib points it at a buffer it owns, valid
        // until the next call on this client — which is why the words are copied
        // out immediately below and the pointer never escapes.
        let rc = unsafe { sys::snd_seq_ump_event_input(seq.raw(), &mut ev) };

        if rc < 0 {
            // Empty queue is the common case, not a failure.
            if rc != EAGAIN {
                tracing::debug!("ALSA MIDI input: {}", super::client::strerror(rc));
            }
            match polls.wait() {
                Ok(Ready::Input) => continue,
                Ok(Ready::Stopped) => return,
                Err(e) => {
                    tracing::debug!("ALSA MIDI input stopped: {e}");
                    return;
                }
            }
        }
        if ev.is_null() {
            continue;
        }

        // SAFETY: `rc >= 0` and `ev` non-NULL means alsa-lib wrote a complete
        // event; copying by value ends this function's use of its buffer.
        let words = unsafe { (*ev).ump };
        let now = Instant::now();

        // A `snd_seq_ump_event_t` carries one UMP message in a fixed 4-word
        // field, so the trailing words are padding whenever the message is
        // shorter. `split_ump_stream` reads the length from the type nibble and
        // yields exactly that one message — using it rather than passing all
        // four words to `from_ump` is what keeps a 1-word message from
        // swallowing three words of zeros.
        let decoded = split_ump_stream(&words).next();
        if let Some(event) = decoded
            && !producer.push(event, now)
        {
            tracing::debug!("MIDI input ring full, dropping event");
        }
    }
}
