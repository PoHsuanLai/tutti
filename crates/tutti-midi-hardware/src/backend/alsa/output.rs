//! Sending UMP to a sequencer port.

use std::sync::Mutex;

use super::client::{SeqClient, check};
use super::enumerate::unpack_id;
use crate::error::{Error, Result};
use tutti_alsa_ump_sys as sys;
use tutti_midi_device::EndpointId;
use tutti_midi_types::MidiOut;
use tutti_midi_types::ump::MidiEvent;

/// An open ALSA output, as a [`MidiOut`].
pub struct AlsaOutput {
    /// The client, its port, and this client's own id — all three are needed to
    /// address an outbound event, and they are only valid together.
    ///
    /// Behind a `Mutex` because `MidiOut::queue` takes `&self` while
    /// `snd_seq_ump_event_output_direct` takes `*mut`. Sending happens on the
    /// control thread's output drain, never the audio thread, so a lock costs
    /// nothing that matters here.
    inner: Mutex<Sender>,
}

struct Sender {
    seq: SeqClient,
    /// Our client id, as the sequencer addresses it.
    own_client: u8,
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
    ///
    /// # Panics
    ///
    /// If the output mutex was poisoned by a previous panic while sending.
    pub fn send_ump(&self, words: &[u32]) -> Result<()> {
        if words.is_empty() {
            return Ok(());
        }
        let inner = self.inner.lock().expect("output mutex poisoned");

        let mut ev = sys::snd_seq_ump_event_t {
            // `SND_SEQ_EVENT_NONE`. For a UMP event the type field is unused —
            // the message type lives in the first word's top nibble — but it
            // must be zeroed rather than left stale.
            type_: 0,
            source: sys::snd_seq_addr_t {
                client: inner.own_client,
                port: inner.port,
            },
            // **Not** the destination's own address, even though it is known.
            //
            // The port is already subscribed (`snd_seq_connect_to` at open), and
            // an explicit `dest` on top of a subscription is rejected outright —
            // `snd_seq_ump_event_output_direct` returns `-EINVAL`, not a partial
            // send. `SUBSCRIBERS` means "everyone connected to my port", which
            // is exactly the set `connect_to` established.
            dest: sys::snd_seq_addr_t {
                client: sys::SND_SEQ_ADDRESS_SUBSCRIBERS,
                port: 0,
            },
            // Dispatch now rather than through a queue. Zero here is queue 0 —
            // a real queue this client never created — which is the other half
            // of that same `-EINVAL`.
            queue: sys::SND_SEQ_QUEUE_DIRECT,
            ..Default::default()
        };
        // A UMP message is 1–4 words; the rest of the field stays zero.
        let n = words.len().min(4);
        ev.ump[..n].copy_from_slice(&words[..n]);

        // SAFETY: `seq` is open for the life of `inner`; `ev` is a fully
        // initialised, correctly laid out event (see the sys crate's compile-time layout asserts) that
        // alsa-lib reads and does not retain.
        check(
            unsafe { sys::snd_seq_ump_event_output_direct(inner.seq.raw(), &mut ev) },
            "snd_seq_ump_event_output_direct",
        )?;
        Ok(())
    }
}

impl MidiOut for AlsaOutput {
    /// Returns how many events reached the sequencer.
    ///
    /// Stops at the first failure rather than skipping it: the count names an
    /// unbroken prefix, so a caller can tell exactly where the stream stopped.
    /// Skipping would report a number that no contiguous run matches, and would
    /// deliver a note-off whose note-on never left.
    fn queue(&self, events: &[MidiEvent]) -> usize {
        let mut accepted = 0;
        for event in events {
            if let Err(e) = self.send_ump(event.data_words()) {
                tracing::debug!("ALSA MIDI send: {e}");
                break;
            }
            accepted += 1;
        }
        accepted
    }
}

/// Opens `id` for output.
///
/// # Errors
///
/// [`Error::Alsa`] if the sequencer cannot be opened, the
/// port created, or the subscription to `id` made.
pub fn open(id: EndpointId) -> Result<Box<dyn MidiOut>> {
    let (dest_client, dest_port) = unpack_id(id);

    // Blocking: a send should apply back-pressure rather than silently drop when
    // the destination's queue is full. Only the *input* pump needs to poll.
    let seq = SeqClient::open("tutti-out", false)?;
    let port = seq.open_port(
        "tutti-out",
        // Not subscribable by others, so never listed as a device (see
        // `input::open`); this client subscribes its own port to the
        // destination.
        sys::SND_SEQ_PORT_CAP_READ,
        sys::SND_SEQ_PORT_TYPE_MIDI_GENERIC | sys::SND_SEQ_PORT_TYPE_APPLICATION,
    )?;
    let own_client = seq_addr_byte(seq.id()?, "snd_seq_client_id")?;
    let own_port = seq_addr_byte(port, "snd_seq_create_simple_port")?;

    // SAFETY: `seq` is open; the address came from enumeration.
    check(
        unsafe { sys::snd_seq_connect_to(seq.raw(), port, dest_client, dest_port) },
        "snd_seq_connect_to",
    )?;

    // The destination is not stored: once `connect_to` has subscribed us, every
    // send addresses `SUBSCRIBERS` rather than the endpoint by name. Keeping the
    // address around would invite exactly the explicit-`dest` send that ALSA
    // rejects.
    Ok(Box::new(AlsaOutput {
        inner: Mutex::new(Sender {
            seq,
            own_client,
            port: own_port,
        }),
    }))
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
