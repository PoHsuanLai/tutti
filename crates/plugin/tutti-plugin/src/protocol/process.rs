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
    /// number alone. `u64` is wide enough that it never wraps, so a reply can
    /// never be matched to the wrong block.
    pub seq: u64,
    /// Block length in **frames**, not samples — the slab's stride carries the
    /// channel count.
    pub num_samples: usize,
    /// MIDI to deliver at the top of the block, inline up to
    /// `MIDI_STACK_CAPACITY`.
    pub midi_events: IpcMidiEventVec,
    /// Sample-accurate automation points to apply across the block.
    pub param_changes: ParameterChanges,
    /// Per-note expression (pitch, pressure, brightness) for the block.
    pub note_expression: NoteExpressionChanges,
    /// VST3 sequencer-context chords. VST3-only; other formats ignore it.
    pub chords: ChordChanges,
    /// VST3 sequencer-context scales. VST3-only; other formats ignore it.
    pub scales: ScaleChanges,
    /// VST3 per-note text expression. VST3-only; other formats ignore it.
    pub expr_texts: NoteExpressionTextChanges,
    /// VST3 per-note integer expression. VST3-only; other formats ignore it.
    pub expr_ints: NoteExpressionIntChanges,
    /// Tempo, time signature and play state for the block.
    pub transport: TransportInfo,
}
