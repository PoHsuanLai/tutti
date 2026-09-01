//! Generic per-block input producer for a plugin node.
//!
//! Several host-produced inputs — chord/scale harmony, sample-accurate
//! parameter automation, transport info — share one shape: *given this block's
//! size, produce a payload the plugin consumes this block, reading the live
//! transport as needed.* [`BlockInput`] names that shape; [`InputSlot`] holds
//! one installed producer and drains it per block.
//!
//! **Why the slot is a shared cell.** fundsp's frontend/backend split means the
//! box the audio thread runs is a *different clone* than the one a host-side
//! `set_*_source` call mutates, and `Net::migrate` discards `node_mut`/clone
//! edits on commit. So the *slot itself* is an `Arc<ArcSwapOption<…>>` shared
//! across clones: an install on any clone is seen live by whichever clone the
//! audio thread runs, lock-free, no commit needed. The contract lives here
//! once rather than copy-pasted into each producer slot, because a slot that
//! forgets to share silently never fires. See
//! [[plugin-source-install-shared-cell]].
//!
//! **Feature gating is data, not logic.** Some inputs are only sent to plugins
//! that advertised wanting them (`Features::SEQUENCER_CONTEXT` for harmony,
//! `Features::TRANSPORT` for transport); parameter automation is universal
//! (empty gate ⇒ always send). The gate rides on the slot as a `Features`
//! value, so the send decision is one uniform check, never a per-producer
//! special case.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::protocol::Features;

/// Per-block context handed to a [`BlockInput`]. Currently just the block size —
/// the producers read the live transport directly, so none needs a host-supplied
/// sample position (only [`super::PluginClient`]'s MIDI path tracks one, and it
/// stays outside this abstraction). Widen the struct if a future producer needs
/// more; it's a struct, not a positional arg, so that's a non-breaking change.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockCtx {
    pub block_size: usize,
}

/// Reset an input's output buffer to "nothing this block" **without
/// reallocating**. This is the RT-critical half of [`InputSlot::drain`]: the
/// common case is a plugin with no source installed (or gated off), which runs
/// every block, so it must not allocate. `clear()` on the SmallVec-backed change
/// lists keeps their capacity; `TransportInfo` resets to its default snapshot.
pub(crate) trait BlockReset {
    fn reset(&mut self);
}

/// Something the host produces once per audio block and feeds to the plugin.
/// The producer reads the live transport itself (it holds its own
/// `Arc<dyn Timeline>`), so `refill` needs only the block size via `ctx`.
pub(crate) trait BlockInput: Send + Sync {
    /// The per-block payload this input fills. Reused across blocks as scratch.
    type Out: Default + BlockReset;

    /// Fill `out` for this block. Implementations must first `out.reset()` (or
    /// otherwise fully overwrite it) so no stale data from a prior block leaks.
    fn refill(&self, ctx: BlockCtx, out: &mut Self::Out);
}

/// One installed [`BlockInput`], shared across fundsp graph-commit clones.
///
/// `source` is the shared cell (see module docs); `drain` is per-clone scratch
/// (each clone fills its own, no cross-talk). `gate` is the feature the plugin
/// must advertise to receive this input — `Features::empty()` means "always
/// send" (the input is universal).
pub(crate) struct InputSlot<B: BlockInput> {
    source: Arc<ArcSwapOption<B>>,
    gate: Features,
    drain: B::Out,
}

impl<B: BlockInput> InputSlot<B> {
    /// A slot gated on `gate` (`Features::empty()` = always send), with no source
    /// installed yet.
    pub(crate) fn new(gate: Features) -> Self {
        Self {
            source: Arc::new(ArcSwapOption::empty()),
            gate,
            drain: B::Out::default(),
        }
    }

    /// Install (or replace) the producer. Visible to every clone immediately.
    pub(crate) fn install(&self, source: Arc<B>) {
        self.source.store(Some(source));
    }

    /// Drop the producer; subsequent blocks drain empty.
    pub(crate) fn clear(&self) {
        self.source.store(None);
    }

    /// The shared source cell, for callers that need to reach the installed
    /// producer directly (e.g. to poke a live-updatable field like the transport
    /// sample rate). Returns the `ArcSwapOption` so the caller `load()`s it.
    pub(crate) fn source_ref(&self) -> &ArcSwapOption<B> {
        &self.source
    }

    /// Fill and return this block's payload.
    ///
    /// Returns the reset (empty) payload when the plugin lacks the gated feature
    /// or no source is installed — the RT-common path, allocation-free via
    /// [`BlockReset::reset`]. Otherwise delegates to the source's `refill`.
    pub(crate) fn drain(&mut self, ctx: BlockCtx, plugin_features: Features) -> &B::Out {
        if !self.gate.is_empty() && !plugin_features.contains(self.gate) {
            self.drain.reset();
            return &self.drain;
        }
        // `load()` is lock-free; the guard holds the current source for the refill.
        match self.source.load().as_ref() {
            Some(src) => src.refill(ctx, &mut self.drain),
            None => self.drain.reset(),
        }
        &self.drain
    }
}

impl<B: BlockInput> Clone for InputSlot<B> {
    fn clone(&self) -> Self {
        Self {
            // Share the SLOT (Arc clone), not the Option — a later install on any
            // clone must be visible to whichever clone the audio thread runs.
            // Cloning the Option instead was the historical shared-cell bug.
            source: Arc::clone(&self.source),
            gate: self.gate,
            drain: B::Out::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Output buffer for the dummy input: a count, resettable to 0.
    #[derive(Default)]
    struct Count(usize);
    impl BlockReset for Count {
        fn reset(&mut self) {
            self.0 = 0;
        }
    }

    /// A dummy input that writes a fixed value into its `Count`, and records how
    /// many times `refill` ran (to prove the gate suppresses it).
    struct Dummy {
        value: usize,
        refills: AtomicUsize,
    }
    impl BlockInput for Dummy {
        type Out = Count;
        fn refill(&self, _ctx: BlockCtx, out: &mut Count) {
            self.refills.fetch_add(1, Ordering::Relaxed);
            out.0 = self.value;
        }
    }

    const CTX: BlockCtx = BlockCtx { block_size: 64 };

    /// Regression guard for [[plugin-source-install-shared-cell]], generalized:
    /// installing on ONE clone is visible to ANOTHER clone (shared slot).
    #[test]
    fn install_propagates_across_clones() {
        let mut original: InputSlot<Dummy> = InputSlot::new(Features::empty());
        let clone_a = original.clone();
        let mut clone_b = original.clone();

        clone_a.install(Arc::new(Dummy {
            value: 7,
            refills: AtomicUsize::new(0),
        }));

        assert_eq!(
            clone_b.drain(CTX, Features::empty()).0,
            7,
            "clone_b sees install"
        );
        assert_eq!(
            original.drain(CTX, Features::empty()).0,
            7,
            "original sees install"
        );

        clone_b.clear();
        assert_eq!(
            original.drain(CTX, Features::empty()).0,
            0,
            "clear propagates"
        );
    }

    /// An empty gate always sends; a non-empty gate suppresses the refill entirely
    /// (no allocation, `refill` never runs) when the plugin lacks the feature.
    #[test]
    fn gate_suppresses_refill_when_feature_absent() {
        let mut slot: InputSlot<Dummy> = InputSlot::new(Features::TRANSPORT);
        let src = Arc::new(Dummy {
            value: 5,
            refills: AtomicUsize::new(0),
        });
        slot.install(Arc::clone(&src));

        // Plugin lacks TRANSPORT → gated off, refill never runs, output stays reset.
        assert_eq!(slot.drain(CTX, Features::empty()).0, 0);
        assert_eq!(
            src.refills.load(Ordering::Relaxed),
            0,
            "refill suppressed by gate"
        );

        // Plugin has TRANSPORT → refills.
        assert_eq!(slot.drain(CTX, Features::TRANSPORT).0, 5);
        assert_eq!(src.refills.load(Ordering::Relaxed), 1);
    }

    /// Empty gate = universal: sends regardless of plugin features.
    #[test]
    fn empty_gate_always_sends() {
        let mut slot: InputSlot<Dummy> = InputSlot::new(Features::empty());
        slot.install(Arc::new(Dummy {
            value: 9,
            refills: AtomicUsize::new(0),
        }));
        assert_eq!(slot.drain(CTX, Features::empty()).0, 9);
        assert_eq!(slot.drain(CTX, Features::TRANSPORT).0, 9);
    }
}
