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
    ParamAddress, ParameterChanges, ParameterQueue, ScaleChanges, TransportInfo,
};

/// Sequencer-context inputs for one block: chords, scales, per-note text and
/// integer per-note expression.
///
/// The host sends this bundle as [`ProcessContext::expressive`] only when the
/// plugin advertised
/// [`Features::SEQUENCER_CONTEXT`](crate::Features::SEQUENCER_CONTEXT). Only the
/// VST3 loader reads it.
#[derive(Default)]
pub struct ExpressiveContext<'a> {
    /// Chord events in effect this block. `None` when the host sends no chord
    /// track — not "no chord".
    pub chords: Option<&'a ChordChanges>,
    /// Scale/key events in effect this block. `None` when the host sends no
    /// key lane.
    pub scales: Option<&'a ScaleChanges>,
    /// Per-note text annotations for this block.
    pub expr_texts: Option<&'a NoteExpressionTextChanges>,
    /// Per-note stepped expression for this block, for the dimensions a `0..=1`
    /// scale cannot carry.
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
    /// MIDI for this block, in ascending time order. An empty slice rather than
    /// an `Option` — every format takes MIDI, so there is no "did not send"
    /// state to distinguish from "sent nothing".
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
    /// Builds a context with no MIDI and every best-effort input absent.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches this block's MIDI (builder).
    pub fn midi(mut self, events: &'a [MidiEvent]) -> Self {
        self.midi_events = events;
        self
    }

    /// Attaches parameter automation (builder). Call only when the plugin
    /// advertised [`Features::PARAM_AUTOMATION`](crate::Features).
    pub fn params(mut self, changes: &'a ParameterChanges) -> Self {
        self.param_changes = Some(changes);
        self
    }

    /// Attaches note expression (builder). Call only when the plugin advertised
    /// [`Features::NOTE_EXPRESSION`](crate::Features).
    pub fn note_expression(mut self, changes: &'a NoteExpressionChanges) -> Self {
        self.note_expression = Some(changes);
        self
    }

    /// Attaches transport state (builder). Call only when the plugin advertised
    /// [`Features::TRANSPORT`](crate::Features).
    pub fn transport(mut self, info: &'a TransportInfo) -> Self {
        self.transport = Some(info);
        self
    }

    /// Attaches sequencer context (builder). Call only when the plugin
    /// advertised [`Features::SEQUENCER_CONTEXT`](crate::Features).
    pub fn expressive(mut self, ctx: ExpressiveContext<'a>) -> Self {
        self.expressive = Some(ctx);
        self
    }
}

/// Per-block outputs from
/// [`PluginAudio::process`](crate::PluginAudio::process) beyond the
/// audio buffer.
///
/// **Owned by the caller and reused across blocks.** `process` takes one as an
/// out-parameter and fills it; the caller keeps it, so its heap capacity
/// survives from block to block and the realtime path stops allocating once
/// warm. Reset it with [`clear`](Self::clear), never by assigning a fresh
/// value — that discards exactly the capacity the reuse exists for.
#[derive(Default)]
pub struct ProcessOutput {
    /// MIDI the plugin emitted this block — a note effect's output, or an
    /// instrument echoing what it consumed.
    ///
    /// Capped at [`MIDI_STACK_CAPACITY`](crate::MIDI_STACK_CAPACITY): fill it
    /// through [`emit_midi`](Self::emit_midi), which drops and counts what
    /// does not fit. `MidiEventVec::extend` panics past the cap.
    pub midi_events: MidiEventVec,
    /// Parameter changes the plugin made itself, so the host can follow a knob
    /// the user turned in the plugin's own editor.
    ///
    /// Fill it through [`emit_param_point`](Self::emit_param_point), not
    /// `ParameterChanges::add_change`, when this value is being reused across
    /// blocks — see that method for why the difference is an allocation.
    pub param_changes: ParameterChanges,
    /// Note expression the plugin emitted this block.
    pub note_expression: NoteExpressionChanges,
    // Retired `ParameterQueue`s, kept only for the `points` buffers inside
    // them: `clear` moves the block's queues here and `emit_param_point` takes
    // one back when it needs a new queue. Private because it is storage rather
    // than data. A plain `Vec`: it keeps its capacity across `clear`, so it
    // grows only while the plugin automates more parameters than ever before.
    retired: Vec<ParameterQueue>,
    // MIDI events `emit_midi` dropped this block for want of room.
    midi_dropped: usize,
}

impl ProcessOutput {
    /// Empties every list, keeping the storage behind it.
    ///
    /// Call at the top of each block. The block's [`ParameterQueue`]s are
    /// moved into an internal pool rather than dropped, so their `points`
    /// buffers survive for [`emit_param_point`](Self::emit_param_point) to
    /// reuse; a reader of `param_changes` never sees last block's queues.
    // Clearing `queues` directly would drop each queue and the `points` buffer
    // it owns, so a plugin automating the same parameter every block would
    // reallocate that buffer every block.
    pub fn clear(&mut self) {
        self.midi_events.clear();
        self.midi_dropped = 0;
        // `drain` empties `queues` while leaving its own capacity intact, and
        // each moved queue carries its `points` allocation into the pool.
        for mut queue in self.param_changes.queues.drain(..) {
            queue.points.clear();
            self.retired.push(queue);
        }
        self.note_expression.changes.clear();
    }

    /// Appends `events` to [`midi_events`](Self::midi_events) until it is
    /// full, and drops the rest. Returns how many were dropped, which also
    /// adds to [`midi_dropped`](Self::midi_dropped). Allocation-free: the
    /// list is inline and capped.
    pub fn emit_midi(&mut self, events: impl IntoIterator<Item = MidiEvent>) -> usize {
        let mut dropped = 0;
        for e in events {
            if self.midi_events.try_push(e).is_err() {
                dropped += 1;
            }
        }
        self.midi_dropped += dropped;
        dropped
    }

    /// The MIDI events [`emit_midi`](Self::emit_midi) dropped since the last
    /// [`clear`](Self::clear), for want of room: the plugin emitted more than
    /// [`MIDI_STACK_CAPACITY`](crate::MIDI_STACK_CAPACITY) in one block.
    pub fn midi_dropped(&self) -> usize {
        self.midi_dropped
    }

    /// Appends one automation point, reusing a pooled queue when a new one is
    /// needed.
    ///
    /// Behaves like [`ParameterChanges::add_change`] — same matching on the
    /// full [`ParamAddress`], same clamping of `value` — but takes its queues
    /// from [`clear`](Self::clear)'s pool rather than constructing them. That
    /// is the whole difference, and it is the difference between a warm block
    /// allocating nothing and allocating once per automated parameter.
    ///
    /// A pool miss (the first blocks, or a plugin that automates more
    /// parameters than ever before) falls back to constructing a queue, which
    /// is the warm-up cost this design accepts.
    pub fn emit_param_point(&mut self, param_id: ParamAddress, sample_offset: i32, value: f64) {
        if let Some(queue) = self
            .param_changes
            .queues
            .iter_mut()
            .find(|q| q.param_id == param_id)
        {
            queue.add_point(sample_offset, value);
            return;
        }
        let mut queue = match self.retired.pop() {
            Some(mut pooled) => {
                // The pooled queue is another parameter's; only its `points`
                // capacity is wanted, so re-address it.
                pooled.param_id = param_id;
                pooled.points.clear();
                pooled
            }
            None => ParameterQueue::new(param_id),
        };
        queue.add_point(sample_offset, value);
        self.param_changes.queues.push(queue);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIDI_STACK_CAPACITY;

    /// **`emit_midi` fills to the cap, drops and counts the rest, and never
    /// panics**; `clear` resets the count with the list. What lets a loader
    /// copy a plugin's MIDI-out whatever its size without growing (or, with
    /// `extend`, panicking on) the inline list.
    ///
    /// Mutation (run): `emit_midi` through `extend` → panics past 256 →
    /// fails; the count not added to `midi_dropped` → fails.
    #[test]
    fn emit_midi_caps_and_counts() {
        let mut out = ProcessOutput::default();
        let events =
            (0..MIDI_STACK_CAPACITY as u32 + 44).map(|i| MidiEvent::from_ump(i, &[0x2090_3C40]));
        assert_eq!(out.emit_midi(events), 44);
        assert_eq!(out.midi_events.len(), MIDI_STACK_CAPACITY);
        assert_eq!(
            out.midi_events[MIDI_STACK_CAPACITY - 1].frame_offset,
            255,
            "the first kept"
        );
        assert_eq!(out.emit_midi([MidiEvent::from_ump(0, &[0x2090_3C40])]), 1);
        assert_eq!(out.midi_dropped(), 45);
        out.clear();
        assert_eq!(out.midi_dropped(), 0);
        assert!(out.midi_events.is_empty());
    }
}
