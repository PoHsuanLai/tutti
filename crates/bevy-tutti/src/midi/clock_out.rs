//! MIDI clock-master output: drain the engine's [`ClockMaster`] ring to
//! hardware MIDI-out.
//!
//! The [`ClockMaster`](tutti_midi_runtime::ClockMaster) runs on the audio
//! thread (installed on the RT processor by bevy-tutti) and pushes outbound
//! MIDI Beat Clock / MTC into a lock-free [`MidiMailbox`] mailbox. This module
//! owns the *off-RT* half: [`ClockMasterRes`] holds the master handle (for
//! enable/config from the UI) plus the mailbox's [`MidiReceiver`], and
//! [`pump_clock_out_system`] drains it each frame to the OS MIDI output via
//! [`MidiIo::send`](tutti_midi_io::MidiIo).
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

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
use super::hardware_out::{JrStamperRes, UmpOutRes};
use super::hardware_out::{drain_receiver_through, MidiOutRouter};

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
    clock_out: Res<ClockMasterRes>,
    drops: Res<super::hardware_out::MidiOutDrops>,
    #[cfg(feature = "midi-hardware")] midi_io: Option<Res<super::device::MidiIoRes>>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))] ump_out: Option<ResMut<UmpOutRes>>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))] jr: Option<Res<JrStamperRes>>,
) {
    let mut router = MidiOutRouter {
        #[cfg(feature = "midi-hardware")]
        midi_io: midi_io.as_deref(),
        #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
        jr_out: super::hardware_out::jr_out_active(ump_out, jr),
        drops: Some(&drops),
        #[cfg(not(feature = "midi-hardware"))]
        _marker: std::marker::PhantomData,
    };

    drain_receiver_through(&clock_out.receiver, &mut router);
}

/// Registers the clock-master output pump. The [`ClockMasterRes`] itself is
/// claimed by [`TuttiMidiPlugin`](super::plugin::TuttiMidiPlugin) from the engine
/// handoff; this plugin only schedules the drain.
pub struct ClockOutPlugin;

impl Plugin for ClockOutPlugin {
    fn build(&self, app: &mut App) {
        // `run_if(engine_ready)` like every other MIDI system. `ClockMasterRes`
        // arrives with the engine block, so it needs no `Option`; `MidiIoRes`
        // keeps one, since a hardware port may be absent with the engine up.
        app.add_systems(Update, pump_clock_out_system.run_if(crate::graph::engine_ready));
    }
}
