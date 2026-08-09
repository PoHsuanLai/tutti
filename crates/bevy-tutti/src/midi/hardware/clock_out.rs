//! MIDI clock-master output: drain the engine's [`ClockMaster`] ring to
//! hardware MIDI-out.
//!
//! The [`ClockMaster`](tutti_midi_runtime::ClockMaster) runs on the audio
//! thread (installed on the RT processor by bevy-tutti) and pushes outbound
//! MIDI Beat Clock / MTC into a lock-free [`MidiMailbox`] mailbox. This module
//! owns the *off-RT* half: [`ClockMasterRes`] holds the master handle (for
//! enable/config from the UI) plus the mailbox's [`MidiReceiver`], and
//! [`pump_clock_out_system`] drains it each frame to the OS MIDI output via
//! [`MidiIo::send`](tutti_midi_hardware::MidiIo).
//!
//! Modeled on the hardware-input drain: engine produces on the audio thread, a
//! per-frame Bevy system forwards the results. The drain cadence doesn't affect
//! inter-tick spacing — the tick timing is baked into each event on the audio
//! thread; the OS output thread writes on receipt.
//!
//! Where those events *go* is [`hardware_out`](super::hardware_out)'s: this
//! module owns the clock master's handle and its drain, nothing about the wire.

use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::{ClockMaster, MidiReceiver};

use super::hardware_out::{drain_receiver_through, JrStamperRes, MidiOutRouter, UmpOutRes};

/// The clock master + its output-mailbox receiver, claimed from the engine
/// handoff.
///
/// Present only when the engine was built with a clock master (i.e. the `midi`
/// feature). The handle lets the UI toggle enable / MTC / frame-rate; the
/// [`MidiReceiver`] is drained to hardware-out by [`pump_clock_out_system`].
///
/// No `Mutex`: [`MidiReceiver`] is `Sync` and its `poll_into` takes `&self`, so
/// the receiver sits directly in the resource — the single per-frame pump is the
/// only reader.
#[derive(Resource)]
pub struct ClockMasterRes {
    pub master: Arc<ClockMaster>,
    pub receiver: MidiReceiver,
}

impl ClockMasterRes {
    pub fn new(master: Arc<ClockMaster>, receiver: MidiReceiver) -> Self {
        Self { master, receiver }
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

    drain_receiver_through(&clock_out.receiver, &mut router);
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
