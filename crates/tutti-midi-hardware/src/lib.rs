#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` call into ALSA and CoreMIDI is in `tutti-alsa-ffi` and
// `tutti-coremidi-ffi` (`scripts/check-midi-ffi.sh`).
#![forbid(unsafe_code)]

// --- Framework-free hardware I/O core ---

// No `///` on these: a doc comment on a `pub mod` shadows the module's own
// `//!` and re-resolves its intra-doc links in this scope.
mod backend;
mod block;
mod capability;
mod devices;
mod endpoints;
mod error;
mod port;
mod session;
mod stamp;
mod stream;
#[cfg(test)]
mod testing;
mod wire;

// A backend with no OS behind it, for testing the session layer. See the
// module's own docs for why it is exported rather than `#[cfg(test)]`.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use capability::{EndpointInfo, UmpCapability};
pub use devices::{
    ChangeSink, DeviceEvents, Devices, OpenInput, OpenOutput, RecvTimeoutError, SeedError,
    WatchFailure,
};
pub use endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
pub use error::{Error, Result};
pub use port::{HardwareMidiInputs, InputProducerHandle, PortId, PortInfo};
pub use session::MidiSession;
pub use stream::{Basis, DeviceStream};
pub use tutti_midi_device::{
    Capabilities, DeviceEvent, DeviceId, DeviceInfo, Direction, Endpoint, EndpointId,
    FunctionBlock, Identity, OsChange, OsDevice, Resolved,
};

#[cfg(target_os = "macos")]
pub use backend::coremidi::{UmpVirtualDestination, UmpVirtualSource};

// --- Runtime delivery (the graph's MIDI border) ---
//
// The nodes a port feeds (`MidiIngestNode`, over the `DeviceInlets` each port is a slot of), the
// ring MIDI crosses threads on (`MidiMailbox`/`MidiSender`/`MidiReceiver`) and
// clip playback live in `tutti-midi-runtime`; surfaced here because delivery
// is what a port feeds, so a hardware consumer needs both. That is the test a
// file codec fails: a `.mid` reader needs no port, and no port needs it.

pub use tutti_midi_runtime::{
    DeviceInlets, MidiIngestNode, MidiMailbox, MidiOut, MidiReceiver, MidiSender, NoFreeSlot,
    TimedMidiEvent,
};

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs,
// and a consumer that wants only the former should not link CoreMIDI. Depend
// on `tutti-midi-file` directly.

/// The common types for a hardware MIDI app, for
/// `use tutti_midi_hardware::prelude::*;`.
///
/// It holds this crate's I/O and delivery (the packets themselves are
/// `tutti_ump`'s):
///
/// - **Hardware I/O** — [`MidiSession`] (enumerate / connect / send).
/// - **Delivery** — [`MidiIngestNode`] (the ingest of the input ports as a graph
///   node), [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] (the lock-free
///   ring), and [`TimedMidiEvent`] (a clip's events).
///
/// The sync decoders (`MidiClockDecoder`, `MtcDecoder`) are
/// `tutti-midi-runtime`'s, and the MPE zone types `tutti-midi-protocol`'s;
/// this crate does not re-export them. The SMF / Clip File codecs are not here at all:
/// they are `tutti-midi-file`'s, and this crate does not re-export them.
///
/// ```
/// use tutti_midi_hardware::prelude::*;
/// use tutti_fixtures::midi::note_on;
/// use tutti_graph::Offset;
/// use tutti_ump::{MidiChannel, MidiGroup, Ump};
///
/// // Build an event, and hand it across threads with the delivery types.
/// let ump = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
/// let (tx, rx) = MidiMailbox::pair();
/// tx.queue(&[(Offset::ZERO, ump)]);
/// let mut buf = [(0u32, Ump::default()); 4];
/// assert_eq!(rx.poll_into(&mut buf), 1);
/// ```
pub mod prelude {
    pub use crate::{MidiIngestNode, MidiMailbox, MidiReceiver, MidiSender, TimedMidiEvent};

    pub use crate::MidiSession;
}

// This crate is OS MIDI I/O plus the value types a host drives. The ECS
// bindings that drive them — routing, sequence, scheduled dispatch, clock-out,
// track-out, metadata, device management, MPE — live in
// `bevy_tutti::midi`.
