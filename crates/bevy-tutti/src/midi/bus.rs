//! The MIDI fan-out bus, and the MPE mode the engine build reads.
//!
//! [`MidiBusRes`] wraps a [`tutti_midi_runtime::MidiBus`] — the
//! `MidiUnitId → MidiSender` router every MIDI duty queues into. Senders reach
//! it through [`registration`](super::registration), never by hand.

use bevy_ecs::prelude::*;

/// MIDI fan-out bus — audio-thread event dispatch to per-unit inboxes.
///
/// **Must be the instance the RT [`MidiPreBlock`] was built with.** The engine
/// build hands the pre-block a clone (`MidiBus` shares its map behind an `Arc`)
/// and inserts this resource from the same value, so both see one map. Construct
/// a second one and every registered sender lands somewhere the audio thread
/// never reads: MIDI silently stops, with no error and nothing in the log.
///
/// Hence no `Default` and a private field — `init_resource::<MidiBusRes>()` used
/// to compile and do exactly that. The only way in is [`new`](Self::new), which
/// [`build_into`](crate::engine::build_into) calls with the pre-block's own bus.
/// `MidiRoutingRes` guards the same hazard the same way.
///
/// [`MidiPreBlock`]: tutti_midi_runtime::MidiPreBlock
#[derive(Resource, Clone, Debug)]
pub struct MidiBusRes(pub(crate) tutti_midi_runtime::MidiBus);

impl MidiBusRes {
    /// Wrap the bus the RT pre-block shares. Crate-internal: a host that built
    /// its own would be building the failure above.
    pub(crate) fn new(bus: tutti_midi_runtime::MidiBus) -> Self {
        Self(bus)
    }
}

impl std::ops::Deref for MidiBusRes {
    type Target = tutti_midi_runtime::MidiBus;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Configures the MPE mode the engine installs. Insert this *before* the engine
/// builds to override the default.
///
/// Default is [`MpeMode::Disabled`](tutti_midi_io::MpeMode) — apps that want MPE
/// flip it to `LowerZone` / `UpperZone` / `DualZone` / `SingleChannelRotation`.
///
/// Read once, at build time, to construct the input-edge
/// [`MpeIngest`](tutti_midi_runtime::MpeIngest) that rewrites classic-MPE
/// channel spread into native MIDI-2 per-note messages (per M2-104, zone
/// handling is an *ingestion* concern, not a synthesis one). Changing it later
/// does nothing: the transform is baked into the pre-block.
///
/// It lives here rather than in a module of its own because it is read exactly
/// once, by the same build step that mints the bus above.
#[derive(Resource, Debug, Clone)]
pub struct MpeModeConfig(pub tutti_midi_io::MpeMode);

impl Default for MpeModeConfig {
    fn default() -> Self {
        Self(tutti_midi_io::MpeMode::Disabled)
    }
}
