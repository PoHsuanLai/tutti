//! Sending UMP to a sequencer port.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tutti_graph::UmpAt;

use super::enumerate::unpack_id;
use crate::endpoints::DeviceOut;
use crate::error::{Error, Result};
use tutti_alsa_ffi::{
    CAP_READ, Client, QueueClock, TYPE_APPLICATION, TYPE_MIDI_GENERIC, UmpVersion,
};
use tutti_midi_device::EndpointId;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt};

/// An open ALSA output, as a [`MidiOut`].
pub struct AlsaOutput {
    /// The client and its port, which are only valid together.
    ///
    /// Behind a `Mutex` because `MidiOut::queue` takes `&self` and a `Client`
    /// is used from one thread at a time. Sending happens on the control
    /// thread's output drain, never the audio thread, so a lock costs nothing
    /// that matters here.
    inner: Mutex<Sender>,
}

struct Sender {
    seq: Client,
    port: u8,
    /// The client's own queue, started when the output opened, which holds an
    /// event until its time.
    queue: QueueClock,
    /// The queue's real time and the `Instant` read together when the output
    /// opened: [`Sender::queue_time`]'s pairing.
    paired: (Instant, Duration),
}

impl Sender {
    /// The queue's real time at `at`: the one place an `Instant` becomes
    /// ALSA's clock, by the pairing taken at the output's open
    /// ([`crate::stamp::departure`]).
    fn queue_time(&self, at: Instant) -> Option<Duration> {
        crate::stamp::departure(self.paired.0, self.paired.1, at)
    }
}

impl AlsaOutput {
    /// Sends one UMP message, surfacing the real error, held by the sequencer
    /// until `at` (`Now` is at once; an instant is the queue's real time, and
    /// one before the queue's pairing is sent at once, as a time already past
    /// is).
    ///
    /// The inherent-method-plus-thin-trait-impl idiom: a caller holding the
    /// concrete type gets the ALSA code, while the erased [`MidiOut`] keeps the
    /// ring-shaped `queue`.
    ///
    /// `words` is one complete UMP message; only its first 4 words are sent,
    /// which is the whole of any UMP message. Empty input is a no-op.
    ///
    /// # Errors
    ///
    /// [`Error::Alsa`] with the code
    /// `snd_seq_ump_event_output_direct` returned.
    pub fn send_ump_at(&self, words: &[u32], at: SendAt) -> Result<()> {
        if words.is_empty() {
            return Ok(());
        }
        // A poisoned lock guards nothing broken: `Sender` is a client handle and
        // two ids, never left half-written, and a send that panicked inside
        // alsa-lib leaves the client usable. A panic here would take down the
        // output drain over a fault that is already past.
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        // A UMP message is 1-4 words; only the first four are sent.
        match at {
            SendAt::At(instant) => match inner.queue_time(instant) {
                Some(time) => {
                    inner
                        .seq
                        .send_to_subscribers_at(inner.port, words, &inner.queue, time)?;
                }
                // Beyond what a queue's clock holds: not schedulable, so now.
                None => inner.seq.send_to_subscribers(inner.port, words)?,
            },
            SendAt::Now => inner.seq.send_to_subscribers(inner.port, words)?,
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "sequencer-tests"))]
impl AlsaOutput {
    /// Poisons the send lock, as a panic inside a send would.
    pub(super) fn poison(&self) {
        std::thread::scope(|scope| {
            let panicked = scope.spawn(|| {
                let _held = self.inner.lock().unwrap();
                panic!("poisoning the output lock on purpose");
            });
            assert!(panicked.join().is_err());
        });
        assert!(self.inner.is_poisoned());
    }
}

impl MidiOut for AlsaOutput {
    /// Returns how many events reached the sequencer.
    ///
    /// Stops at the first failure rather than skipping it: the count names an
    /// unbroken prefix, so a caller can tell exactly where the stream stopped.
    /// Skipping would report a number that no contiguous run matches, and would
    /// deliver a note-off whose note-on never left.
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.queue_timed(&Outgoing::now_batch(events))
    }

    /// Each event is held by the sequencer's queue until its time (an `At`),
    /// or sent directly (`Now`).
    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        let mut accepted = 0;
        for event in events {
            if let Err(e) = self.send_ump_at(event.ump.words(), event.send) {
                tracing::debug!("ALSA MIDI send: {e}");
                break;
            }
            accepted += 1;
        }
        accepted
    }
}

impl DeviceOut for AlsaOutput {}

/// Opens `id` for output.
///
/// # Errors
///
/// [`Error::Alsa`] if the sequencer cannot be opened, the
/// port created, or the subscription to `id` made.
pub fn open(id: EndpointId) -> Result<Box<dyn DeviceOut>> {
    Ok(Box::new(connect(id)?))
}

/// [`open`], as the concrete type.
pub(super) fn connect(id: EndpointId) -> Result<AlsaOutput> {
    let (dest_client, dest_port) = unpack_id(id).ok_or_else(|| super::input::not_minted(id))?;

    // Blocking: a send should apply back-pressure rather than silently drop when
    // the destination's queue is full. Only the *input* pump needs to poll.
    let seq = super::client::open("tutti-out", false, UmpVersion::Midi2)?;
    let port = seq.open_port(
        "tutti-out",
        // Not subscribable by others, so never listed as a device (see
        // `input::open`); this client subscribes its own port to the
        // destination.
        CAP_READ,
        TYPE_MIDI_GENERIC | TYPE_APPLICATION,
    )?;
    let own_port = seq_addr_byte(port, "snd_seq_create_simple_port")?;

    seq.connect_to(port, dest_client, dest_port)?;

    // The queue that holds scheduled events, paired with the OS's clock once:
    // the queue's real time is read between two readings of `Instant`, and the
    // pairing is their midpoint. A queue the client never created answers
    // `-EINVAL` to a scheduled send, so it is the client's own and started.
    let mut queue = seq.start_queue()?;
    let before = Instant::now();
    let started = queue.now(&seq);
    let after = Instant::now();
    let started = started.ok_or_else(|| Error::Alsa {
        operation: "snd_seq_get_queue_status",
        // The sequencer would not read its own queue: `-EINVAL`.
        code: -22,
    })?;
    let paired = (before + after.duration_since(before) / 2, started);

    // The destination is not stored: once `connect_to` has subscribed us, every
    // send addresses `SUBSCRIBERS` rather than the endpoint by name. Keeping the
    // address around would invite exactly the explicit-`dest` send that ALSA
    // rejects.
    Ok(AlsaOutput {
        inner: Mutex::new(Sender {
            seq,
            port: own_port,
            queue,
            paired,
        }),
    })
}

/// A client or port id as the sequencer's address byte; ids come from the
/// sequencer and fit, so a wider one is reported rather than truncated.
pub(super) fn seq_addr_byte(id: i32, operation: &'static str) -> Result<u8> {
    u8::try_from(id).map_err(|_| Error::Alsa {
        operation,
        code: -EINVAL,
    })
}

const EINVAL: i32 = 22;
