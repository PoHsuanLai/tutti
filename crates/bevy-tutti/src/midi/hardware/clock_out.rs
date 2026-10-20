//! MIDI clock-master output, and everything else wired to the hardware out:
//! drain the engine's hardware-out node to hardware MIDI-out.
//!
//! The clock is a graph node (`ClockNode`) the engine build wires to a
//! `MidiOutNode`, the hardware out ([`MidiEngineNodes`](crate::midi::MidiEngineNodes)).
//! Anything else wired to that node's entity (`EventSources`: a clip, a
//! plugin's MIDI out) reaches the wire the same way. This module owns the
//! *off-RT* half: [`ClockMasterRes`] holds the master handle (for
//! enable/config from the UI), and [`pump_clock_out_system`] runs the
//! session's `Phase::MidiOut` each frame, which drains the out node to the
//! output the host connected.
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

use tutti_midi_runtime::ClockMaster;
use tutti_session::Phase;

use super::hardware_out::JrStamperRes;
use crate::graph::AudioGraphRes;

/// The clock master, from the engine build.
///
/// Present only when the engine was built with the `midi` feature. The
/// master lets the UI toggle enable / MTC / frame-rate. The hardware-out
/// node's ring it ticks into is the session's (`Session::midi_nodes`),
/// drained by its `Phase::MidiOut`, the one reader.
#[derive(Resource)]
pub struct ClockMasterRes {
    /// The generator itself, shared with the clock node that ticks it. Use it
    /// to toggle enable / MTC / frame rate; it starts disabled, so nothing
    /// reaches the wire until a host turns it on.
    pub master: Arc<ClockMaster>,
}

impl ClockMasterRes {
    /// Holds the clock master of the engine's clock node.
    pub fn new(master: Arc<ClockMaster>) -> Self {
        Self { master }
    }
}

/// Runs the session's `Phase::MidiOut` each frame: drains the hardware out
/// (the clock, and whatever else is wired to its node) through the
/// session's router to the output the host connected, stamping as
/// [`JrStamperRes`] says. With none connected, the events are dropped
/// (keeping the ring from backing up) and counted.
pub fn pump_clock_out_system(
    // `Option`, despite the `engine_ready` gate: that condition reads
    // `AudioEngineState`, which a host can insert on its own. A session with
    // no MIDI nodes (a headless graph) drains nothing.
    graph: Option<ResMut<AudioGraphRes>>,
    jr: Option<Res<JrStamperRes>>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    super::hardware_out::mirror_stamping(&mut graph, jr.as_deref());
    // `MidiOutDropped` is for a host polling the session; the session has
    // warned of the drops already, and the count is `Midi::out_drops`.
    let _ = graph.run_phase(Phase::MidiOut, std::time::Instant::now());
}

/// Registers the clock-master output pump. The [`ClockMasterRes`] itself is
/// inserted by [`build_into`](crate::engine::build_into); this plugin only
/// schedules the drain. Part of
/// [`TuttiMidiPlugin`](crate::midi::plugin::TuttiMidiPlugin).
pub struct ClockOutPlugin;

impl Plugin for ClockOutPlugin {
    fn build(&self, app: &mut App) {
        // `run_if(engine_ready)` like every other MIDI system; the graph is
        // still an `Option` (see the system).
        app.add_systems(
            Update,
            pump_clock_out_system.run_if(crate::graph::engine_ready),
        );
    }
}
