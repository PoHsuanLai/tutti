//! Per-block process inputs/outputs beyond the audio buffer.
//!
//! [`ProcessContext`] gathers the best-effort per-block *inputs* (MIDI,
//! parameter automation, note expression, transport, sequencer context) a
//! plugin may consume; [`ProcessOutput`] gathers the per-block *outputs* it may
//! emit. Both are format-agnostic so the shared
//! [`PluginAudio`](crate::PluginAudio) trait and the four format host
//! crates speak the same vocabulary.

use tutti_graph::UmpAt;

use crate::{
    ChordChanges, MidiEventVec, NoteExpressionChanges, NoteExpressionTextChanges, ParamId,
    ParameterChanges, ScaleChanges, TransportChange, TransportInfo, WireMidi,
};

/// Sequencer-context inputs for one block: chords, scales, per-note text and
/// integer per-note expression.
///
/// The host sends this bundle as [`ProcessContext::expressive`] only when the
/// plugin advertised
/// [`Features::SEQUENCER_CONTEXT`](crate::Features::SEQUENCER_CONTEXT). Only the
/// VST3 loader reads it.
#[derive(Default, Debug)]
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
#[derive(Default, Debug)]
pub struct ProcessContext<'a> {
    /// MIDI for this block, in ascending time order. An empty slice rather than
    /// an `Option` — every format takes MIDI, so there is no "did not send"
    /// state to distinguish from "sent nothing".
    pub midi_events: &'a [UmpAt],
    /// Sent only when the plugin advertised [`Features::PARAM_AUTOMATION`](crate::Features).
    pub param_changes: Option<&'a ParameterChanges>,
    /// Sent only when the plugin advertised [`Features::NOTE_EXPRESSION`](crate::Features).
    pub note_expression: Option<&'a NoteExpressionChanges>,
    /// Sent only when the plugin advertised [`Features::TRANSPORT`](crate::Features).
    pub transport: Option<&'a TransportInfo>,
    /// The transport changes inside the block (for a chunked Processor, the
    /// chunk's so far: see [`Processor::chunk`](crate::Processor::chunk)), in time order, each at its
    /// frame ([`TransportChange`]); `transport` is the one at the first frame.
    /// Empty when there are none, or for a plugin told no transport. Only a
    /// format with an inter-block transport event reads it (CLAP); VST3,
    /// VST2 and AU have none and read `transport` alone.
    pub transport_changes: &'a [TransportChange],
    /// Sent only when the plugin advertised [`Features::SEQUENCER_CONTEXT`](crate::Features).
    pub expressive: Option<ExpressiveContext<'a>>,
}

impl<'a> ProcessContext<'a> {
    /// Builds a context with no MIDI and every best-effort input absent.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches this block's MIDI (builder).
    #[must_use]
    pub fn midi(mut self, events: &'a [UmpAt]) -> Self {
        self.midi_events = events;
        self
    }

    /// Attaches parameter automation (builder). Call only when the plugin
    /// advertised [`Features::PARAM_AUTOMATION`](crate::Features).
    #[must_use]
    pub fn params(mut self, changes: &'a ParameterChanges) -> Self {
        self.param_changes = Some(changes);
        self
    }

    /// Attaches note expression (builder). Call only when the plugin advertised
    /// [`Features::NOTE_EXPRESSION`](crate::Features).
    #[must_use]
    pub fn note_expression(mut self, changes: &'a NoteExpressionChanges) -> Self {
        self.note_expression = Some(changes);
        self
    }

    /// Attaches transport state (builder). Call only when the plugin advertised
    /// [`Features::TRANSPORT`](crate::Features).
    #[must_use]
    pub fn transport(mut self, info: &'a TransportInfo) -> Self {
        self.transport = Some(info);
        self
    }

    /// Attaches the transport changes inside the block (builder). Call only
    /// when the plugin advertised [`Features::TRANSPORT`](crate::Features).
    #[must_use]
    pub fn transport_changes(mut self, changes: &'a [TransportChange]) -> Self {
        self.transport_changes = changes;
        self
    }

    /// Attaches sequencer context (builder). Call only when the plugin
    /// advertised [`Features::SEQUENCER_CONTEXT`](crate::Features).
    #[must_use]
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
#[derive(Default, Debug)]
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
    /// Empty, with room for nothing, until
    /// [`reserve_params`](Self::reserve_params) sizes it; fill it through
    /// [`emit_param_point`](Self::emit_param_point), which never grows it.
    pub param_changes: ParameterChanges,
    /// Note expression the plugin emitted this block.
    pub note_expression: NoteExpressionChanges,
    // MIDI events `emit_midi` dropped this block for want of room.
    midi_dropped: usize,
    // Parameter writes the plugin refused this block.
    params_refused: usize,
    // MIDI events a chunked Processor accepted and could not send.
    midi_lost: usize,
    // Whether a chunk this call completed went without its parameter changes.
    params_unsent: bool,
}

impl ProcessOutput {
    /// Empties every list, keeping the storage behind it.
    ///
    /// Call at the top of each block. The block's parameter queues go back
    /// to `param_changes`' spares with their point storage
    /// ([`ParameterChanges::clear`]), so a reader of `param_changes` never
    /// sees last block's queues and the next block reuses their storage.
    pub fn clear(&mut self) {
        self.midi_events.clear();
        self.midi_dropped = 0;
        self.params_refused = 0;
        self.midi_lost = 0;
        self.params_unsent = false;
        self.param_changes.clear();
        self.note_expression.changes.clear();
    }

    /// Sizes [`param_changes`](Self::param_changes) for `queues` parameter
    /// queues (the plugin's parameter count) of up to `points` points each
    /// ([`ParameterChanges::reserved`]), dropping what it held. Allocates;
    /// call it off the audio thread, when the plugin is loaded.
    pub fn reserve_params(&mut self, queues: usize, points: usize) {
        self.param_changes = ParameterChanges::reserved(queues, points);
    }

    /// Appends `events` to [`midi_events`](Self::midi_events) until it is
    /// full, and drops the rest. Returns how many were dropped, which also
    /// adds to [`midi_dropped`](Self::midi_dropped). Allocation-free: the
    /// list is inline and capped.
    pub fn emit_midi(&mut self, events: impl IntoIterator<Item = WireMidi>) -> usize {
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

    /// Adds `n` to the parameter writes the plugin refused in this block: a
    /// format that applies the block's [`ProcessContext::param_changes`]
    /// itself and can be refused (VST2's `setParameter` on an index the
    /// plugin lacks) says how many. Reset by [`clear`](Self::clear).
    pub fn count_params_refused(&mut self, n: usize) {
        self.params_refused += n;
    }

    /// The parameter writes the plugin refused since the last
    /// [`clear`](Self::clear), as [`count_params_refused`](Self::count_params_refused)
    /// counted them.
    pub fn params_refused(&self) -> usize {
        self.params_refused
    }

    /// Adds `n` to the MIDI events a chunked Processor ([`Processor::chunk`](crate::Processor::chunk))
    /// took in earlier calls and lost when it shipped their chunk: the chunk
    /// could not be staged, or the peer refused it. For the graph node, which
    /// folds them into its dropped-MIDI counts and skips the rest of every
    /// SysEx open in them. Reset by [`clear`](Self::clear).
    pub fn count_midi_lost(&mut self, n: usize) {
        self.midi_lost += n;
    }

    /// The MIDI events counted by [`count_midi_lost`](Self::count_midi_lost)
    /// since the last [`clear`](Self::clear).
    pub fn midi_lost(&self) -> usize {
        self.midi_lost
    }

    /// Says the chunk this call completed shipped without its parameter
    /// changes (a chunked Processor could not stage it, or the peer refused
    /// it): the graph node owes each parameter's latest value to the next
    /// chunk. Reset by [`clear`](Self::clear).
    pub fn set_params_unsent(&mut self) {
        self.params_unsent = true;
    }

    /// Whether [`set_params_unsent`](Self::set_params_unsent) was called since
    /// the last [`clear`](Self::clear).
    pub fn params_unsent(&self) -> bool {
        self.params_unsent
    }

    /// Appends one automation point within the room
    /// [`reserve_params`](Self::reserve_params) gave
    /// [`param_changes`](Self::param_changes)
    /// ([`ParameterChanges::add_change`]: same matching on the full
    /// [`ParamId`], same clamping of `value`). Never allocates: past the
    /// room the parameter keeps its latest value, or a parameter past every
    /// queue is dropped, and `false` is returned, counted in the set's
    /// [`ParameterChanges::points_overflowed`].
    pub fn emit_param_point(&mut self, param_id: ParamId, sample_offset: i32, value: f64) -> bool {
        self.param_changes
            .add_change(param_id, sample_offset, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIDI_STACK_CAPACITY;
    use tutti_ump::Ump;

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
        let events = (0..MIDI_STACK_CAPACITY as u32 + 44).map(|i| WireMidi {
            offset: i,
            ump: Ump([0x2090_3C40, 0, 0, 0]),
        });
        assert_eq!(out.emit_midi(events), 44);
        assert_eq!(out.midi_events.len(), MIDI_STACK_CAPACITY);
        assert_eq!(
            out.midi_events[MIDI_STACK_CAPACITY - 1].offset,
            255,
            "the first kept"
        );
        assert_eq!(
            out.emit_midi([WireMidi {
                offset: 0,
                ump: Ump([0x2090_3C40, 0, 0, 0])
            }]),
            1
        );
        assert_eq!(out.midi_dropped(), 45);
        out.clear();
        assert_eq!(out.midi_dropped(), 0);
        assert!(out.midi_events.is_empty());
    }

    /// **The loss reports of a chunked Processor accumulate within a block and
    /// `clear` resets them**: the node reads them after each call, and a flag
    /// left from the last block would fold a loss twice.
    ///
    /// Mutation (run): `clear` not resetting `midi_lost` → the second read is
    /// 5 → fails; not resetting `params_unsent` → fails.
    #[test]
    fn loss_reports_accumulate_and_clear() {
        let mut out = ProcessOutput::default();
        assert!(!out.params_unsent());
        out.count_midi_lost(2);
        out.count_midi_lost(3);
        out.set_params_unsent();
        assert_eq!(out.midi_lost(), 5);
        assert!(out.params_unsent());
        out.clear();
        assert_eq!(out.midi_lost(), 0);
        assert!(!out.params_unsent());
    }
}
