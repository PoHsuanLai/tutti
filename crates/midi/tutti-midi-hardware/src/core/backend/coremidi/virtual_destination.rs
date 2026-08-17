//! A **native-UMP** virtual MIDI destination (macOS / CoreMIDI) — the inbound
//! counterpart to [`UmpVirtualSource`](super::UmpVirtualSource).
//!
//! Why this exists: over a MIDI-1.0 transport every inbound message arrives as
//! legacy bytes. Most of MIDI 2.0 survives that — MIDI-CI is Universal SysEx *by
//! design* (M2-101), precisely so two devices can negotiate before either knows
//! the other speaks MIDI 2.0 — but **UMP Stream** (message type 0xF: Endpoint
//! Discovery / Info / Function Block, M2-104 §7.1) has *no* MIDI-1.0 encoding at
//! all. It can only arrive over a MIDI-2.0-protocol endpoint, which is what this
//! publishes.
//!
//! [`UmpVirtualDestination`] creates one such endpoint with
//! `MIDIDestinationCreateWithProtocol` and hands every received UMP message to a
//! callback. Unlike the source side, the safe `coremidi` 0.8 wrapper *does*
//! expose this (`Client::virtual_destination_with_protocol`), so no unsafe FFI
//! bridge is needed here.
//!
//! CoreMIDI delivers a packet as a *packed word stream* — several concatenated
//! UMP messages with no separators, each one's length implied by its type nibble.
//! [`split_ump_stream`](tutti_midi_types::ump::split_ump_stream) walks that.

use coremidi::{Client, EventList, Protocol, VirtualDestination};
use tracing::debug;
use tutti_midi_types::ump::{split_ump_stream, MidiEvent};

use crate::core::error::{Error, Result};

/// A CoreMIDI virtual destination that speaks the **MIDI 2.0 (UMP) protocol**.
///
/// Other apps see it as a UMP-capable MIDI destination and can send us native
/// UMP — including the UMP-Stream messages that have no MIDI-1.0 form. Each
/// received message is passed to the callback supplied at construction, on
/// CoreMIDI's own delivery thread.
///
/// Keep the value alive: dropping it tears the endpoint down.
pub struct UmpVirtualDestination {
    // Both are retained purely to own the endpoint's lifetime — the callback
    // installed at creation is what actually does the work.
    _client: Client,
    _destination: VirtualDestination,
    name: String,
}

impl core::fmt::Debug for UmpVirtualDestination {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpVirtualDestination")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl UmpVirtualDestination {
    /// Create a MIDI-2.0 virtual destination named `name`, invoking `on_event`
    /// for every UMP message received.
    ///
    /// `on_event` runs on CoreMIDI's delivery thread, so it must not block —
    /// push into a lock-free ring (see
    /// [`with_producer`](Self::with_producer)) rather than doing work inline.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`] if CoreMIDI refuses either the client or the
    /// destination endpoint.
    pub fn new<F>(name: &str, mut on_event: F) -> Result<Self>
    where
        F: FnMut(MidiEvent) + Send + 'static,
    {
        let client =
            Client::new(&format!("tutti-ump-dst-{name}")).map_err(|status| Error::CoreMidi {
                operation: "create client (ump destination)",
                status,
            })?;

        let destination = client
            .virtual_destination_with_protocol(
                name,
                Protocol::Midi20,
                move |event_list: &EventList| {
                    for packet in event_list.iter() {
                        // One packet carries several concatenated UMP messages.
                        for event in split_ump_stream(packet.data()) {
                            on_event(event);
                        }
                    }
                },
            )
            .map_err(|status| Error::CoreMidi {
                operation: "create ump virtual destination",
                status,
            })?;

        debug!(name, "Created native-UMP virtual MIDI destination");
        Ok(Self {
            _client: client,
            _destination: destination,
            name: name.to_string(),
        })
    }

    /// Create a MIDI-2.0 virtual destination that pushes every received message
    /// into an input ring, exactly like a hardware input port does.
    ///
    /// This is the wiring an app wants: the events land in the same
    /// [`HardwareMidiInputs`](crate::core::HardwareMidiInputs) rings a physical
    /// endpoint feeds, so downstream consumers see one merged stream regardless
    /// of which transport a message arrived on.
    pub fn with_producer(name: &str, producer: crate::core::InputProducerHandle) -> Result<Self> {
        Self::new(name, move |event| {
            if !producer.push(event, std::time::Instant::now()) {
                debug!("UMP input ring full, dropping event");
            }
        })
    }

    /// The destination's display name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    #[test]
    fn create_ump_destination() {
        let dst = UmpVirtualDestination::new("Test UMP Dest", |_| {}).expect("creates");
        assert_eq!(dst.name(), "Test UMP Dest");
    }

    /// The decode half of the receive path, driven directly.
    ///
    /// A CoreMIDI packet is a packed word stream; this is the exact transform the
    /// receive callback applies to `packet.data()`. Tested here rather than over a
    /// live endpoint because two *virtual* CoreMIDI endpoints in one process are
    /// not connected to each other — a virtual source publishes to clients that
    /// explicitly connect to it, so a source→destination round trip in-process
    /// delivers nothing and would make the test vacuous. Real delivery needs a
    /// second process or hardware, which belongs in manual QA, not `cargo test`.
    ///
    /// UMP Stream (type 0xF) is the message family that motivates this endpoint:
    /// it has no MIDI-1.0 encoding, so it can only arrive this way.
    #[test]
    fn packet_words_decode_to_ump_stream_and_channel_voice() {
        use tutti_midi_types::ump::UmpMessageType;
        use tutti_midi_types::EndpointDiscoveryRequest;

        let discovery = MidiEvent::endpoint_discovery(1, 1, EndpointDiscoveryRequest::all());
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(3), 60, 0x8000);

        // One packet carrying both, concatenated exactly as CoreMIDI delivers.
        let mut packet_words = Vec::new();
        packet_words.extend_from_slice(discovery.data_words());
        packet_words.extend_from_slice(note.data_words());

        let decoded: Vec<_> = split_ump_stream(&packet_words).collect();
        assert_eq!(decoded, vec![discovery, note]);
        assert_eq!(decoded[0].message_type(), UmpMessageType::UmpStream);
        assert_eq!(decoded[1].message_type(), UmpMessageType::ChannelVoice2);
    }

    /// The callback wiring: whatever the decode yields must reach the caller's
    /// closure. Exercises the `FnMut(MidiEvent)` seam `new` installs.
    #[test]
    fn callback_receives_each_decoded_event() {
        let (tx, rx) = mpsc::channel();
        let sink = move |event: MidiEvent| {
            let _ = tx.send(event);
        };

        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
        let clock = MidiEvent::timing_clock(MidiGroup::FIRST);
        let mut words = Vec::new();
        words.extend_from_slice(note.data_words());
        words.extend_from_slice(clock.data_words());

        // The body of the receive callback, verbatim.
        for event in split_ump_stream(&words) {
            sink(event);
        }

        assert_eq!(rx.recv().expect("note"), note);
        assert_eq!(rx.recv().expect("clock"), clock);
    }
}
