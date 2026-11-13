#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]

// --- Framework-free hardware I/O core ---

// No `///` on these: a doc comment on a `pub mod` shadows the module's own
// `//!` and re-resolves its intra-doc links in this scope.
mod backend;
// Compiled everywhere so its tests run on every platform; only the native
// backends (ALSA, CoreMIDI) convert an OS block, so it is dead where the stub
// is the backend.
#[cfg_attr(
    not(any(target_os = "macos", all(target_os = "linux", alsa_ump))),
    allow(dead_code, reason = "only a native backend converts OS blocks")
)]
mod block;
mod capability;
mod ci;
mod devices;
mod endpoints;
mod error;
// Only a native backend's callback runs through it; the stub has none.
#[cfg_attr(
    not(any(target_os = "macos", all(target_os = "linux", alsa_ump))),
    allow(dead_code, reason = "only a native backend has a driver callback")
)]
mod guard;
mod port;
mod profiles;
mod session;
mod stamp;
mod stream;
#[cfg(test)]
mod testing;
mod wire;

// A backend with no OS behind it, for testing the session layer. See the
// module's own docs for why it is exported rather than `#[cfg(test)]`.
#[cfg(any(test, feature = "testing"))]
pub mod test_support;

pub use capability::{EndpointInfo, UmpCapability};
pub use ci::{CiChange, CiPeerInfo};
pub use devices::{
    ChangeSink, DeviceEvents, Devices, OpenInput, OpenOutput, Pending, RecvTimeoutError, SeedError,
    WatchFailure,
};
pub use endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
pub use error::{Error, Result};
pub use port::{HardwareMidiInputs, InputProducerHandle, PortInfo, Pushed, StreamDrain};
pub use profiles::{
    ChannelCount, ChannelProfile, ListedProfile, PeerProfiles, ProfileError, ProfileSwitch,
    ProfilesChange, Whole, WholeProfile,
};
pub use session::{DeviceQuery, MidiSession, Polled};
pub use stream::{Basis, DeviceStream};
pub use tutti_midi_device::{
    BindPolicy, Capabilities, DeviceEvent, DeviceId, DeviceInfo, Endpoint, EndpointId,
    FunctionBlock, Identity, Learned, Match, OsChange, OsDevice, PortDirection, Resolved,
    SavedDevice,
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
    DeviceInlets, DeviceSlot, MidiIngestNode, MidiMailbox, MidiOut, MidiReceiver, MidiSender,
    NoFreeSlot, TimedMidiEvent,
};

// --- Standard MIDI File codec: NOT here ---
//
// The file codecs live in `tutti-midi-file` and are deliberately *not*
// re-exported. Reading a `.mid` and talking to a MIDI port are different jobs,
// and a consumer that wants only the former should not link CoreMIDI. Depend
// on `tutti-midi-file` directly.

// This crate is OS MIDI I/O plus the value types a host drives.
