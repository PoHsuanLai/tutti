//! [`TuttiMidiPlugin`] — composition root for the MIDI subsystem.

use bevy_app::{App, Plugin};

/// MIDI routing, sequence playback and time-delayed dispatch, plus hardware
/// device management (`midi-hardware`) and MPE (`mpe`).
///
/// Composes the per-duty sub-plugins:
///
/// - [`MidiRoutingPlugin`](super::routing::MidiRoutingPlugin) — component-driven route table
/// - [`MidiSequencePlugin`](super::sequence::MidiSequencePlugin) — transport-beat note firing
/// - [`ScheduledMidiPlugin`](super::scheduled::ScheduledMidiPlugin) — time-delayed dispatch
/// - [`ClockOutPlugin`](super::clock_out::ClockOutPlugin) — outbound Beat Clock / MTC
/// - [`MidiOutPlugin`](super::track_out::MidiOutPlugin) — arbitrary MIDI-out to external hardware
/// - [`MidiNegotiationPlugin`](super::negotiation::MidiNegotiationPlugin) — MIDI-CI + UMP-Stream discovery
/// - [`MidiMetadataPlugin`](super::metadata::MidiMetadataPlugin) — Flex Data metadata broadcast
/// - `MidiDevicePlugin` — hardware connect/poll (`midi-hardware`)
///
/// The MIDI handles these systems read — `MidiBusRes`, `MidiRoutingRes`,
/// `ClockMasterRes`, and under `midi-hardware` `MidiIoRes` — are inserted by
/// [`build_into`](crate::engine::build_into), which builds them alongside the
/// RT pre-block that shares their state.
pub struct TuttiMidiPlugin;

impl Plugin for TuttiMidiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(super::routing::MidiRoutingPlugin);
        app.add_plugins(super::sequence::MidiSequencePlugin);
        app.add_plugins(super::scheduled::ScheduledMidiPlugin);
        app.add_plugins(super::clock_out::ClockOutPlugin);
        app.add_plugins(super::track_out::MidiOutPlugin);
        app.add_plugins(super::negotiation::MidiNegotiationPlugin);
        app.add_plugins(super::metadata::MidiMetadataPlugin);

        #[cfg(feature = "midi-hardware")]
        app.add_plugins(super::device::MidiDevicePlugin);

        // MPE is configured via `MpeModeConfig`; the engine build reads it to
        // construct the input-edge `MpeIngest`. Ensure the resource exists (the
        // app / inspector may override it).
        app.init_resource::<super::mpe::MpeModeConfig>();
    }
}
