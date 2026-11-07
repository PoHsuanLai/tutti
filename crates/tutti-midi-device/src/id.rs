//! What names a device: the OS's handle, what survives a restart, and the id
//! built from it.

use core::fmt;

/// The OS's handle to one endpoint, opaque and backend-owned.
///
/// Not an index: ALSA's `(client, port)` pair and CoreMIDI's unique id differ in
/// shape, and an index is only good until the next hot-plug. A handle the OS
/// has since given to another device opens that device, so a holder watches for
/// the `Removed` that retires it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(u64);

impl EndpointId {
    /// Mint a handle from a backend's own identifier. CoreMIDI stores a
    /// `MIDIUniqueID`; ALSA packs `(client << 32) | port`.
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The backend's identifier. Meaningful only to the backend that minted it.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// What a device is known by, as far as the OS lets it be known.
///
/// With the `serde` feature the document names the variant, so renaming one
/// breaks saved files.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Identity {
    /// The UMP Product Instance Id: the device's own, the same across restarts
    /// and across ports.
    Product(String),
    /// CoreMIDI's `MIDIUniqueID`, persistent across restarts.
    CoreMidi(i32),
    /// ALSA names only: a client number is a handle to the current connection,
    /// so the client and port name stand in.
    Alsa {
        /// The sequencer client's name.
        client: String,
        /// The port's name.
        port: String,
    },
}

impl Identity {
    /// An ALSA device known by its client and port name.
    #[must_use]
    pub fn alsa(client: impl Into<String>, port: impl Into<String>) -> Self {
        Self::Alsa {
            client: client.into(),
            port: port.into(),
        }
    }
}

/// A device's id: its [`Identity`] and an ordinal that tells apart devices the
/// OS names alike.
///
/// Two identical keyboards share an identity; the table gives the first to
/// appear ordinal 0 and the next 1. An ordinal is free again once its device is
/// gone.
///
/// An id names a device within one run. The ordinal says which of several alike
/// devices this is *now*, in the order they appeared, and not which physical
/// unit: unplug A (#0) and B (#1) and plug B back first, and B is #0. Across a
/// restart or a replug, an id of a device with a one-of-a-kind identity (a
/// Product Instance Id, a CoreMIDI unique id, or an ALSA name no other device
/// shares) names the same device again; one with an ordinal above 0 does not.
///
/// A saved id is restored with [`DeviceTable::resolve`](crate::DeviceTable::resolve),
/// never compared with a listed id by hand.
///
/// The serde form is the plain derive. The fields are private, but there is no
/// raw to refuse: any identity string and any `u32` ordinal is an id the table
/// could have minted, so `new` has no check for a `try_from` to share. The
/// document names the `Identity` variants, so renaming one breaks saved files.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DeviceId {
    identity: Identity,
    ordinal: u32,
}

impl DeviceId {
    pub(crate) fn new(identity: Identity, ordinal: u32) -> Self {
        Self { identity, ordinal }
    }

    /// What the OS knows the device by.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Which of the devices sharing [`identity`](Self::identity) this is.
    #[must_use]
    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.identity {
            Identity::Product(id) => write!(f, "product:{id}")?,
            Identity::CoreMidi(id) => write!(f, "coremidi:{id}")?,
            Identity::Alsa { client, port } => write!(f, "alsa:{client}/{port}")?,
        }
        write!(f, "#{}", self.ordinal)
    }
}

/// Which way MIDI flows through a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Direction {
    /// It sends MIDI to this engine.
    Input,
    /// This engine sends MIDI to it.
    Output,
    /// Both: one port carries each way.
    Duplex,
}

impl Direction {
    /// Whether the engine can receive from it.
    #[must_use]
    pub fn is_input(self) -> bool {
        matches!(self, Self::Input | Self::Duplex)
    }

    /// Whether the engine can send to it.
    #[must_use]
    pub fn is_output(self) -> bool {
        matches!(self, Self::Output | Self::Duplex)
    }
}

/// One device as the table lists it. An app that remembers a device keeps
/// this whole (with the `serde` feature) and restores it with
/// [`DeviceTable::resolve`](crate::DeviceTable::resolve): the name and direction
/// tell the ports of one unit apart. A field added later takes
/// `#[serde(default)]`, so files saved before it still load.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DeviceInfo {
    /// How to open it.
    pub id: DeviceId,
    /// The name to show, as the OS reports it.
    pub name: String,
    /// Which way MIDI flows.
    pub direction: Direction,
}
