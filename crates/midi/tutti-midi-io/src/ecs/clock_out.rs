//! MIDI clock-master output: drain the engine's [`ClockMaster`] ring to
//! hardware MIDI-out.
//!
//! The [`ClockMaster`](tutti_midi_runtime::ClockMaster) runs on the audio
//! thread (installed on the RT processor by bevy-tutti) and pushes outbound
//! MIDI Beat Clock / MTC into a lock-free ring. This module owns the *off-RT*
//! half: [`ClockMasterRes`] holds the master handle (for enable/config from the
//! UI) plus the ring's consumer, and [`pump_clock_out_system`] drains it each
//! frame to the OS MIDI output via [`MidiIo::send`](crate::MidiIo).
//!
//! Modeled on the hardware-input drain (`dawai-frontend`'s `drain_hardware_midi`):
//! engine produces on the audio thread, a per-frame Bevy system forwards the
//! results. The drain cadence doesn't affect inter-tick spacing — the tick
//! timing is baked into each event on the audio thread; the OS output thread
//! writes on receipt.

use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use parking_lot::Mutex;

use tutti_midi_runtime::{ClockMaster, MidiOutputConsumer};
use tutti_midi_types::ump::MidiEvent;

/// The clock master + its output-ring consumer, claimed from the engine handoff.
///
/// Present only when the engine was built with a clock master (i.e. the `midi`
/// feature). The handle lets the UI toggle enable / MTC / frame-rate; the
/// consumer is drained to hardware-out by [`pump_clock_out_system`].
///
/// The consumer is behind a `Mutex` because `ringbuf`'s consumer is `!Sync` and
/// a Bevy `Resource` must be `Sync`; only the single per-frame pump ever locks
/// it, so it's uncontended.
#[derive(Resource)]
pub struct ClockMasterRes {
    pub master: Arc<ClockMaster>,
    pub consumer: Mutex<MidiOutputConsumer>,
}

impl ClockMasterRes {
    pub fn new(master: Arc<ClockMaster>, consumer: MidiOutputConsumer) -> Self {
        Self {
            master,
            consumer: Mutex::new(consumer),
        }
    }
}

/// Max events drained per frame into the stack buffer — one frame of clock at
/// any sane tempo is a handful of events, so this is generous headroom.
const DRAIN_CHUNK: usize = 256;

/// Per-frame: drain the clock-master ring and send each event to hardware MIDI
/// out. Under `midi-hardware` this reaches the OS; otherwise it drains and
/// drops (keeping the ring from backing up).
#[cfg_attr(not(feature = "midi-hardware"), allow(unused_variables))]
pub fn pump_clock_out_system(
    clock_out: Option<Res<ClockMasterRes>>,
    #[cfg(feature = "midi-hardware")] midi_io: Option<Res<super::device::MidiIoRes>>,
) {
    let Some(clock_out) = clock_out else {
        return;
    };
    let mut consumer = clock_out.consumer.lock();
    let mut buf = [MidiEvent::noop(); DRAIN_CHUNK];
    loop {
        let n = consumer.drain_into(&mut buf);
        if n == 0 {
            break;
        }
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &midi_io {
            for ev in &buf[..n] {
                io.0.send(*ev);
            }
        }
        if n < DRAIN_CHUNK {
            break;
        }
    }
}

/// Registers the clock-master output pump. The [`ClockMasterRes`] itself is
/// claimed by [`TuttiMidiPlugin`](crate::TuttiMidiPlugin) from the engine
/// handoff; this plugin only schedules the drain.
pub struct ClockOutPlugin;

impl Plugin for ClockOutPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, pump_clock_out_system);
    }
}
