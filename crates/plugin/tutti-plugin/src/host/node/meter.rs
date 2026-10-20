//! The project meter a plugin node reads its time signature and bar with.

use std::sync::Arc;

use tutti_core::{MeterSlot, MeterSlotCell, ProjectMeter};

/// A plugin node's meter: the project meter it reads once per block for the
/// time-signature and bar fields of the transport it tells the plugin.
///
/// Every plugin node has one, shared with the node: the out-of-process node's
/// [`PluginControls::meter`](super::PluginControls::meter), and the in-process
/// VST2 node's controls (what inserting it hands back). A clone is another
/// handle on the same slot, so an install through any handle reaches the node
/// the audio thread runs, lock-free and with no graph commit. Empty until a
/// host installs a meter; the node then tells the plugin 4/4 from bar 0.
///
/// # One cell, fixed when the node is built
///
/// The node reads one cell of its own ([`MeterSlot::cell`], taken when it is
/// built), holding the project's [`MeterMap`](tutti_core::MeterMap) or
/// nothing. Installing a meter makes the slot *follow* a [`ProjectMeter`]:
/// the project meter publishes into the slot now, and on every
/// [`ProjectMeter::set_meter`] after, so a meter edit reaches the metronome
/// and every plugin with one call. Installing or clearing is a publish into
/// the node's cell, never a swap of which cell it reads.
///
/// The cell is an [`ArcLend`](tutti_core::ArcLend) of an `Option<Arc<_>>`,
/// so the node only ever *borrows* the meter: an install or a clear during
/// playback retires the old value for the control side to free. (An
/// `ArcSwapOption` here turned the node's guard into the last strong count,
/// and its drop freed the meter on the audio thread.)
#[derive(Clone, Default)]
pub struct PluginMeter(MeterSlot);

impl PluginMeter {
    /// Give the node the project meter: it follows `meter`, so an edit the
    /// host publishes there reaches the running plugin with nothing
    /// re-installed. Replaces any meter installed before.
    ///
    /// Control thread: publishes into the node's cell, and frees the value
    /// it replaces here unless the node is reading it this block (then the
    /// next publish frees it).
    pub fn set_meter(&self, meter: ProjectMeter) {
        self.0.follow(meter);
    }

    /// Drop the meter; the node tells the plugin 4/4 from bar 0.
    ///
    /// Control thread, as [`set_meter`](Self::set_meter).
    pub fn clear_meter(&self) {
        self.0.unfollow();
    }

    /// Whether a meter is installed. Control side: reads the slot's own
    /// record, never the node's cell.
    pub fn has_meter(&self) -> bool {
        self.0.is_following()
    }

    /// The cell the node reads once per block: for the node, when it is
    /// built. A borrow of it is wait-free, allocates nothing, and frees
    /// nothing when it drops.
    pub(crate) fn cell(&self) -> Arc<MeterSlotCell> {
        self.0.cell()
    }

    /// The meter installed now, to give a fork (whose slot is its own).
    /// Control side.
    pub(crate) fn installed(&self) -> Option<ProjectMeter> {
        self.0.following()
    }

    /// A handle on a new slot following `meter`, or nothing: an in-process
    /// VST2 fork's.
    #[cfg(feature = "vst2")]
    pub(crate) fn holding(meter: Option<ProjectMeter>) -> Self {
        Self(MeterSlot::following_meter(meter))
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
    use tutti_core::{Beat, MeterChange, MeterMap, TimeSignature};

    /// A project meter of `n`/4 from beat 0, for a block to recognise.
    fn meter_of(n: u32) -> ProjectMeter {
        ProjectMeter::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::from_parts(n, 4),
        )]))
    }

    /// Installing or clearing the meter while the node reads it frees nothing
    /// on the audio thread, even when the host kept no clone of the meter it
    /// installed (so the slot's value held the meter's last `Arc`).
    ///
    /// Each block does the node's read (one borrow of the node's cell, as
    /// `graph_node.rs` and the in-process VST2 feed do) under
    /// `assert_no_alloc`, and the host's install or clear lands while the
    /// borrow is live, under `permit_alloc`: the free of the replaced meter
    /// must then fall to the control side, never to the borrow's drop. The
    /// lib test binary registers the `AllocDisabler` allocator
    /// (`process_pipeline_tests`). This pins the reader's code path; the race
    /// itself is `arc-lend`'s loom model's.
    ///
    /// Mutation (run): the block taking an owning handle rather than a
    /// borrow (`Option::clone(&cell.lend())`, the shape an `ArcSwapOption`
    /// guard upgraded into) aborts inside the gate (SIGABRT): an install
    /// replaced the only other owner of the meter the block held, and the
    /// handle's drop frees it there.
    #[test]
    fn installing_or_clearing_during_a_block_frees_nothing_on_the_audio_thread() {
        let slot = PluginMeter::default();
        let cell = slot.cell();
        // The host keeps no clone: the slot holds the project meter's only
        // handle.
        slot.set_meter(meter_of(1));
        // One read outside the gate, as a stream's first block would be: so
        // the mutation below fails on the free, not on a lazy first-read
        // registration (arc-swap allocated a per-thread node on a thread's
        // first `load`).
        assert!(cell.lend().is_some());

        for block in 1..=16u32 {
            let seen = assert_no_alloc::assert_no_alloc(|| {
                let installed = cell.lend();
                let meter = installed.as_deref();
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

    /// **A project meter edit reaches the node** with nothing re-installed,
    /// and a clear stops it: the node's cell follows the meter it was given.
    ///
    /// Mutation (run): `set_meter` publishing the project meter's value once
    /// and not following it (`MeterSlot::follow` without `attach`'s
    /// registration) → the edit to 7/8 never reaches the cell, fails.
    #[test]
    fn a_project_meter_edit_reaches_the_installed_node() {
        let project = meter_of(3);
        let slot = PluginMeter::default();
        let cell = slot.cell();
        slot.set_meter(project.clone());
        assert!(slot.installed().unwrap().ptr_eq(&project));
        let seven_eight = TimeSignature::from_parts(7, 8);
        project.set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            seven_eight,
        )])));
        assert_eq!(
            cell.lend().as_deref().map(|m| m.changes()[0].signature),
            Some(seven_eight)
        );
        slot.clear_meter();
        assert!(cell.lend().is_none());
        project.set_meter(Arc::new(MeterMap::default()));
        assert!(cell.lend().is_none(), "a cleared node no longer follows");
    }
}
