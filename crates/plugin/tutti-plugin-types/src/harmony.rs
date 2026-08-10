//! Harmony / sequencer-context vocabulary: chord, scale, per-note text, and
//! integer-valued per-note expression.
//!
//! These are inputs the host *sequencer* supplies (a chord track, a key/scale
//! lane, per-note annotations), not derived from MIDI. Only the VST3 host
//! consumes them today (other formats ignore them), but they're cross-format
//! vocabulary so the protocol and any future consumer speak one definition.
//! Text is an owned UTF-8 `String` here; the VST3 boundary converts it to
//! UTF-16.
//!
//! The `Serialize`/`Deserialize` derives are gated behind the `serde`
//! feature.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

const STACK_CAPACITY: usize = 4;

/// Per-note text annotation (lyric, ornament, …) bound to a `note_id`.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionTextValue {
    /// Frame offset from the start of the process block.
    pub sample_offset: i32,
    /// The note this annotates, as a host note id — see
    /// [`note_id_for`](crate::note_id_for).
    pub note_id: i32,
    /// Which text dimension this is, in the format's own numbering. VST3 passes
    /// it through to the plugin verbatim; nothing here interprets it.
    pub type_id: u32,
    /// The annotation, owned UTF-8. The VST3 boundary converts it to UTF-16.
    pub text: String,
}

/// Integer-valued per-note expression (stepped / enumerated dimensions).
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionIntValue {
    /// Frame offset from the start of the process block.
    pub sample_offset: i32,
    /// The note this drives, as a host note id — see
    /// [`note_id_for`](crate::note_id_for).
    pub note_id: i32,
    /// Which stepped dimension this is, in the format's own numbering. Passed
    /// through to the plugin verbatim; nothing here interprets it.
    pub type_id: u32,
    /// The step, in whatever space `type_id` names. Not normalized — the
    /// stepped dimensions are exactly the ones a `0..=1` scale cannot carry.
    pub value: i64,
}

/// Current chord context: root + bass note (0..127), a degree `mask`, and a
/// display name.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ChordValue {
    /// Frame offset from the start of the process block, at which the chord
    /// takes effect.
    pub sample_offset: i32,
    /// Root pitch as a MIDI note number, `0..=127`.
    pub root: i16,
    /// Bass pitch as a MIDI note number, `0..=127`. Equal to `root` for a chord
    /// in root position; different for a slash chord.
    pub bass_note: i16,
    /// The chord's degrees as a 12-bit mask relative to `root`, bit `n` set when
    /// semitone `n` sounds.
    pub mask: i16,
    /// Display name (`"Cmaj7"`), owned UTF-8. Presentation only — the degrees a
    /// plugin acts on are in `mask`. The VST3 boundary converts it to UTF-16.
    pub text: String,
}

/// Current scale/key context: root (0..127) + a 12-bit degree `mask`, and a
/// display name.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ScaleValue {
    /// Frame offset from the start of the process block, at which the scale
    /// takes effect.
    pub sample_offset: i32,
    /// Tonic pitch as a MIDI note number, `0..=127`.
    pub root: i16,
    /// The scale's degrees as a 12-bit mask relative to `root`, bit `n` set when
    /// semitone `n` is in the scale.
    pub mask: i16,
    /// Display name (`"D Dorian"`), owned UTF-8. Presentation only — the degrees
    /// a plugin acts on are in `mask`. The VST3 boundary converts it to UTF-16.
    pub text: String,
}

macro_rules! changes_container {
    ($(#[$m:meta])* $name:ident, $item:ty, $field:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Default)]
        #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
        pub struct $name {
            /// The block's events, in the order the sequencer appended them.
            /// The first `STACK_CAPACITY` live inline, so a typical block adds
            /// no allocation on the RT path.
            pub $field: SmallVec<[$item; STACK_CAPACITY]>,
        }

        impl $name {
            /// Builds an empty container.
            pub fn new() -> Self {
                Self::default()
            }

            /// Appends one event. Order is the caller's to maintain; nothing
            /// here sorts by `sample_offset`.
            pub fn add_change(&mut self, change: $item) {
                self.$field.push(change);
            }

            /// Returns `true` when the block carries no event of this kind.
            pub fn is_empty(&self) -> bool {
                self.$field.is_empty()
            }
        }
    };
}

changes_container!(
    /// Per-block chord events.
    ChordChanges, ChordValue, changes
);
changes_container!(
    /// Per-block scale events.
    ScaleChanges, ScaleValue, changes
);
changes_container!(
    /// Per-block per-note text events.
    NoteExpressionTextChanges, NoteExpressionTextValue, changes
);
changes_container!(
    /// Per-block integer per-note expression events.
    NoteExpressionIntChanges, NoteExpressionIntValue, changes
);
