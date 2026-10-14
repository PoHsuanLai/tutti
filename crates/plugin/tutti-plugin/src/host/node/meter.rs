//! The project meter a plugin node reads its time signature and bar with.

use std::sync::Arc;

use tutti_core::{MeterMap, RtPublish, RtRef};

/// What a [`PluginMeter`] publishes: the installed meter's cell, or none.
type Installed = Option<Arc<RtPublish<MeterMap>>>;

/// A plugin node's meter cell: the project meter it reads once per block for
/// the time-signature and bar fields of the transport it tells the plugin.
///
/// Every plugin node has one, shared with the node: the out-of-process node's
/// [`PluginControls::meter`](super::PluginControls::meter), and the in-process
/// VST2 node's controls (what inserting it hands back). A clone is another
/// handle on the same cell, so an install through any handle reaches the node
/// the audio thread runs, lock-free and with no graph commit. Empty until a
/// host installs a meter; the node then tells the plugin 4/4 from bar 0.
///
/// # Why a cell of an `Arc`, read twice
///
/// The slot is an [`RtPublish`] of the installed cell's `Arc`, so the node
/// only ever *borrows* that `Arc`: an install or a clear during playback
/// retires the old one for the control side to free, even when the host kept
/// no clone of the meter it installed. (An `ArcSwapOption` here turned the
/// node's guard into the last strong count, and its drop freed the host's
/// whole meter cell on the audio thread.)
///
/// The node's block read is a nested one: this slot, then the meter cell it
/// holds. Reader slots are per cell, so the two reads use one slot each, in
/// different cells. This slot's readers are the nodes holding a handle on it
/// (the node the audio thread runs, once per block, and any clone of an
/// in-process VST2 node that shares the cell), plus the odd control-side
/// [`has_meter`](Self::has_meter). The meter cell is the project's, shared
/// by every plugin node and by other readers (the click node, export forks
/// sharing the cell, control-side reads), so its live readers are bounded by
/// the threads inside a read of that cell at one moment: within the cell's
/// slot count while that is eight or fewer, and safe past it (the overflow
/// path, which pins a retired meter a publish longer).
#[derive(Clone)]
pub struct PluginMeter(Arc<RtPublish<Installed>>);

impl Default for PluginMeter {
    fn default() -> Self {
        Self::holding(None)
    }
}

impl PluginMeter {
    /// Give the node the project meter. Pass the cell the host publishes
    /// meter edits into, and an edit reaches the running plugin with nothing
    /// re-installed. Replaces any meter installed before.
    ///
    /// Control thread: allocates the slot's new value, and frees the meter it
    /// replaces here unless the node is reading it this block (then the next
    /// install or clear frees it).
    pub fn set_meter(&self, meter: Arc<RtPublish<MeterMap>>) {
        self.0.publish(Arc::new(Some(meter)));
    }

    /// Drop the meter; the node tells the plugin 4/4 from bar 0.
    ///
    /// Control thread, as [`set_meter`](Self::set_meter).
    pub fn clear_meter(&self) {
        self.0.publish(Arc::new(None));
    }

    /// Whether a meter is installed.
    pub fn has_meter(&self) -> bool {
        self.0.read().is_some()
    }

    /// The installed meter's cell, for the node's once-per-block read: a
    /// borrow, wait-free, no allocation, and dropping it frees nothing.
    pub(crate) fn read(&self) -> RtRef<'_, Installed> {
        self.0.read()
    }

    /// The meter installed now, to give a fork (whose cell is its own).
    /// Control thread: clones the `Arc`.
    pub(crate) fn installed(&self) -> Installed {
        Option::clone(&self.0.read())
    }

    /// A handle on a new cell holding `meter`: the default's, and an
    /// in-process VST2 fork's.
    pub(crate) fn holding(meter: Installed) -> Self {
        Self(Arc::new(RtPublish::new(meter)))
    }
}

impl std::fmt::Debug for PluginMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginMeter")
            .field("installed", &self.has_meter())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{Beat, MeterChange, TimeSignature};

    /// A meter of `n`/4 from beat 0, for a block to recognise.
    fn meter_of(n: u32) -> Arc<RtPublish<MeterMap>> {
        Arc::new(RtPublish::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::from_parts(n, 4),
        )])))
    }

    /// Installing or clearing the meter while the node reads it frees nothing
    /// on the audio thread, even when the host kept no clone of the meter it
    /// installed (so the slot held the meter cell's last `Arc`).
    ///
    /// Each block does the node's read (`PluginMeter::read`, then the meter
    /// cell's, as `graph_node.rs` and the in-process VST2 feed do) under
    /// `assert_no_alloc`, and the host's install or clear lands while both
    /// borrows are live, under `permit_alloc`: the free of the replaced meter
    /// must then fall to the control side, never to the borrows' drops. The
    /// lib test binary registers the `AllocDisabler` allocator
    /// (`process_pipeline_tests`). This pins the reader's code path; the race
    /// itself is `tutti-types`' loom model's.
    ///
    /// Mutation: putting the slot back on `ArcSwapOption` (`load()` for
    /// `read`, `store` for `set_meter` and `clear_meter`) aborts this on its
    /// first block: the store turns the node's guard into the replaced meter
    /// cell's last strong count, and the guard's drop frees the whole cell
    /// inside the gate.
    #[test]
    fn installing_or_clearing_during_a_block_frees_nothing_on_the_audio_thread() {
        let slot = PluginMeter::default();
        // The host keeps no clone: the slot holds the meter's only `Arc`.
        slot.set_meter(meter_of(1));
        // One read outside the gate, as a stream's first block would be: so
        // the mutation below fails on the free, not on a lazy first-read
        // registration (arc-swap allocated a per-thread node on a thread's
        // first `load`).
        assert!(slot.has_meter());

        for block in 1..=16u32 {
            let seen = assert_no_alloc::assert_no_alloc(|| {
                let installed = slot.read();
                let meter = installed.as_ref().map(|cell| cell.read());
                assert_no_alloc::permit_alloc(|| {
                    if block % 3 == 0 {
                        slot.clear_meter();
                    } else {
                        slot.set_meter(meter_of(block + 1));
                    }
                });
                meter.map(|m| m.changes()[0].signature)
            });
            // The block read what was installed before it began, whatever
            // landed meanwhile: `block`/4 (the first meter, or the previous
            // block's install), or none after the previous block's clear.
            let cleared_before = block > 1 && (block - 1) % 3 == 0;
            let want = (!cleared_before).then(|| TimeSignature::from_parts(block, 4));
            assert_eq!(
                seen, want,
                "block {block} reads the meter installed before it"
            );
        }
        assert!(slot.has_meter(), "block 16 installed 17/4");
    }
}
