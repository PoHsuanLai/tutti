//! [`TuttiMidiPlugin`] — composition root for the MIDI subsystem.
//!
//! It claims the engine-built MIDI handles (the fan-out bus + optional hardware
//! I/O) out of the [`PendingMidi`] transient bevy-tutti inserted, then adds the
//! per-duty sub-plugins:
//!
//! - [`MidiInputPlugin`](crate::input::MidiInputPlugin) — hardware → ECS event bridge
//! - [`MidiRoutingPlugin`](crate::routing::MidiRoutingPlugin) — component-driven route table
//! - [`MidiSequencePlugin`](crate::sequence::MidiSequencePlugin) — transport-beat note firing
//! - [`ScheduledMidiPlugin`](crate::scheduled::ScheduledMidiPlugin) — time-delayed dispatch
//! - [`MidiDevicePlugin`](crate::device::MidiDevicePlugin) — hardware connect/poll (`midi-hardware`)
//! - [`MpePlugin`](crate::mpe::MpePlugin) — per-note expression read side (`mpe`)
//!
//! Claiming happens synchronously in `build()` (before frame 1), so every
//! `*Res` is present by the time any gated system runs — the same invariant
//! every other tutti subsystem plugin relies on.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::bus::MidiBusRes;

#[cfg(feature = "midi-hardware")]
use crate::device::MidiIoRes;

/// Transient handoff: the built MIDI handles. bevy-tutti's `build_into` inserts
/// this; [`TuttiMidiPlugin`]'s `build()` claims it into [`MidiBusRes`] (and, under
/// `midi-hardware`, [`MidiIoRes`](crate::device::MidiIoRes)), then removes it.
#[derive(Resource)]
pub struct PendingMidi {
    pub bus: Option<tutti_midi_runtime::MidiBus>,
    #[cfg(feature = "midi-hardware")]
    pub io: Option<crate::MidiIo>,
}

/// Bevy plugin: MIDI input + routing + sequence playback + time-delayed
/// dispatch, plus hardware device management (`midi-hardware`) and MPE (`mpe`).
pub struct TuttiMidiPlugin;

impl Plugin for TuttiMidiPlugin {
    fn build(&self, app: &mut App) {
        // Claim our handles out of the transient `build_into` inserted
        // (synchronous, during plugin build — present before frame 1).
        if let Some(mut pending) = app.world_mut().remove_resource::<PendingMidi>() {
            if let Some(bus) = pending.bus.take() {
                app.insert_resource(MidiBusRes(bus));
            }
            #[cfg(feature = "midi-hardware")]
            if let Some(io) = pending.io.take() {
                app.insert_resource(MidiIoRes(io));
            }
        }

        app.add_plugins(crate::input::MidiInputPlugin);
        app.add_plugins(crate::routing::MidiRoutingPlugin);
        app.add_plugins(crate::sequence::MidiSequencePlugin);
        app.add_plugins(crate::scheduled::ScheduledMidiPlugin);

        #[cfg(feature = "midi-hardware")]
        app.add_plugins(crate::device::MidiDevicePlugin);

        #[cfg(feature = "mpe")]
        app.add_plugins(crate::mpe::MpePlugin);
    }
}
