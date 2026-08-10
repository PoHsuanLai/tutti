//! The hardware-input routing table.
//!
//! Maps an inbound MIDI channel to the unit mailbox it feeds. Only the *inbound*
//! edge consults it — `MidiPreBlock` for hardware in, and a plugin's MIDI-out
//! re-entering as if it were a device. Anything already bound to a unit (clip
//! playback, a preview, musical typing) writes to that unit's port directly and
//! never asks a route.
//!
//! It is not part of the audio graph: no fundsp edge is involved, and a route
//! names a *channel*, not a node.

use bevy_ecs::prelude::*;

/// The MIDI routing table's writer half: channel → destination unit mailbox.
///
/// **Must be the instance whose `snapshot_arc()` the RT [`MidiPreBlock`] was
/// built with.** The pre-block reads that shared `ArcSwap` every block; this
/// resource is the only writer. Build a second table and its `commit()`
/// publishes into an `ArcSwap` nothing reads — every hardware MIDI event is
/// dropped, silently, with nothing in the log.
///
/// No `Default`, no `Clone`, and a crate-private field, so that mistake is a
/// compile error rather than a silent one. It only arrives via
/// [`build_into`](crate::engine::build_into)'s handoff, the same way
/// [`MidiBusRes`](crate::midi::MidiBusRes) does.
///
/// [`MidiPreBlock`]: tutti_midi_runtime::MidiPreBlock
#[derive(Resource)]
pub struct MidiRoutingRes(pub(crate) tutti_midi_types::MidiRoutingTable);

impl MidiRoutingRes {
    /// Wrap the table the RT pre-block shares. Crate-internal: a host that
    /// built its own would be building the failure above.
    pub(crate) fn new(table: tutti_midi_types::MidiRoutingTable) -> Self {
        Self(table)
    }

    /// Replace the routing rules and publish them to the audio thread.
    ///
    /// One call rather than the engine's `set_routes` + `commit` pair, because
    /// staging without publishing is not a state any caller here wants to be
    /// in: an uncommitted edit reaches nothing, and the dirty flag is invisible
    /// from outside. `set_routes` takes the rules wholesale — the engine has no
    /// incremental edit — so this is a whole-table replace, and the caller
    /// collects every rule before calling.
    pub fn publish(
        &mut self,
        routes: impl IntoIterator<Item = tutti_midi_types::MidiRoute>,
        fallback: Option<tutti_midi_types::MidiUnitId>,
    ) {
        self.0.set_routes(routes, fallback);
        self.0.commit();
    }

    /// How many rules are staged. For diagnostics and tests.
    pub fn route_count(&self) -> usize {
        self.0.route_count()
    }
}
