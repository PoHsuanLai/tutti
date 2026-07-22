//! [`TuttiMidiPlugin`] — composition root for the MIDI subsystem.
//!
//! It claims the engine-built MIDI handles (the fan-out bus + optional hardware
//! I/O) out of the [`PendingMidi`] transient bevy-tutti inserted, then adds the
//! per-duty sub-plugins:
//!
//! - [`MidiRoutingPlugin`](super::routing::MidiRoutingPlugin) — component-driven route table
//! - [`MidiSequencePlugin`](super::sequence::MidiSequencePlugin) — transport-beat note firing
//! - [`ScheduledMidiPlugin`](super::scheduled::ScheduledMidiPlugin) — time-delayed dispatch
//! - `MidiDevicePlugin` — hardware connect/poll (`midi-hardware`)
//! - `MpePlugin` — per-note expression read side (`mpe`)
//!
//! Claiming happens synchronously in `build()` (before frame 1), so every
//! `*Res` is present by the time any gated system runs — the same invariant
//! every other tutti subsystem plugin relies on.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use super::bus::MidiBusRes;

#[cfg(feature = "midi-hardware")]
use super::device::MidiIoRes;

/// Transient handoff: the built MIDI handles. bevy-tutti's `build_into` inserts
/// this; [`TuttiMidiPlugin`]'s `build()` claims it into [`MidiBusRes`] (and, under
/// `midi-hardware`, `MidiIoRes`), then removes it.
#[derive(Resource)]
pub struct PendingMidi {
    pub bus: Option<tutti_midi_runtime::MidiBus>,
    #[cfg(feature = "midi-hardware")]
    pub io: Option<crate::MidiIo>,
    /// The audio-thread clock master + its output-ring consumer, if the engine
    /// built one. Claimed into [`ClockMasterRes`](super::clock_out::ClockMasterRes).
    pub clock_out: Option<super::clock_out::ClockMasterRes>,
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
            if let Some(clock_out) = pending.clock_out.take() {
                app.insert_resource(clock_out);
            }
        }

        app.add_plugins(super::routing::MidiRoutingPlugin);
        app.add_plugins(super::sequence::MidiSequencePlugin);
        app.add_plugins(super::scheduled::ScheduledMidiPlugin);
        app.add_plugins(super::clock_out::ClockOutPlugin);

        #[cfg(feature = "midi-hardware")]
        app.add_plugins(super::device::MidiDevicePlugin);

        app.add_plugins(super::mpe::MpePlugin);
    }
}
