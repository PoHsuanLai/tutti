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
    pub sample_offset: i32,
    pub note_id: i32,
    pub type_id: u32,
    pub text: String,
}

/// Integer-valued per-note expression (stepped / enumerated dimensions).
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionIntValue {
    pub sample_offset: i32,
    pub note_id: i32,
    pub type_id: u32,
    pub value: i64,
}

/// Current chord context: root + bass note (0..127), a degree `mask`, and a
/// display name.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ChordValue {
    pub sample_offset: i32,
    pub root: i16,
    pub bass_note: i16,
    pub mask: i16,
    pub text: String,
}

/// Current scale/key context: root (0..127) + a 12-bit degree `mask`, and a
/// display name.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ScaleValue {
    pub sample_offset: i32,
    pub root: i16,
    pub mask: i16,
    pub text: String,
}

macro_rules! changes_container {
    ($(#[$m:meta])* $name:ident, $item:ty, $field:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Default)]
        #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
        pub struct $name {
            pub $field: SmallVec<[$item; STACK_CAPACITY]>,
        }

        impl $name {
            pub fn new() -> Self {
                Self::default()
            }

            pub fn add_change(&mut self, change: $item) {
                self.$field.push(change);
            }

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
