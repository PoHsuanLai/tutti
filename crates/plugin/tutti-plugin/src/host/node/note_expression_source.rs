//! Beat-scheduled note-expression as a per-block producer — the rail only.
//!
//! The note-expression counterpart of [`super::harmony_source::HarmonySource`]:
//! a [`NoteExpressionSource`] is a [`BlockInput`] that fills a
//! [`NoteExpressionChanges`] for the plugins that declared
//! [`Features::NOTE_EXPRESSION`](crate::protocol::Features::NOTE_EXPRESSION),
//! installed into the gated `note_expression` [`InputSlot`] on the plugin node.
//!
//! **Reader deferred.** The producer rail (this type + its slot + the
//! `set_note_expression_source` install seam + the `NOTE_EXPRESSION` gate) exists
//! so note-expression rides the same declare/install/drain-with-gate machinery as
//! transport / harmony / param-automation, closing the gap where the
//! `note_expression` payload field was hard-coded empty with no producer behind
//! it. The concrete *source of the data* — a note-expression lane reader — is not
//! built yet (no expression-lane storage exists), so `refill` currently emits
//! nothing. When lane storage lands, only [`NoteExpressionSource::refill`] changes;
//! the wiring is already in place.

use std::sync::Arc;

use tutti_core::transport::Timeline;
use tutti_core::SampleRate;

use crate::host::node::input_slot::{BlockCtx, BlockInput, BlockReset};
use crate::protocol::NoteExpressionChanges;

/// Beat-scheduled note-expression producer. Cheap to clone (transport shared via
/// `Arc`) so the fundsp graph-commit clone of the parent node doesn't restart it.
#[derive(Clone)]
pub struct NoteExpressionSource {
    // Held for parity with the other beat-scheduled sources and so the reader,
    // when built, can compute this block's beat window without a signature
    // change. Read once lane storage exists.
    #[allow(dead_code)]
    transport: Arc<dyn Timeline>,
    /// Typed because the reader this is held for will hand it to
    /// `BeatCursor::new`, which takes `impl Into<SampleRate>`, as the harmony
    /// source's does. This field crosses no boundary at
    /// all: no ABI, no wire, no fundsp call.
    #[allow(dead_code)]
    sample_rate: SampleRate,
}

impl NoteExpressionSource {
    /// Creates a source bound to `transport`, stamped at `sample_rate`.
    pub fn new(transport: Arc<dyn Timeline>, sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            transport,
            sample_rate: sample_rate.into(),
        }
    }

    /// The transport this source reads.
    pub(crate) fn timeline(&self) -> &Arc<dyn Timeline> {
        &self.transport
    }

    /// Fill `out` (cleared first) with this block's note-expression samples.
    ///
    /// Reader deferred: with no note-expression lane storage yet, this clears
    /// `out` and returns — the plugin receives an empty batch, identical to the
    /// prior hard-coded-empty behaviour, but now through the uniform slot so the
    /// data source can be dropped in without touching the wiring.
    pub fn refill(&self, _block_size: usize, out: &mut NoteExpressionChanges) {
        out.changes.clear();
    }
}

impl BlockInput for NoteExpressionSource {
    type Out = NoteExpressionChanges;
    fn refill(&self, ctx: BlockCtx, out: &mut NoteExpressionChanges) {
        NoteExpressionSource::refill(self, ctx.block_size, out);
    }
}

impl BlockReset for NoteExpressionChanges {
    fn reset(&mut self) {
        self.changes.clear();
    }
}
