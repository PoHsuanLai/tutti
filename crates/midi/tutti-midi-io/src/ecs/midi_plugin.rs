//! [`TuttiMidiPlugin`] — composition root for the MIDI subsystem.
//!
//! It claims the engine-built MIDI handles (the fan-out bus + optional hardware
//! I/O) out of the [`PendingMidi`] transient bevy-tutti inserted, then adds the
//! per-duty sub-plugins:
//!
//! - [`MidiRoutingPlugin`](super::routing::MidiRoutingPlugin) — component-driven route table
//! - [`MidiSequencePlugin`](super::sequence::MidiSequencePlugin) — transport-beat note firing
//! - [`ScheduledMidiPlugin`](super::scheduled::ScheduledMidiPlugin) — time-delayed dispatch
//! - [`MidiOutPlugin`](super::track_out::MidiOutPlugin) — arbitrary MIDI-out to external hardware
//! - [`MidiNegotiationPlugin`](super::negotiation::MidiNegotiationPlugin) — MIDI-CI + UMP-Stream discovery
//! - [`MidiMetadataPlugin`](super::metadata::MidiMetadataPlugin) — Flex Data metadata broadcast
//! - `MidiDevicePlugin` — hardware connect/poll (`midi-hardware`)
//! - `MpePlugin` — per-note expression read side (`mpe`)
//!
//! Claiming happens synchronously in `build()` (before frame 1), so every
//! `*Res` is present by the time any gated system runs — the same invariant
//! every other tutti subsystem plugin relies on.

use bevy_app::{App, Plugin};

/// Bevy plugin: MIDI input + routing + sequence playback + time-delayed
/// dispatch, plus hardware device management (`midi-hardware`) and MPE (`mpe`).
///
/// The MIDI handles themselves (`MidiBusRes`, `MidiRoutingRes`,
/// `ClockMasterRes`, and under `midi-hardware` `MidiIoRes`) are inserted
/// directly by bevy-tutti's `build_into` — insertion *is* the handoff — so this
/// plugin only composes the per-duty sub-plugins.
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
