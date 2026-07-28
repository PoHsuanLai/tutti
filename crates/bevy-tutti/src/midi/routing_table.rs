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
/// Deliberately no `Default` and no `Clone`, so that mistake is a compile error
/// rather than a silent one. It only arrives via
/// [`build_into`](crate::engine::build_into)'s handoff, the same way
/// [`MidiBusRes`](super::MidiBusRes) does.
///
/// [`MidiPreBlock`]: tutti_midi_runtime::MidiPreBlock
#[derive(Resource)]
pub struct MidiRoutingRes(pub tutti_midi_types::MidiRoutingTable);
