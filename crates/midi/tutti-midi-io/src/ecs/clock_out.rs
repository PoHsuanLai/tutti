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

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
use super::metadata::{JrStamperRes, UmpOutRes};

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

/// The shared outbound routing target: where drained MIDI-out events go.
///
/// Captures the per-frame output resources once, so both the clock-master ring
/// (here) and the track MIDI-out ring ([`super::track_out`]) route through one
/// place — the JR-stamp/UMP-vs-MIDI-1 decision lives in exactly one spot.
///
/// **JR-out routing (macOS):** when a [`UmpOutRes`] native-UMP source is present
/// and [`JrStamperRes`] is enabled, each event is JR-stamped (a JR Timestamp
/// prefix derived from its `frame_offset` at the reference clock) and sent as UMP
/// words — the one transport where JR Timestamps reach the wire. Otherwise events
/// go to the MIDI-1 [`MidiIoRes`](super::device::MidiIoRes) port (which drops the
/// JR words, so we don't stamp for it).
pub(super) struct MidiOutRouter<'a> {
    #[cfg(feature = "midi-hardware")]
    pub midi_io: Option<&'a super::device::MidiIoRes>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
    pub jr_out: Option<(&'a mut UmpOutRes, &'a JrStamperRes)>,
    /// Keeps the lifetime and the non-hardware build honest (no fields to borrow).
    #[cfg(not(feature = "midi-hardware"))]
    pub _marker: std::marker::PhantomData<&'a ()>,
}

impl MidiOutRouter<'_> {
    /// Route a batch of drained events to the active output transport.
    pub(super) fn route(&mut self, events: &[MidiEvent]) {
        #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
        if let Some((ump, jr)) = self.jr_out.as_mut() {
            ump.send_stamped(events, &jr.stamper);
            return;
        }
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &self.midi_io {
            for ev in events {
                io.0.send(*ev);
            }
        }
        // No hardware feature (or no connected port): drop, keeping the ring
        // from backing up.
        let _ = events;
    }
}

/// Drain a MIDI-output ring consumer fully and route every event.
pub(super) fn drain_ring_through(
    consumer: &mut MidiOutputConsumer,
    router: &mut MidiOutRouter<'_>,
) {
    let mut buf = [MidiEvent::noop(); DRAIN_CHUNK];
    loop {
        let n = consumer.drain_into(&mut buf);
        if n == 0 {
            break;
        }
        router.route(&buf[..n]);
        if n < DRAIN_CHUNK {
            break;
        }
    }
}

/// Per-frame: drain the clock-master ring and send each event to hardware MIDI
/// out, via the shared [`MidiOutRouter`]. Under `midi-hardware` this reaches the
/// OS; otherwise it drains and drops (keeping the ring from backing up).
pub fn pump_clock_out_system(
    clock_out: Option<Res<ClockMasterRes>>,
    #[cfg(feature = "midi-hardware")] midi_io: Option<Res<super::device::MidiIoRes>>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))] ump_out: Option<ResMut<UmpOutRes>>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))] jr: Option<Res<JrStamperRes>>,
) {
    let Some(clock_out) = clock_out else {
        return;
    };

    let mut router = MidiOutRouter {
        #[cfg(feature = "midi-hardware")]
        midi_io: midi_io.as_deref(),
        #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
        jr_out: jr_out_active(ump_out, jr),
        #[cfg(not(feature = "midi-hardware"))]
        _marker: std::marker::PhantomData,
    };

    let mut consumer = clock_out.consumer.lock();
    drain_ring_through(&mut consumer, &mut router);
}

/// JR-out is active only with an enabled stamper *and* a native-UMP source.
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub(super) fn jr_out_active<'a>(
    ump_out: Option<ResMut<'a, UmpOutRes>>,
    jr: Option<Res<'a, JrStamperRes>>,
) -> Option<(&'a mut UmpOutRes, &'a JrStamperRes)> {
    match (ump_out, jr) {
        (Some(ump), Some(jr)) if jr.enabled => Some((ump.into_inner(), jr.into_inner())),
        _ => None,
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
