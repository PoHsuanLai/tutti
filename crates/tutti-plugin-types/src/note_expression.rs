//! Note-expression (MPE-style) vocabulary shared across the plugin host
//! crates and the `tutti-plugin` IPC protocol.
//!
//! [`NoteExpressionType`] is the **superset** of the dimensions any hosted
//! format speaks: the five VST3 standard dimensions plus CLAP's `Pressure`
//! and `Expression`. Decoding a plugin's output into this enum is always
//! lossless. *Encoding* toward a format that lacks a dimension is the
//! partial direction — each host crate owns that conversion and is explicit
//! about what it cannot represent (e.g. VST3 has no `typeId` for `Pressure`),
//! rather than silently coercing to a different dimension.
//!
//! The `Serialize`/`Deserialize` derives are gated behind the `serde`
//! feature (the IPC wire path enables it; pure in-process consumers don't
//! pay for it).

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

const NOTE_EXPR_STACK_CAPACITY: usize = 8;

/// A per-note expression dimension. The union of what VST3 and CLAP support;
/// VST3 covers the first five, CLAP adds [`Pressure`](Self::Pressure) and
/// [`Expression`](Self::Expression).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum NoteExpressionType {
    /// Per-note volume / loudness.
    Volume,
    /// Per-note pan.
    Pan,
    /// Per-note tuning, in the format's native convention.
    Tuning,
    /// Vibrato intensity.
    Vibrato,
    /// Brightness / filter cutoff.
    Brightness,
    /// Per-note pressure (poly aftertouch). CLAP-native; VST3 cannot encode
    /// this as a note-expression `typeId`.
    Pressure,
    /// Generic per-note expression. CLAP-native; VST3 cannot encode this as a
    /// note-expression `typeId`.
    Expression,
}

/// A single note-expression sample bound to a specific active voice
/// (`note_id`) at a frame offset within the current block.
///
/// This is the **neutral** value the protocol and VST3 speak. CLAP carries
/// extra voice-addressing fields (port / channel / key) in its own local
/// wrapper, converting to/from this shape at its boundary.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionValue {
    pub sample_offset: i32,
    pub note_id: i32,
    pub expression_type: NoteExpressionType,
    pub value: f64,
}

pub type NoteExpressionVec = SmallVec<[NoteExpressionValue; NOTE_EXPR_STACK_CAPACITY]>;

/// Per-block batch of note-expression samples.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionChanges {
    pub changes: NoteExpressionVec,
}

impl NoteExpressionChanges {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_change(&mut self, change: NoteExpressionValue) {
        self.changes.push(change);
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_change_collects() {
        let mut expr = NoteExpressionChanges::new();
        assert!(expr.is_empty());

        expr.add_change(NoteExpressionValue {
            sample_offset: 0,
            note_id: 1,
            expression_type: NoteExpressionType::Tuning,
            value: 0.5,
        });

        assert!(!expr.is_empty());
        assert_eq!(expr.changes.len(), 1);
        assert_eq!(expr.changes[0].expression_type, NoteExpressionType::Tuning);
    }
}
