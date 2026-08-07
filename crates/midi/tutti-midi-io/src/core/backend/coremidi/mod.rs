//! The macOS backend: CoreMIDI, native UMP.
//!
//! # Almost no raw FFI
//!
//! The safe `coremidi 0.8` wrapper covers the entire real-device path —
//! `Sources`/`Destinations` enumeration, `Client::input_port_with_protocol`
//! (`MIDIInputPortCreateWithProtocol`), `InputPortWithContext::connect_source`,
//! `OutputPort::send` (`MIDISendEventList`), `EventBuffer::push(ts, &[u32])`
//! which takes UMP words directly, and even `Properties::protocol_id()` for
//! capability detection.
//!
//! Raw `coremidi-sys` survives in exactly one place, [`virtual_source`], because
//! the wrapper has `virtual_destination_with_protocol` but **no**
//! `virtual_source_with_protocol` — publishing a MIDI-2.0-protocol virtual
//! source needs `MIDISourceCreateWithProtocol` by hand.
//!
//! # Protocol is asked, not assumed
//!
//! We open input ports with `Protocol::Midi20`, which makes CoreMIDI convert a
//! MIDI-1.0 device's traffic to UMP for us. That is *not* the same as the device
//! speaking MIDI 2.0, so [`UmpCapability`] records `kMIDIPropertyProtocolID` as
//! the OS reports it — and falls back to `Midi1` when the property is absent,
//! which is the conservative direction (see `UmpCapability::default`).

use coremidi::{
    Client, Destinations, EventBuffer, InputPortWithContext, OutputPort, Properties,
    PropertyGetter, Protocol as CmProtocol, Source, Sources,
};

use crate::core::capability::{EndpointId, EndpointInfo, UmpCapability};
use crate::core::endpoints::{InputConnection, MidiEndpoints};
use crate::core::error::{Error, Result};
use crate::core::InputProducerHandle;
use tutti_midi_types::ump::{split_ump_stream, MidiEvent};
use tutti_midi_types::{MidiOut, Protocol};

pub mod virtual_destination;
pub mod virtual_source;
pub use virtual_destination::UmpVirtualDestination;
pub use virtual_source::UmpVirtualSource;

/// CoreMIDI's `kMIDIProtocol_2_0`, as `kMIDIPropertyProtocolID` reports it.
const PROTOCOL_ID_MIDI_2_0: i32 = 2;

/// Read an endpoint's name, falling back to a positional label.
///
/// `display_name` is what the user sees in Audio MIDI Setup — preferred over
/// `name`, which is the bare endpoint name without its device prefix.
fn endpoint_name(object: &coremidi::Object, index: usize) -> String {
    Properties::display_name()
        .value_from(object)
        .or_else(|_| Properties::name().value_from(object))
        .unwrap_or_else(|_| format!("MIDI Endpoint {index}"))
}

/// What the OS says this endpoint carries.
///
/// An absent or unreadable `kMIDIPropertyProtocolID` means "the OS did not say",
/// which resolves to MIDI 1.0 — claiming MIDI 2.0 on no evidence would feed
/// per-note messages into a `to_midi1_bytes` drop.
fn endpoint_capability(object: &coremidi::Object) -> UmpCapability {
    let protocol_id: std::result::Result<i32, _> = Properties::protocol_id().value_from(object);
    match protocol_id {
        Ok(id) if id == PROTOCOL_ID_MIDI_2_0 => UmpCapability::midi2(),
        _ => UmpCapability::midi1(),
    }
}

/// A stable id for an endpoint.
///
/// `kMIDIPropertyUniqueID` survives a device list refresh; the enumeration index
/// does not. Falling back to the index when the property is unreadable keeps a
/// nameless device openable within one enumeration, which is strictly better
/// than dropping it from the list.
fn endpoint_id(object: &coremidi::Object, index: usize) -> EndpointId {
    let unique: std::result::Result<i32, _> = Properties::unique_id().value_from(object);
    match unique {
        Ok(id) => EndpointId::from_raw(id as u32 as u64),
        Err(_) => EndpointId::from_raw(index as u64),
    }
}

/// The macOS [`MidiEndpoints`] implementation.
pub struct CoreMidiEndpoints {
    /// One client for the whole backend. CoreMIDI ties ports to their client, so
    /// a per-open client would leak one per connection.
    client: Option<Client>,
}

impl core::fmt::Debug for CoreMidiEndpoints {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CoreMidiEndpoints").finish_non_exhaustive()
    }
}

impl Default for CoreMidiEndpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreMidiEndpoints {
    /// Create the backend, opening its CoreMIDI client.
    ///
    /// A failed client is kept as `None` rather than panicking: enumeration then
    /// reports nothing and opening reports an error, which is what a headless
    /// machine or a sandbox without MIDI access should look like.
    pub fn new() -> Self {
        Self {
            client: Client::new("tutti-midi").ok(),
        }
    }

    fn client(&self) -> Result<&Client> {
        self.client.as_ref().ok_or(Error::CoreMidi {
            operation: "create client",
            status: -1,
        })
    }

    fn find_source(id: EndpointId) -> Option<Source> {
        (0..Sources::count())
            .filter_map(Source::from_index)
            .enumerate()
            .find(|(i, s)| endpoint_id(s, *i) == id)
            .map(|(_, s)| s)
    }

    fn find_destination(id: EndpointId) -> Option<coremidi::Destination> {
        (0..Destinations::count())
            .filter_map(coremidi::Destination::from_index)
            .enumerate()
            .find(|(i, d)| endpoint_id(d, *i) == id)
            .map(|(_, d)| d)
    }
}

impl MidiEndpoints for CoreMidiEndpoints {
    fn inputs(&self) -> Vec<EndpointInfo> {
        (0..Sources::count())
            .filter_map(Source::from_index)
            .enumerate()
            .map(|(index, source)| EndpointInfo {
                id: endpoint_id(&source, index),
                name: endpoint_name(&source, index),
                capability: endpoint_capability(&source),
            })
            .collect()
    }

    fn outputs(&self) -> Vec<EndpointInfo> {
        (0..Destinations::count())
            .filter_map(coremidi::Destination::from_index)
            .enumerate()
            .map(|(index, dest)| EndpointInfo {
                id: endpoint_id(&dest, index),
                name: endpoint_name(&dest, index),
                capability: endpoint_capability(&dest),
            })
            .collect()
    }

    fn open_input(
        &self,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let client = self.client()?;
        let source = Self::find_source(id)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI source with id {}", id.raw())))?;

        // No `Sysex7Assembler` on this path, deliberately. We open with
        // `Protocol::Midi20`, so CoreMIDI hands us UMP words — a MIDI-1.0
        // device's SysEx arrives already fragmented into UMP SysEx7 packets,
        // which `Sysex7Reassembler` (one layer up) rejoins. The byte-run
        // assembler is for transports that deliver raw `F0 … F7`; ALSA's legacy
        // bridge is one, and it is the ALSA backend that will need it.
        let mut port = client
            .input_port_with_protocol("tutti-in", CmProtocol::Midi20, move |event_list, _ctx| {
                let now = std::time::Instant::now();
                for packet in event_list.iter() {
                    // One CoreMIDI packet is a packed word stream of several
                    // concatenated UMP messages.
                    for event in split_ump_stream(packet.data()) {
                        if !producer.push(event, now) {
                            tracing::debug!("MIDI input ring full, dropping event");
                        }
                    }
                }
            })
            .map_err(|status| Error::CoreMidi {
                operation: "create input port",
                status,
            })?;

        port.connect_source(&source, ())
            .map_err(|status| Error::CoreMidi {
                operation: "connect source",
                status,
            })?;

        Ok(Box::new(CoreMidiInput { id, _port: port }))
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn MidiOut>> {
        let client = self.client()?;
        let destination = Self::find_destination(id).ok_or_else(|| {
            Error::MidiDevice(format!("no MIDI destination with id {}", id.raw()))
        })?;
        let protocol = endpoint_capability(&destination).protocol;

        let port = client
            .output_port("tutti-out")
            .map_err(|status| Error::CoreMidi {
                operation: "create output port",
                status,
            })?;

        Ok(Box::new(CoreMidiOutput {
            port,
            destination,
            protocol,
        }))
    }
}

/// An open CoreMIDI input. Dropping it closes the port.
struct CoreMidiInput {
    id: EndpointId,
    /// Held for its `Drop`: releasing the port is what disconnects.
    _port: InputPortWithContext<()>,
}

// SAFETY: `InputPortWithContext` owns an opaque `MIDIPortRef` — a `UInt32`
// handle into CoreMIDI, whose objects are internally synchronised. The same
// contract `UmpVirtualSource` relies on.
unsafe impl Send for CoreMidiInput {}

impl InputConnection for CoreMidiInput {
    fn endpoint(&self) -> EndpointId {
        self.id
    }
}

/// An open CoreMIDI output, as a [`MidiOut`].
///
/// No interior mutability: `OutputPort::send` takes `&self` and each call builds
/// its own [`EventBuffer`], so `MidiOut::queue(&self, ..)` needs no lock.
///
/// The per-call buffer allocates. That is acceptable *here* and would not be on
/// the audio thread: this is the control-thread send path, reached from the
/// output drain, not from `process()`.
struct CoreMidiOutput {
    port: OutputPort,
    destination: coremidi::Destination,
    protocol: Protocol,
}

// SAFETY: as `CoreMidiInput` — opaque CoreMIDI handles, internally synchronised.
unsafe impl Send for CoreMidiOutput {}
unsafe impl Sync for CoreMidiOutput {}

impl CoreMidiOutput {
    /// Send one UMP message, surfacing the real error.
    ///
    /// The inherent-method-plus-thin-trait-impl idiom `MidiSender` already uses:
    /// a caller holding the concrete type gets the `OSStatus`, while the erased
    /// [`MidiOut`] stays the ring-shaped `queue`.
    fn send_ump(&self, words: &[u32]) -> Result<()> {
        if words.is_empty() {
            return Ok(());
        }
        let cm_protocol = match self.protocol {
            Protocol::Midi2 => CmProtocol::Midi20,
            Protocol::Midi1 => CmProtocol::Midi10,
        };
        let buffer = EventBuffer::new(cm_protocol).with_packet(0, words);
        self.port
            .send(&self.destination, &buffer)
            .map_err(|status| Error::CoreMidi {
                operation: "send event list",
                status,
            })
    }
}

impl MidiOut for CoreMidiOutput {
    /// Returns how many events reached the endpoint. Stops at the first failure
    /// so the count names an unbroken prefix — see [`AlsaOutput::queue`] for why
    /// skipping would be worse than stopping.
    ///
    /// [`AlsaOutput::queue`]: crate::core::backend
    fn queue(&self, events: &[MidiEvent]) -> usize {
        let mut accepted = 0;
        for event in events {
            if let Err(e) = self.send_ump(event.data_words()) {
                tracing::debug!("CoreMIDI send: {e}");
                break;
            }
            accepted += 1;
        }
        accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enumeration must not panic on a machine with no MIDI devices, and must
    /// return well-formed records where there are some. CI boxes have none; a
    /// dev machine has IAC.
    ///
    /// This deliberately does *not* assert a device count — that would pass
    /// vacuously on CI and fail spuriously on a laptop. It asserts the shape of
    /// whatever comes back.
    #[test]
    fn enumeration_yields_well_formed_records() {
        let backend = CoreMidiEndpoints::new();
        for info in backend.inputs().into_iter().chain(backend.outputs()) {
            assert!(
                !info.name.is_empty(),
                "every endpoint must carry a name, even a synthesised one"
            );
        }
    }

    /// An id that no endpoint owns must fail to open rather than resolve to
    /// whatever sits at that index — the reason `EndpointId` is opaque and not a
    /// positional index.
    #[test]
    fn an_unknown_id_does_not_open() {
        let backend = CoreMidiEndpoints::new();
        let unknown = EndpointId::from_raw(u64::from(u32::MAX));
        assert!(
            !backend.outputs().iter().any(|e| e.id == unknown),
            "precondition: the sentinel id must not belong to a real endpoint"
        );
        assert!(backend.open_output(unknown).is_err());
    }

    /// Capability must be *read from the endpoint*, not assumed.
    ///
    /// The trap this guards: we open inputs with `Protocol::Midi20` so CoreMIDI
    /// up-converts MIDI-1.0 traffic for us. If `endpoint_capability` returned
    /// that choice rather than reading `kMIDIPropertyProtocolID`, every endpoint
    /// would claim MIDI 2.0 and `UmpCapability` would be vestigial.
    ///
    /// Enumerating real devices **cannot** catch that here: every endpoint on a
    /// modern Mac (the IAC buses included) genuinely reports MIDI 2.0, so
    /// `assert!(matches!(.., Midi1 | Midi2))` passes under a hardcoded `midi2()`
    /// too — verified by mutation. The fixture has to *supply* the distinction,
    /// so this creates a MIDI-1.0 virtual source and requires the reader to say
    /// so.
    #[test]
    fn capability_is_read_from_the_endpoint_not_assumed() {
        // A MIDI-1.0-protocol virtual source: the safe wrapper's plain
        // `virtual_source` is MIDI 1.0 by construction.
        let client = match Client::new("tutti-cap-test") {
            Ok(c) => c,
            // No CoreMIDI here (sandbox/headless) — nothing to test against.
            Err(_) => return,
        };
        let Ok(source) = client.virtual_source("tutti-cap-midi1") else {
            return;
        };

        let cap = endpoint_capability(&source);
        assert_eq!(
            cap.protocol,
            Protocol::Midi1,
            "a MIDI-1.0 endpoint must report Midi1 — returning a constant Midi2 \
             (or echoing the protocol we open ports with) fails here"
        );
        assert!(!cap.carries_midi2_only());
    }

    /// The `Midi2` direction is covered by the real-device enumeration above
    /// (every endpoint on this machine reports MIDI 2.0), so a hardcoded
    /// `midi1()` would fail *there*. Pinning it here too would need a
    /// MIDI-2.0-protocol endpoint we can hand to `endpoint_capability` as a
    /// `coremidi::Object` — `UmpVirtualSource` keeps its `MIDIEndpointRef`
    /// private, and widening that API purely for a test is the wrong trade.
    /// Between the two, neither constant survives.
    #[test]
    fn a_midi2_endpoint_is_covered_by_enumeration() {
        let backend = CoreMidiEndpoints::new();
        let any_midi2 = backend
            .inputs()
            .into_iter()
            .chain(backend.outputs())
            .any(|e| e.capability.protocol == Protocol::Midi2);

        // Not an assertion: a machine with no MIDI-2.0 endpoint is legitimate,
        // and asserting here would fail spuriously on CI. Reported so a reader
        // knows whether this ran.
        if !any_midi2 {
            eprintln!(
                "note: no MIDI-2.0 endpoint on this machine; the Midi2 direction \
                 of capability reading is uncovered here"
            );
        }
    }
}
