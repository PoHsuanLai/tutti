//! The macOS backend: CoreMIDI, native UMP.
//!
//! # Almost no raw FFI
//!
//! The safe `coremidi` wrapper covers the entire real-device path —
//! `Sources`/`Destinations` enumeration, `Client::input_port_with_protocol`
//! (`MIDIInputPortCreateWithProtocol`), `InputPortWithContext::connect_source`,
//! `OutputPort::send` (`MIDISendEventList`), `EventBuffer::push(ts, &[u32])`
//! which takes UMP words directly, and even `Properties::protocol_id()` for
//! capability detection.
//!
//! Raw `coremidi-sys` is confined to exactly one place, [`virtual_source`],
//! because the wrapper has `virtual_destination_with_protocol` but **no**
//! `virtual_source_with_protocol` — publishing a MIDI-2.0-protocol virtual
//! source needs `MIDISourceCreateWithProtocol` by hand.
//!
//! # Protocol is asked, not assumed
//!
//! Input ports open with `Protocol::Midi20`, which makes CoreMIDI up-convert a
//! MIDI-1.0 device's traffic to UMP. That is *not* the same as the device
//! speaking MIDI 2.0, so [`UmpCapability`] records `kMIDIPropertyProtocolID` as
//! the OS reports it — and falls back to `Midi1` when the property is absent,
//! which is the conservative direction (see `UmpCapability::default`).

use coremidi::{
    Client, Destinations, EventBuffer, InputPortWithContext, OutputPort, Properties,
    PropertyGetter, Protocol as CmProtocol, Source, Sources,
};

use crate::InputProducerHandle;
use crate::capability::{EndpointId, EndpointInfo, UmpCapability};
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use tutti_midi_types::ump::{MidiEvent, split_ump_stream};
use tutti_midi_types::{MidiOut, Protocol};

pub mod virtual_destination;
pub mod virtual_source;
pub use virtual_destination::UmpVirtualDestination;
pub use virtual_source::UmpVirtualSource;

/// The process-wide CoreMIDI client every backend and virtual destination
/// shares.
///
/// coremidi 0.9 never disposes a [`Client`] on drop (Apple: disposing the last
/// client may make the MIDI server exit and fail every later client creation),
/// so a client per endpoint set or virtual port would accumulate. One is created
/// on first use and kept for the life of the process; a failed creation is not
/// cached, so a later call retries.
pub(crate) fn shared_client() -> std::result::Result<&'static Client, coremidi::OSStatus> {
    static CLIENT: std::sync::Mutex<Option<&'static Client>> = std::sync::Mutex::new(None);
    let mut slot = CLIENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(client) = *slot {
        return Ok(client);
    }
    let client: &'static Client = Box::leak(Box::new(Client::new("tutti-midi")?));
    *slot = Some(client);
    Ok(client)
}

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

/// A stable id for an endpoint, or `None` when the OS gives it no unique id.
///
/// `kMIDIPropertyUniqueID` survives a device list refresh; the enumeration index
/// does not, and an [`EndpointId`] promises the same endpoint across a refresh.
/// An endpoint without a unique id is therefore left out of the list rather than
/// handed an index that would name a different device after a hot-plug.
fn endpoint_id(object: &coremidi::Object) -> Option<EndpointId> {
    let unique: std::result::Result<i32, _> = Properties::unique_id().value_from(object);
    unique.ok().map(|id| EndpointId::from_raw(id as u32 as u64))
}

/// The macOS [`MidiEndpoints`] implementation.
pub struct CoreMidiEndpoints {
    /// The process-wide client ([`shared_client`]); `None` when CoreMIDI refused
    /// it.
    client: Option<&'static Client>,
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
    /// Creates the backend on the process-wide CoreMIDI client.
    ///
    /// A failed client is kept as `None` rather than panicking: enumeration then
    /// reports nothing and opening reports an error, which is what a headless
    /// machine or a sandbox without MIDI access should look like.
    pub fn new() -> Self {
        Self {
            client: shared_client().ok(),
        }
    }

    fn client(&self) -> Result<&'static Client> {
        self.client.ok_or(Error::CoreMidi {
            operation: "create client",
            status: -1,
        })
    }

    fn find_source(id: EndpointId) -> Option<Source> {
        (0..Sources::count())
            .filter_map(Source::from_index)
            .find(|s| endpoint_id(s) == Some(id))
    }

    fn find_destination(id: EndpointId) -> Option<coremidi::Destination> {
        (0..Destinations::count())
            .filter_map(coremidi::Destination::from_index)
            .find(|d| endpoint_id(d) == Some(id))
    }
}

impl MidiEndpoints for CoreMidiEndpoints {
    fn inputs(&self) -> Vec<EndpointInfo> {
        (0..Sources::count())
            .filter_map(Source::from_index)
            .enumerate()
            .filter_map(|(index, source)| {
                Some(EndpointInfo {
                    id: endpoint_id(&source)?,
                    name: endpoint_name(&source, index),
                    capability: endpoint_capability(&source),
                })
            })
            .collect()
    }

    fn outputs(&self) -> Vec<EndpointInfo> {
        (0..Destinations::count())
            .filter_map(coremidi::Destination::from_index)
            .enumerate()
            .filter_map(|(index, dest)| {
                Some(EndpointInfo {
                    id: endpoint_id(&dest)?,
                    name: endpoint_name(&dest, index),
                    capability: endpoint_capability(&dest),
                })
            })
            .collect()
    }

    fn open_input(
        &self,
        id: EndpointId,
        mut producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let client = self.client()?;
        let source = Self::find_source(id)
            .ok_or_else(|| Error::MidiDevice(format!("no MIDI source with id {}", id.raw())))?;

        // No `Sysex7ByteAssembler` on this path, deliberately. The port opens
        // with `Protocol::Midi20`, so CoreMIDI delivers UMP words — a MIDI-1.0
        // device's SysEx arrives already fragmented into UMP SysEx7 packets,
        // which `Sysex7PacketReassembler` (one layer up) rejoins. The byte-run
        // assembler is for a transport that delivers raw `F0 … F7` bytes.
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

        Ok(Box::new(CoreMidiInput { _port: port }))
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
    /// Held for its `Drop`: releasing the port is what disconnects.
    _port: InputPortWithContext<()>,
}

// SAFETY: `InputPortWithContext` owns an opaque `MIDIPortRef` — a `UInt32`
// handle into CoreMIDI, whose objects are internally synchronised. The same
// contract `UmpVirtualSource` relies on.
unsafe impl Send for CoreMidiInput {}

impl InputConnection for CoreMidiInput {}

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
    /// Returns how many events reached the endpoint.
    ///
    /// Stops at the first failure rather than skipping it, so the count names an
    /// unbroken **prefix** of `events`. Skipping would report a number no
    /// contiguous run matches, and would deliver a note-off whose note-on never
    /// left. The ALSA backend's `queue` behaves identically.
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
mod tests;
