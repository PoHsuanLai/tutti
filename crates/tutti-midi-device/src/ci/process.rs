//! The Process Inquiry messages (M2-101 §9, Tables 40 to 46): the capabilities
//! inquiry and its reply, and the three messages that bracket a MIDI Message
//! Report.
//!
//! Every limit the tables set is a type. The three bitmaps of which messages to
//! report are flag sets ([`SystemMessages`], [`ChannelControllers`], [`NoteData`])
//! that hold no reserved bit: `from_bits` refuses one, and a message read from the
//! wire drops one a later revision may define. The Message Data Control is the
//! closed set [`DataControl`]. Which bitmaps a request may carry depends on its
//! Device ID (§9.5: System Messages are requested of the Function Block alone),
//! so the Device ID and the bitmaps are one value, [`ReportScope`], and a system
//! bitmap on a Channel or a Group cannot be built. Both ends of the wire are
//! held to it: a message that breaks it decodes as an
//! [`Unread`](super::Unread) for the NAK, and the Device ID of these messages is
//! the one the message or its scope fixes, never the header's.

use bitflags::bitflags;
use tutti_ump::MidiChannel;

use super::CiDeviceId;

/// Sub-ID#2: Inquiry: Process Inquiry Capabilities (M2-101 §9.2).
pub const SUB_ID2_PROCESS_CAPABILITIES_INQUIRY: u8 = 0x40;
/// Sub-ID#2: Reply to Process Inquiry Capabilities (§9.3).
pub const SUB_ID2_PROCESS_CAPABILITIES_REPLY: u8 = 0x41;
/// Sub-ID#2: Inquiry: MIDI Message Report (§9.5).
pub const SUB_ID2_MESSAGE_REPORT_INQUIRY: u8 = 0x42;
/// Sub-ID#2: Reply to MIDI Message Report (§9.6).
pub const SUB_ID2_MESSAGE_REPORT_REPLY: u8 = 0x43;
/// Sub-ID#2: End of MIDI Message Report (§9.8).
pub const SUB_ID2_MESSAGE_REPORT_END: u8 = 0x44;

bitflags! {
    /// The Process Inquiry features a Responder supports (Table 42, the Supported
    /// Features bitmap of the reply to the capabilities inquiry).
    ///
    /// Bits D1 to D6 are reserved.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct ProcessFeatures: u8 {
        /// MIDI Message Report (§9.4 to §9.8).
        const MIDI_MESSAGE_REPORT = 1 << 0;
    }
}

bitflags! {
    /// The System Messages of a MIDI Message Report (Tables 43 and 45, the first
    /// bitmap).
    ///
    /// Bits D3 to D7 are reserved, and so is the byte after the bitmap, which
    /// is always 0.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct SystemMessages: u8 {
        /// MTC Quarter Frame.
        const MTC_QUARTER_FRAME = 1 << 0;
        /// Song Position.
        const SONG_POSITION = 1 << 1;
        /// Song Select.
        const SONG_SELECT = 1 << 2;
    }
}

bitflags! {
    /// The Channel Controller Messages of a MIDI Message Report (the second
    /// bitmap).
    ///
    /// Bits D6 and D7 are reserved.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct ChannelControllers: u8 {
        /// Pitch Bend.
        const PITCH_BEND = 1 << 0;
        /// Control Change.
        const CONTROL_CHANGE = 1 << 1;
        /// Registered Controller (RPN).
        const REGISTERED_CONTROLLER = 1 << 2;
        /// Assignable Controller (NRPN).
        const ASSIGNABLE_CONTROLLER = 1 << 3;
        /// Program Change.
        const PROGRAM_CHANGE = 1 << 4;
        /// Channel Pressure.
        const CHANNEL_PRESSURE = 1 << 5;
    }
}

bitflags! {
    /// The Note Data Messages of a MIDI Message Report (the third bitmap).
    ///
    /// Bits D5 to D7 are reserved. Asking for the notes that are playing is for
    /// compliance testing only (§9.5).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct NoteData: u8 {
        /// Notes: Note On and Note Off.
        const NOTES = 1 << 0;
        /// Poly Pressure.
        const POLY_PRESSURE = 1 << 1;
        /// Per-Note Pitch Bend.
        const PER_NOTE_PITCH_BEND = 1 << 2;
        /// Registered Per-Note Controller.
        const REGISTERED_PER_NOTE_CONTROLLER = 1 << 3;
        /// Assignable Per-Note Controller.
        const ASSIGNABLE_PER_NOTE_CONTROLLER = 1 << 4;
    }
}

/// How much a Responder reports (§9.5.1, Table 44): the Message Data Control of
/// an Inquiry: MIDI Message Report.
///
/// `0x02` to `0x7E` are reserved and a message carrying one is not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub enum DataControl {
    /// `0x00`: no data, the Reply and the End alone, which shows which messages
    /// the Responder can report.
    Capabilities,
    /// `0x01`: only the controllers with non-default values, the notes that are
    /// active and the per-note controllers with non-default values.
    NonDefault,
    /// `0x7F`: everything requested and supported.
    Full,
}

impl DataControl {
    /// The control, or `None` for a reserved value (`0x02..=0x7E`) or one with the
    /// top bit set.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        match raw {
            0x00 => Some(Self::Capabilities),
            0x01 => Some(Self::NonDefault),
            0x7F => Some(Self::Full),
            _ => None,
        }
    }

    /// The Message Data Control byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        match self {
            Self::Capabilities => 0x00,
            Self::NonDefault => 0x01,
            Self::Full => 0x7F,
        }
    }
}

/// A raw value that is not a Message Data Control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#04x} is not a MIDI Message Report data control (reserved)", .0)]
pub struct NotADataControl(pub u8);

impl TryFrom<u8> for DataControl {
    type Error = NotADataControl;

    fn try_from(raw: u8) -> Result<Self, NotADataControl> {
        Self::new(raw).ok_or(NotADataControl(raw))
    }
}

impl From<DataControl> for u8 {
    fn from(control: DataControl) -> u8 {
        control.get()
    }
}

/// Where a MIDI Message Report is, and which kinds of message it covers: the
/// Device ID and the three bitmaps of an Inquiry: MIDI Message Report or of its
/// Reply (§9.5, §9.6).
///
/// System Messages are requested of the Function Block alone: only that variant
/// has the bitmap. Asked of the Function Block, the channelized bitmaps cover
/// every Group and Channel of it; asked of a Group, every Channel of the Group;
/// asked of a Channel, that Channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReportScope {
    /// The Function Block (`0x7F`).
    FunctionBlock {
        /// The System Messages.
        system: SystemMessages,
        /// The Channel Controller Messages of every Channel.
        controllers: ChannelControllers,
        /// The Note Data Messages of every Channel.
        notes: NoteData,
    },
    /// The Group (`0x7E`).
    Group {
        /// The Channel Controller Messages of every Channel of the Group.
        controllers: ChannelControllers,
        /// The Note Data Messages of every Channel of the Group.
        notes: NoteData,
    },
    /// One Channel (`0x00` to `0x0F`).
    Channel {
        /// The Channel.
        channel: MidiChannel,
        /// Its Channel Controller Messages.
        controllers: ChannelControllers,
        /// Its Note Data Messages.
        notes: NoteData,
    },
}

impl ReportScope {
    /// A scope at `address` that covers nothing: what a Responder with nothing
    /// to report declares.
    #[must_use]
    pub const fn none(address: CiDeviceId) -> Self {
        let (controllers, notes) = (ChannelControllers::empty(), NoteData::empty());
        match address {
            CiDeviceId::FunctionBlock => Self::FunctionBlock {
                system: SystemMessages::empty(),
                controllers,
                notes,
            },
            CiDeviceId::Group => Self::Group { controllers, notes },
            CiDeviceId::Channel(channel) => Self::Channel {
                channel,
                controllers,
                notes,
            },
        }
    }

    /// The scope a message's three bitmap bytes and Device ID describe, or `None`
    /// when System Messages are set at a Device ID other than the Function
    /// Block's. A bit this revision reserves is dropped, as §5.3 has a receiver
    /// read only the bits of its own version.
    #[must_use]
    pub const fn from_wire(
        address: CiDeviceId,
        system: u8,
        controllers: u8,
        notes: u8,
    ) -> Option<Self> {
        let system = SystemMessages::from_bits_truncate(system);
        let controllers = ChannelControllers::from_bits_truncate(controllers);
        let notes = NoteData::from_bits_truncate(notes);
        match address {
            CiDeviceId::FunctionBlock => Some(Self::FunctionBlock {
                system,
                controllers,
                notes,
            }),
            _ if !system.is_empty() => None,
            CiDeviceId::Group => Some(Self::Group { controllers, notes }),
            CiDeviceId::Channel(channel) => Some(Self::Channel {
                channel,
                controllers,
                notes,
            }),
        }
    }

    /// The Device ID of the message.
    #[must_use]
    pub const fn address(&self) -> CiDeviceId {
        match self {
            Self::FunctionBlock { .. } => CiDeviceId::FunctionBlock,
            Self::Group { .. } => CiDeviceId::Group,
            Self::Channel { channel, .. } => CiDeviceId::Channel(*channel),
        }
    }

    /// The System Messages; empty away from the Function Block.
    #[must_use]
    pub const fn system(&self) -> SystemMessages {
        match self {
            Self::FunctionBlock { system, .. } => *system,
            _ => SystemMessages::empty(),
        }
    }

    /// The Channel Controller Messages.
    #[must_use]
    pub const fn controllers(&self) -> ChannelControllers {
        match self {
            Self::FunctionBlock { controllers, .. }
            | Self::Group { controllers, .. }
            | Self::Channel { controllers, .. } => *controllers,
        }
    }

    /// The Note Data Messages.
    #[must_use]
    pub const fn notes(&self) -> NoteData {
        match self {
            Self::FunctionBlock { notes, .. }
            | Self::Group { notes, .. }
            | Self::Channel { notes, .. } => *notes,
        }
    }

    /// Whether no kind of message is covered.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.system().is_empty() && self.controllers().is_empty() && self.notes().is_empty()
    }

    /// What this covers of `requested`: the kinds in both, at this scope's
    /// address. A Reply may not set a bit its Inquiry did not (§9.6).
    #[must_use]
    pub fn within(&self, requested: &Self) -> Self {
        let (system, controllers, notes) = (
            self.system() & requested.system(),
            self.controllers() & requested.controllers(),
            self.notes() & requested.notes(),
        );
        match *self {
            Self::FunctionBlock { .. } => Self::FunctionBlock {
                system,
                controllers,
                notes,
            },
            Self::Group { .. } => Self::Group { controllers, notes },
            Self::Channel { channel, .. } => Self::Channel {
                channel,
                controllers,
                notes,
            },
        }
    }

    /// The three bitmap bytes and the reserved byte as they go on the wire
    /// (Tables 43 and 45): system, reserved `0x00`, controllers, notes.
    pub(crate) fn bitmap_bytes(&self) -> [u8; 4] {
        [
            self.system().bits(),
            0x00,
            self.controllers().bits(),
            self.notes().bits(),
        ]
    }
}

#[cfg(test)]
mod tests;
