//! Sending UMP to a sequencer port.

use std::sync::{Mutex, PoisonError};
use tutti_graph::UmpAt;

use super::enumerate::unpack_id;
use crate::endpoints::DeviceOut;
use crate::error::{Error, Result};
use tutti_alsa_ffi::{CAP_READ, Client, TYPE_APPLICATION, TYPE_MIDI_GENERIC};
use tutti_midi_device::EndpointId;
use tutti_midi_runtime::MidiOut;

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
}

impl AlsaOutput {
    /// Sends one UMP message, surfacing the real error.
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
    pub fn send_ump(&self, words: &[u32]) -> Result<()> {
        if words.is_empty() {
            return Ok(());
        }
        // A poisoned lock guards nothing broken: `Sender` is a client handle and
        // two ids, never left half-written, and a send that panicked inside
        // alsa-lib leaves the client usable. A panic here would take down the
        // output drain over a fault that is already past.
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        // A UMP message is 1-4 words; only the first four are sent.
        inner.seq.send_to_subscribers(inner.port, words)?;
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
        let mut accepted = 0;
        for (_, ump) in events {
            if let Err(e) = self.send_ump(ump.words()) {
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
    let seq = super::client::open("tutti-out", false)?;
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

    // The destination is not stored: once `connect_to` has subscribed us, every
    // send addresses `SUBSCRIBERS` rather than the endpoint by name. Keeping the
    // address around would invite exactly the explicit-`dest` send that ALSA
    // rejects.
    Ok(AlsaOutput {
        inner: Mutex::new(Sender {
            seq,
            port: own_port,
        }),
    })
}

/// A client or port id as the sequencer's address byte; ids come from the
/// sequencer and fit, so a wider one is reported rather than truncated.
fn seq_addr_byte(id: i32, operation: &'static str) -> Result<u8> {
    u8::try_from(id).map_err(|_| Error::Alsa {
        operation,
        code: -EINVAL,
    })
}

const EINVAL: i32 = 22;
