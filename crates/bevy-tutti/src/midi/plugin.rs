//! [`TuttiMidiPlugin`] — composition root for the MIDI subsystem.

use bevy_app::{App, Plugin};

/// MIDI addressing and registration, outbound clock and track MIDI-out, plus
/// hardware device management (`midi-hardware`).
///
/// Composes the per-duty sub-plugins:
///
/// - [`MidiRegistrationPlugin`](super::endpoint::registration::MidiRegistrationPlugin) —
///   keeps the bus in step with the graph
/// - [`MidiRoutePlugin`](super::inbound::route::MidiRoutePlugin) — where inbound MIDI goes
/// - [`MidiSequencePlugin`](super::sequence::MidiSequencePlugin) — beat-scheduled playback
/// - [`ClockOutPlugin`](super::hardware::clock_out::ClockOutPlugin) — outbound Beat Clock / MTC
/// - [`MidiOutPlugin`](super::hardware::track_out::MidiOutPlugin) — MIDI-out to external hardware
/// - [`MidiNegotiationPlugin`](super::hardware::negotiation::MidiNegotiationPlugin) — MIDI-CI + UMP-Stream
/// - [`MidiMetadataPlugin`](super::hardware::metadata::MidiMetadataPlugin) — Flex Data metadata
/// - `MidiDevicePlugin` — hardware connect/poll (`midi-hardware`)
///
/// The handles these read — [`MidiBusRes`](super::MidiBusRes),
/// [`MidiRoutingRes`](super::MidiRoutingRes),
/// [`ClockMasterRes`](super::ClockMasterRes), and under `midi-hardware`
/// `MidiIoRes` — are inserted by [`build_into`](crate::engine::build_into),
/// which constructs them alongside the RT pre-block that shares their state.
/// This plugin inserts none of them; it only registers the types a host writes
/// and schedules the systems that read them.
///
/// [`MpeModeConfig`](super::MpeModeConfig) is *not* initialised here: the engine
/// build reads it, and `TuttiPlugin` builds the engine before adding this
/// plugin, so a resource created here would always be too late. A host that
/// wants MPE inserts it before adding `TuttiPlugin`.
pub struct TuttiMidiPlugin;

impl Plugin for TuttiMidiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<super::endpoint::target::MidiTargetRegistry>();

        app.add_plugins(super::endpoint::registration::MidiRegistrationPlugin);
        app.add_plugins(super::inbound::route::MidiRoutePlugin);
        app.add_plugins(super::sequence::MidiSequencePlugin);
        app.add_plugins(super::hardware::clock_out::ClockOutPlugin);
        app.add_plugins(super::hardware::track_out::MidiOutPlugin);
        app.add_plugins(super::hardware::negotiation::MidiNegotiationPlugin);
        app.add_plugins(super::hardware::metadata::MidiMetadataPlugin);
        app.add_plugins(super::file::MidiFilePlugin);

        #[cfg(feature = "midi-hardware")]
        app.add_plugins(super::hardware::device::MidiDevicePlugin);
    }
}
