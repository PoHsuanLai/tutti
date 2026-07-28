//! Boxed bulk payload for the `ProcessAudio` wire message.

use serde::{Deserialize, Serialize};

use super::midi::IpcMidiEventVec;
use super::{
    ChordChanges, NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionTextChanges,
    ParameterChanges, ScaleChanges, TransportInfo,
};

/// One process block's inputs: which block this is, the sample count, and all
/// the per-block side-band (MIDI, automation, note-expression, VST3
/// sequencer-context, transport). Audio itself travels in the shared
/// `AudioSlab`; everything here rides the control socket alongside it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessAudioData {
    /// Which block this is. Monotonic per plugin instance from 1, and the index
    /// into the slab's ring: the server reads `seq`'s input slot and publishes
    /// into `seq`'s output slot, so both sides address the shared region by this
    /// number alone. `u64` rather than the `u32` id it replaced — a sequence
    /// that wraps could be matched by the wrong block.
    pub seq: u64,
    pub num_samples: usize,
    pub midi_events: IpcMidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
    /// VST3 sequencer-context inputs (chord / scale / per-note text / int
    /// expression). VST3-only; other formats ignore them. Empty until a host
    /// produces them.
    pub chords: ChordChanges,
    pub scales: ScaleChanges,
    pub expr_texts: NoteExpressionTextChanges,
    pub expr_ints: NoteExpressionIntChanges,
    pub transport: TransportInfo,
}
