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
use smallvec::SmallVec;

/// Sequencer-context inputs (chord / scale / per-note text / int expression).
/// These live in one optional bundle rather than as loose fields on the
/// universal [`ProcessContext`]. The host sends this bundle only when the
/// plugin advertised [`Features::SEQUENCER_CONTEXT`](crate::Features); a plugin
/// that didn't leaves this `None`. (Today only the VST3 loader reads it — that
/// is a fact about the format landscape, not a gate: the gate is the feature
/// flag.)
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
    /// Retired [`ParameterQueue`]s, kept only for the `points` buffers inside
    /// them.
    ///
    /// [`clear`](Self::clear) moves the block's queues here instead of dropping
    /// them, and [`emit_param_point`](Self::emit_param_point) takes one back
    /// when it needs a new queue. Without this the two-tier reset is
    /// unachievable: a `ParameterQueue` owns its `points`, so dropping the
    /// queue frees that buffer no matter how carefully it was cleared first.
    ///
    /// Not `pub`, because it is storage rather than data — a reader of
    /// `param_changes` must not see last block's queues.
    retired: SmallVec<[ParameterQueue; 16]>,
}

impl ProcessOutput {
    /// Empty every list, keeping the storage behind it.
    ///
    /// Called at the top of each block, and the reason this type is reused at
    /// all. The parameter half is the part that is easy to get wrong twice:
    ///
    /// - Clearing only `queues` drops each [`ParameterQueue`] **and the
    ///   `points` buffer it owns**, so a plugin automating the same parameter
    ///   every block reallocates that buffer every block.
    /// - Clearing each `points` first and *then* the queue list looks like it
    ///   fixes that, and does not: the drop still happens, one line later.
    ///
    /// So the queues are **moved aside** into `retired` rather than dropped,
    /// and [`emit_param_point`](Self::emit_param_point) draws from that pool.
    /// The queues cannot simply be left in place: one is addressed by
    /// `param_id`, and the next block may automate an entirely different set,
    /// so a reader must not find last block's.
    pub fn clear(&mut self) {
        self.midi_events.clear();
        // `drain` empties `queues` while leaving its own capacity intact, and
        // each moved queue carries its `points` allocation into the pool.
        for mut queue in self.param_changes.queues.drain(..) {
            queue.points.clear();
            self.retired.push(queue);
        }
        self.note_expression.changes.clear();
    }

    /// Append one automation point, reusing a pooled queue when a new one is
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
