//! VST3 event types and bidirectional conversion to/from the Tutti
//! [`tutti_midi_types::MidiEvent`] UMP representation.
//!
//! VST3's native event list is richer than raw MIDI-1 wire data (typed
//! note-on/off/poly-pressure structs with `f32` velocity plus generic
//! `Data` events for CC / ProgramChange / ChannelPressure / PitchBend). The
//! plugin's event list encodes the workspace's canonical [`MidiEvent`] UMP
//! type into it (at the stream's per-note bend ranges), and
//! [`Vst3Event::to_midi`] decodes it back — one `MidiEvent` maps to one
//! [`Vst3Event`] and round-trips losslessly for the MIDI-representable
//! variants.

//! # Narrowing casts are denied in this module
//!
//! This file is a boundary between the engine's vocabulary and the VST3 C ABI, and every
//! bug it has had was a cast that silently changed a value's meaning: a `u32`
//! frame offset wrapping negative into an `i32` `sampleOffset`, and a release
//! velocity dropped on the way through. Both compiled without complaint.
//!
//! So truncating and sign-changing casts are denied here. Where a cast is
//! genuinely safe, the `#[allow(..., reason = "...")]` states why — the
//! justification is the point, not the lint. A cast nobody can justify is the
//! bug.
//!
//! Scoped to this module deliberately. Enabling these lints crate-wide produces
//! hundreds of warnings that get scrolled past, which is how the two above
//! survived review; a small denied surface that must be argued with is worth
//! more than a large warned one that is not read.
#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

pub use tutti_midi_types::MidiEvent;

use tutti_plugin_types::{
    NoteExpressionType, NoteExpressionValue, SysExRef, is_usable, note_id_for,
};

use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup, PerNoteBendRanges};
use vst3::Steinberg::Vst::Event_::EventTypes_;

mod c_event;
mod data;
mod event;
mod harmony;
mod legacy_cc;
mod midi;
mod note_expression;
mod notes;
mod text;

pub use c_event::*;
pub use data::*;
pub use event::*;
pub use harmony::*;
pub use legacy_cc::*;
pub use midi::*;
pub use note_expression::*;
pub use notes::*;
pub use text::*;

/// Borrowed bundle of every input event stream staged into a VST3 plugin's
/// event list for one processing block: MIDI plus the four per-note-expressive
/// families (chords, scales, expression texts, integer expressions). Grouping
/// them keeps the `process` / `update_from_sources` signatures readable rather
/// than threading six parallel slices.
#[derive(Clone, Copy, Default, Debug)]
pub struct Vst3InputEvents<'a> {
    /// UMP MIDI for the block, in ascending frame-offset order.
    pub midi: &'a [MidiEvent],
    /// Continuous per-note expression values, addressed by note id.
    pub note_expressions: &'a [NoteExpressionValue],
    /// Chord context changes (root, bass, degree mask) for chord-aware plugins.
    pub chords: &'a [ChordValue],
    /// Scale/key context changes (root plus a 12-bit degree mask).
    pub scales: &'a [ScaleValue],
    /// Per-note text annotations; the strings are copied into the event list's
    /// UTF-16 arena during staging.
    pub expr_texts: &'a [NoteExpressionText],
    /// Stepped/enumerated per-note expressions carrying an `i64` rather than a
    /// normalized `f64`.
    pub expr_ints: &'a [NoteExpressionIntValue],
}

impl Vst3InputEvents<'_> {
    /// True when no stream carries an event this block, so staging can be
    /// skipped entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.midi.is_empty()
            && self.note_expressions.is_empty()
            && self.chords.is_empty()
            && self.scales.is_empty()
            && self.expr_texts.is_empty()
            && self.expr_ints.is_empty()
    }
}

/// `type_` discriminant for note-on events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_ON_EVENT: u16 = EventTypes_::kNoteOnEvent as u16;
/// `type_` discriminant for note-off events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_OFF_EVENT: u16 = EventTypes_::kNoteOffEvent as u16;
/// `type_` discriminant for raw-data events (CC, pitch bend, program change, …).
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_DATA_EVENT: u16 = EventTypes_::kDataEvent as u16;
/// `type_` discriminant for poly-pressure events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_POLY_PRESSURE_EVENT: u16 = EventTypes_::kPolyPressureEvent as u16;
/// `type_` discriminant for note-expression value events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_EXPRESSION_VALUE_EVENT: u16 = EventTypes_::kNoteExpressionValueEvent as u16;
/// `type_` discriminant for note-expression *text* events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_EXPRESSION_TEXT_EVENT: u16 = EventTypes_::kNoteExpressionTextEvent as u16;
/// `type_` discriminant for chord events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_CHORD_EVENT: u16 = EventTypes_::kChordEvent as u16;
/// `type_` discriminant for scale events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_SCALE_EVENT: u16 = EventTypes_::kScaleEvent as u16;
/// `type_` discriminant for note-expression integer-value events.
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_NOTE_EXPRESSION_INT_VALUE_EVENT: u16 = EventTypes_::kNoteExpressionIntValueEvent as u16;
/// `type_` discriminant for legacy-MIDI-CC-out events (plugin → host, value 0xFFFF).
#[allow(
    clippy::cast_possible_truncation,
    reason = "SDK event-type ordinals, all < 16"
)]
pub const K_LEGACY_MIDI_CC_OUT_EVENT: u16 = EventTypes_::kLegacyMIDICCOutEvent as u16;
/// `DataEvent.type` subtype marking the payload as a MIDI SysEx message.
///
/// Cast explicitly like the discriminants above: the generated `vst3` bindings
/// give this constant `u32` on Unix but `i32` on Windows, so an uncast
/// initializer only compiles on one of them.
// The cast is a no-op on Unix (where the binding is already `u32`) and a real
// `i32 as u32` on Windows; allow the former's `unnecessary_cast` lint.
#[allow(clippy::unnecessary_cast)]
pub const K_DATA_TYPE_MIDI_SYSEX: u32 =
    vst3::Steinberg::Vst::DataEvent_::DataTypes_::kMidiSysEx as u32;

/// Flat Rust-facing header merging the `busIndex` / `sampleOffset` /
/// `ppqPosition` / `flags` / `type_` fields of `vst3::Steinberg::Vst::Event`
/// so callers can construct events literally.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventHeader {
    /// Event bus index (0 for typical single-bus plugins).
    pub bus_index: i32,
    /// Frame offset within the current processing block.
    pub sample_offset: i32,
    /// Musical position in quarter notes, or 0 if unknown.
    pub ppq_position: f64,
    /// Flags bitfield (see VST3 `EventFlags`).
    pub flags: u16,
    /// One of the `K_*_EVENT` discriminants.
    pub event_type: u16,
}

/// MIDI round-trip tests through `vst3_event_from_midi` + `Vst3Event::to_midi`.
///
/// Tests build SDK structs by hand, so they repeat the same bounded narrowings
/// the production code justifies above (SDK ordinals, text sizes clamped to
/// `MAX_EVENT_TEXT_LEN`). Allowed at module scope rather than per case: a test
/// that overflows one of these fails loudly on its own assertion, so the lint
/// adds nothing here, and the deny is kept meaningful by staying tight in the
/// code that actually crosses the ABI.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[cfg(test)]
mod tests;
