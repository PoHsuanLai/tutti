#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

// --- Framework-free hardware I/O core ---

// No `///` on these: a doc comment on a `pub mod` shadows the module's own
// `//!` and re-resolves its intra-doc links in this scope.
mod backend;
pub mod capability;
pub mod devices;
pub mod endpoints;
pub mod error;
pub mod port;
pub mod session;

// A backend with no OS behind it, for testing the session layer. See the
// module's own docs for why it is exported rather than `#[cfg(test)]`.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use capability::{EndpointInfo, UmpCapability};
pub use devices::{
    ChangeSink, DeviceEvents, Devices, OpenInput, OpenOutput, RecvTimeoutError, WatchFailure,
};
pub use endpoints::{InputConnection, MidiEndpoints, Watch};
pub use error::{Error, Result};
pub use port::{HardwareMidiDrain, HardwareMidiInputs, InputProducerHandle, PortId, PortInfo};
pub use session::MidiSession;
pub use tutti_midi_device::{
    DeviceEvent, DeviceId, DeviceInfo, Direction, EndpointId, Identity, OsChange, OsDevice,
    Resolved,
};

#[cfg(target_os = "macos")]
pub use backend::coremidi::{UmpVirtualDestination, UmpVirtualSource};

// --- Re-exports from tutti-midi-types (the decoded view, until stage 5b part (c)) ---

pub use tutti_midi_types::{
    ControllerNamespace, MidiMessage, NoteAttribute, PerNoteController, UnencodableMessage,
    normalize,
};

pub use tutti_midi_runtime::{
    ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteFrameRate, SmpteTimecode,
};

// --- Runtime delivery (the graph's MIDI border) ---
//
// The nodes a port feeds and is fed by (`MidiInputNode`, `MidiOutNode`), the
// ring MIDI crosses threads on (`MidiMailbox`/`MidiSender`/`MidiReceiver`) and
// clip playback live in `tutti-midi-runtime`; surfaced here because delivery
// is what a port feeds, so a hardware consumer needs both. That is the test a
// file codec fails: a `.mid` reader needs no port, and no port needs it.

pub use tutti_midi_runtime::{
    MidiIn, MidiInputNode, MidiMailbox, MidiOut, MidiOutNode, MidiReceiver, MidiSender,
    TimedMidiEvent,
};

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs,
// and a consumer that wants only the former should not link CoreMIDI. Depend
// on `tutti-midi-file` directly for `smf`/`clip`.

/// The common types for a hardware MIDI app, for
/// `use tutti_midi_hardware::prelude::*;`.
///
/// It re-exports [`tutti_midi_types::prelude`] (the wire event + decoded view +
/// clip-file codec + per-note identity) and adds this crate's I/O and delivery:
///
/// - **Hardware I/O** — [`MidiSession`] (enumerate / connect / send).
/// - **Delivery** — [`MidiInputNode`] / [`MidiOutNode`] (the ports as graph
///   nodes), [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] (the lock-free
///   ring), and [`TimedMidiEvent`] (a clip's events).
///
/// The rarer surfaces (sync decoders such as [`MidiClockDecoder`], MPE zone
/// config) are not in the prelude; import them from the
/// crate root by name. The SMF / Clip File codecs are not here at all:
/// they are `tutti-midi-file`'s, and this crate does not re-export them.
///
/// ```
/// use tutti_midi_hardware::prelude::*;
/// use tutti_fixtures::midi::note_on;
/// use tutti_graph::Offset;
/// use tutti_ump::{MidiChannel, MidiGroup, Ump};
///
/// // The types prelude comes along: build + decode an event.
/// let ump = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
/// assert!(MidiMessage::from_ump(ump).is_note_on());
///
/// // And the delivery types are here too — hand an event across threads.
/// let (tx, rx) = MidiMailbox::pair();
/// tx.queue(&[(Offset::ZERO, ump)]);
/// let mut buf = [(0u32, Ump::default()); 4];
/// assert_eq!(rx.poll_into(&mut buf), 1);
/// ```
pub mod prelude {
    pub use tutti_midi_types::prelude::*;

    pub use crate::{
        MidiInputNode, MidiMailbox, MidiOutNode, MidiReceiver, MidiSender, TimedMidiEvent,
    };

    pub use crate::MidiSession;
}

// This crate is OS MIDI I/O plus the value types a host drives. The ECS
// bindings that drive them — routing, sequence, scheduled dispatch, clock-out,
// track-out, metadata, device management, MPE — live in
// `bevy_tutti::midi`.
