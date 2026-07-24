//! Per-block process inputs/outputs beyond the audio buffer.
//!
//! [`ProcessContext`] gathers the best-effort per-block *inputs* (MIDI,
//! parameter automation, note expression, transport, sequencer context) a
//! plugin may consume; [`ProcessOutput`] gathers the per-block *outputs* it may
//! emit. Both are format-agnostic so the shared
//! [`PluginAudio`](crate::PluginAudio) trait and the four format host
//! crates speak the same vocabulary.

use crate::{
    ChordChanges, MidiEvent, MidiEventVec, NoteExpressionChanges, NoteExpressionTextChanges,
    ParameterChanges, ScaleChanges, TransportInfo,
};

/// Sequencer-context inputs (chord / scale / per-note text / int expression).
/// These live in one optional bundle rather than as loose fields on the
/// universal [`ProcessContext`]. The host sends this bundle only when the
/// plugin advertised [`Features::SEQUENCER_CONTEXT`](crate::Features); a plugin
/// that didn't leaves this `None`. (Today only the VST3 loader reads it — that
/// is a fact about the format landscape, not a gate: the gate is the feature
/// flag.)
#[derive(Default)]
pub struct ExpressiveContext<'a> {
    pub chords: Option<&'a ChordChanges>,
    pub scales: Option<&'a ScaleChanges>,
    pub expr_texts: Option<&'a NoteExpressionTextChanges>,
    pub expr_ints: Option<&'a crate::NoteExpressionIntChanges>,
}

/// Per-block inputs to [`PluginAudio::process`](crate::PluginAudio::process)
/// beyond the audio buffer.
///
/// Each best-effort field is `Some` only when the plugin advertised the
/// matching bit in [`Features::CONSUMES`](crate::Features) — the host gates the
/// send on the flag, never on the plugin's format.
#[derive(Default)]
pub struct ProcessContext<'a> {
    pub midi_events: &'a [MidiEvent],
    /// Sent only when the plugin advertised [`Features::PARAM_AUTOMATION`](crate::Features).
    pub param_changes: Option<&'a ParameterChanges>,
    /// Sent only when the plugin advertised [`Features::NOTE_EXPRESSION`](crate::Features).
    pub note_expression: Option<&'a NoteExpressionChanges>,
    /// Sent only when the plugin advertised [`Features::TRANSPORT`](crate::Features).
    pub transport: Option<&'a TransportInfo>,
    /// Sent only when the plugin advertised [`Features::SEQUENCER_CONTEXT`](crate::Features).
    pub expressive: Option<ExpressiveContext<'a>>,
}

impl<'a> ProcessContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn midi(mut self, events: &'a [MidiEvent]) -> Self {
        self.midi_events = events;
        self
    }

    pub fn params(mut self, changes: &'a ParameterChanges) -> Self {
        self.param_changes = Some(changes);
        self
    }

    pub fn note_expression(mut self, changes: &'a NoteExpressionChanges) -> Self {
        self.note_expression = Some(changes);
        self
    }

    pub fn transport(mut self, info: &'a TransportInfo) -> Self {
        self.transport = Some(info);
        self
    }

    pub fn expressive(mut self, ctx: ExpressiveContext<'a>) -> Self {
        self.expressive = Some(ctx);
        self
    }
}

/// Per-block outputs from
/// [`PluginAudio::process`](crate::PluginAudio::process) beyond the
/// audio buffer.
#[derive(Default)]
pub struct ProcessOutput {
    pub midi_events: MidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
}
