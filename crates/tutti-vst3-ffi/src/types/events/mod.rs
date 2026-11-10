//! VST3 events: the SDK's tagged union as a Rust enum.
//!
//! An [`Event`] is a header plus a [`Payload`]; the wire tag ([`EventKind`]) is
//! derived from the payload, so the two cannot disagree. Payloads that are
//! plain data wrap the SDK struct unchanged; the ones that carry a pointer
//! (`Data`, chord, scale, text) hold an index into the owning list's storage
//! instead, which keeps [`Event`] `Copy`.
//!
//! Translation to and from MIDI lives in `tutti-vst3-host`.
//!
//! Narrowing casts are denied here: this is the boundary with the C ABI, where
//! a silent `u32` to `i32` wrap or a dropped field is a bug in the plugin's
//! view of the event. A cast that is sound says why in its `#[allow]`.
#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use tutti_ump::SysexRef;
use vst3::Steinberg::Vst::{self as sdk, Event_::EventTypes_};

mod c_event;
mod payload;
mod text;

pub use payload::{
    Chord, Data, ExpressionInt, ExpressionText, ExpressionValue, LegacyCcOut, NoteOff, NoteOn,
    PolyPressure, Scale,
};
pub use text::{TextArena, TextRef};

pub(crate) use text::MAX_EVENT_TEXT_LEN;

/// `DataEvent.type` subtype marking the payload as a MIDI SysEx message.
///
/// The bindings type this `u32` on Unix and `i32` on Windows.
pub const K_DATA_TYPE_MIDI_SYSEX: u32 =
    crate::sdk_enum_u32(sdk::DataEvent_::DataTypes_::kMidiSysEx);

/// An SDK event ordinal as the `u16` of `Event::type`, which every ordinal fits.
#[allow(clippy::cast_possible_truncation)]
const fn tag(ordinal: crate::SdkEnum) -> u16 {
    ordinal as u16
}

macro_rules! event_kinds {
    ($($(#[$m:meta])* $name:ident = $sdk:ident),* $(,)?) => {
        /// The `type` tag of an SDK event.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u16)]
        pub enum EventKind {
            $($(#[$m])* $name = tag(EventTypes_::$sdk)),*
        }

        impl TryFrom<u16> for EventKind {
            type Error = UnknownEventKind;

            fn try_from(tag: u16) -> Result<Self, UnknownEventKind> {
                match tag {
                    $(t if t == Self::$name as u16 => Ok(Self::$name),)*
                    other => Err(UnknownEventKind(other)),
                }
            }
        }
    };
}

event_kinds! {
    /// A note started.
    NoteOn = kNoteOnEvent,
    /// A note ended.
    NoteOff = kNoteOffEvent,
    /// Raw bytes (`DataEvent`): CC, pitch bend, program change or SysEx.
    Data = kDataEvent,
    /// Per-note aftertouch.
    PolyPressure = kPolyPressureEvent,
    /// A continuous per-note expression value.
    NoteExpressionValue = kNoteExpressionValueEvent,
    /// A per-note text annotation.
    NoteExpressionText = kNoteExpressionTextEvent,
    /// A chord context change.
    Chord = kChordEvent,
    /// A scale context change.
    Scale = kScaleEvent,
    /// A stepped per-note expression carrying an integer.
    NoteExpressionIntValue = kNoteExpressionIntValueEvent,
    /// A legacy MIDI CC the plugin sends to the host.
    LegacyMidiCcOut = kLegacyMIDICCOutEvent,
}

/// An `Event::type` tag the SDK does not define.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownEventKind(pub u16);

impl std::fmt::Display for UnknownEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown VST3 event type {}", self.0)
    }
}

impl std::error::Error for UnknownEventKind {}

/// The fields every SDK event shares.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EventHeader {
    /// Event bus index (0 for a single-bus plugin).
    pub bus_index: i32,
    /// Frame offset within the block.
    pub sample_offset: i32,
    /// Musical position in quarter notes, or 0 if unknown.
    pub ppq_position: f64,
    /// `EventFlags` bits.
    pub flags: u16,
}

impl EventHeader {
    /// A header at `sample_offset` on bus 0 with no position or flags.
    #[must_use]
    pub fn at(sample_offset: i32) -> Self {
        Self {
            sample_offset,
            ..Self::default()
        }
    }
}

/// What an [`Event`] carries; its variant is the event's [`EventKind`], except
/// that [`SysEx`](Self::SysEx) is a `Data` event too.
#[derive(Debug, Clone, Copy)]
pub enum Payload {
    /// A note started.
    NoteOn(NoteOn),
    /// A note ended, with its release velocity.
    NoteOff(NoteOff),
    /// Up to 16 raw bytes, inline.
    Data(Data),
    /// A SysEx message held in the owning list's buffer.
    SysEx(SysexRef),
    /// Per-note aftertouch.
    PolyPressure(PolyPressure),
    /// A continuous per-note expression value.
    ExpressionValue(ExpressionValue),
    /// A per-note text annotation, held in the list's arena.
    ExpressionText(ExpressionText),
    /// A chord context change.
    Chord(Chord),
    /// A scale context change.
    Scale(Scale),
    /// A stepped per-note expression.
    ExpressionInt(ExpressionInt),
    /// A legacy MIDI CC the plugin sent to the host.
    LegacyCcOut(LegacyCcOut),
}

impl Payload {
    /// The wire tag this payload is sent under.
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            Self::NoteOn(_) => EventKind::NoteOn,
            Self::NoteOff(_) => EventKind::NoteOff,
            Self::Data(_) | Self::SysEx(_) => EventKind::Data,
            Self::PolyPressure(_) => EventKind::PolyPressure,
            Self::ExpressionValue(_) => EventKind::NoteExpressionValue,
            Self::ExpressionText(_) => EventKind::NoteExpressionText,
            Self::Chord(_) => EventKind::Chord,
            Self::Scale(_) => EventKind::Scale,
            Self::ExpressionInt(_) => EventKind::NoteExpressionIntValue,
            Self::LegacyCcOut(_) => EventKind::LegacyMidiCcOut,
        }
    }
}

/// A VST3 event.
#[derive(Debug, Clone, Copy)]
pub struct Event {
    /// Bus, frame offset, position and flags.
    pub header: EventHeader,
    /// The event's data.
    pub payload: Payload,
}

impl Event {
    /// An event with `payload` at `sample_offset`.
    #[must_use]
    pub fn at(sample_offset: i32, payload: Payload) -> Self {
        Self {
            header: EventHeader::at(sample_offset),
            payload,
        }
    }

    /// Frame offset within the block.
    #[must_use]
    pub fn sample_offset(&self) -> i32 {
        self.header.sample_offset
    }
}

#[cfg(test)]
mod tests;
