//! MIDI clock-master output, and everything else wired to the hardware out:
//! drain the engine's hardware-out node to hardware MIDI-out.
//!
//! The clock is a graph node (`ClockNode`) the engine build wires to a
//! `MidiOutNode`, the hardware out ([`MidiEngineNodes`](crate::midi::MidiEngineNodes)).
//! Anything else wired to that node's entity (`EventSources`: a clip, a
//! plugin's MIDI out) reaches the wire the same way. This module owns the
//! *off-RT* half: [`ClockMasterRes`] holds the master handle (for
//! enable/config from the UI) plus the out node's controls, and
//! [`pump_clock_out_system`] drains it each frame to the OS MIDI output.
//!
//! The drain cadence doesn't affect inter-tick spacing — the tick timing is
//! baked into each event on the audio thread; the OS output thread writes on
//! receipt.
//!
//! Where those events *go* is [`hardware_out`](super::hardware_out)'s: this
//! module owns the clock master's handle and the drain, nothing about the wire.

use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::{ClockMaster, MidiOutControls};

use super::hardware_out::{drain_receiver_through, JrStamperRes, MidiOutRouter, UmpOutRes};

/// The clock master and the hardware-out node's controls, from the engine
/// build.
///
/// Present only when the engine was built with the `midi` feature. The
/// master lets the UI toggle enable / MTC / frame-rate; `out` is drained to
/// hardware-out by [`pump_clock_out_system`].
#[derive(Resource)]
pub struct ClockMasterRes {
    /// The generator itself, shared with the clock node that ticks it. Use it
    /// to toggle enable / MTC / frame rate; it starts disabled, so nothing
    /// reaches the wire until a host turns it on.
    pub master: Arc<ClockMaster>,
    /// The hardware-out node's ring. Read only by [`pump_clock_out_system`] —
    /// a second reader would take events that one would then never see.
    pub out: MidiOutControls,
}

impl ClockMasterRes {
    /// Pair a clock master with the hardware-out node it is wired to.
    pub fn new(master: Arc<ClockMaster>, out: MidiOutControls) -> Self {
        Self { master, out }
    }
}

/// Per-frame: drain the clock-master ring and send each event to hardware MIDI
/// out, via the shared [`MidiOutRouter`]. Under `midi-hardware` this reaches the
/// OS; otherwise it drains and drops (keeping the ring from backing up).
pub fn pump_clock_out_system(
    // `Option`, despite the `engine_ready` gate: that condition reads
    // `AudioEngineState`, which a host can insert on its own, while
    // `ClockMasterRes` only appears if `engine::build_into` actually ran. A test
    // or an example that builds a graph and declares the engine up — which the
    // crate's own tests do — has the former without the latter, and a hard `Res`
    // turns that into a panicked schedule rather than a quiet no-op.
    clock_out: Option<Res<ClockMasterRes>>,
    drops: Res<super::hardware_out::MidiOutDrops>,
    ump_out: Option<ResMut<UmpOutRes>>,
    jr: Option<Res<JrStamperRes>>,
) {
    let Some(clock_out) = clock_out else {
        return;
    };
    let mut router = MidiOutRouter {
        out: super::hardware_out::out_active(ump_out, jr),
        drops: Some(&drops),
    };

    drain_receiver_through(clock_out.out.receiver(), &mut router);
}

/// Registers the clock-master output pump. The [`ClockMasterRes`] itself is
/// claimed by [`TuttiMidiPlugin`](crate::midi::plugin::TuttiMidiPlugin) from the engine
/// handoff; this plugin only schedules the drain.
pub struct ClockOutPlugin;

impl Plugin for ClockOutPlugin {
    fn build(&self, app: &mut App) {
        // `run_if(engine_ready)` like every other MIDI system — but the gate is
        // not sufficient on its own, so the system takes `ClockMasterRes` as an
        // `Option` too. `engine_ready` reads `AudioEngineState`; the resource
        // comes from `build_into`. Those are two different facts, and a host can
        // have the first without the second.
        app.add_systems(
            Update,
            pump_clock_out_system.run_if(crate::graph::engine_ready),
        );
    }
}
