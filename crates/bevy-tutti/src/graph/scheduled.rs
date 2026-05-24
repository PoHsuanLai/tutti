//! Time-delayed MIDI dispatch.
//!
//! Some host actions need to fire a MIDI event "in N seconds, on this
//! synth." dawai's `tick_pending_note_offs` is one such — note-off
//! scheduled some duration after the note-on so a play-button preview
//! doesn't hang. Rather than reinvent the timer in every host, this
//! module exposes:
//!
//! - [`MidiSynthMarker`] — attaches the synth's [`MidiUnitId`] to a Bevy
//!   entity. Insert it once at synth-spawn time (reading
//!   `PolySynth::midi_unit_id()` / `SoundFontUnit::midi_unit_id()`); the
//!   marker means "this entity *is* the MIDI sink for that unit-id."
//! - [`ScheduledMidi`] — "fire this MIDI event in `remaining_secs` at
//!   the synth on `target`." [`tick_scheduled_midi`] counts the timer
//!   down and dispatches via [`MidiBusRes`](crate::MidiBusRes).
//!
//! The host owns scheduling (`commands.spawn(ScheduledMidi { ... })`);
//! the system owns delivery. Once fired, the entity is despawned.

use std::time::Instant;

use bevy_ecs::prelude::*;

use tutti::core::MidiUnitId;
use tutti::midi::MidiEvent;

use crate::resources::MidiBusRes;

/// "This entity owns the audio-graph node whose MIDI sink id is `midi_unit_id`."
///
/// Insert at synth-spawn time so other systems (and the host) can route
/// MIDI to the synth without re-deriving its unit id from a graph
/// downcast each frame.
///
/// The id is opaque — read it back via [`MidiSynthMarker::midi_unit_id`]
/// when you need to call `MidiBus::queue`.
///
/// Not `Reflect`: `MidiUnitId` is a foreign opaque id type.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MidiSynthMarker {
    pub midi_unit_id: MidiUnitId,
}

impl MidiSynthMarker {
    pub fn new(midi_unit_id: MidiUnitId) -> Self {
        Self { midi_unit_id }
    }

    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi_unit_id
    }
}

/// "Fire `event` at the synth on `target` in `remaining_secs`."
///
/// `target` should be an entity carrying [`MidiSynthMarker`]; if it
/// doesn't, [`tick_scheduled_midi`] logs and despawns. `remaining_secs`
/// is decremented by the per-frame transport delta time and the event
/// fires when it hits zero.
///
/// One scheduled event per entity. Despawn happens automatically after
/// dispatch, so the host can spawn an unbounded series without manual
/// cleanup.
///
/// Not `Reflect`: `MidiEvent` is a foreign type from `tutti-midi`.
#[derive(Component, Debug, Clone)]
pub struct ScheduledMidi {
    pub target: Entity,
    pub event: MidiEvent,
    pub remaining_secs: f32,
}

/// Counts down [`ScheduledMidi`] timers and dispatches the event when
/// they reach zero. Fires through [`MidiBusRes`].
///
/// Wall-clock dt is measured per-system via a [`Local<Instant>`] so we
/// don't force the host to install `bevy_time`. Hosts that prefer
/// transport-beat scheduling (loop-aware, tempo-aware) should use
/// `tutti-core`'s sequencer instead.
pub fn tick_scheduled_midi(
    mut commands: Commands,
    midi: Option<Res<MidiBusRes>>,
    mut last_tick: Local<Option<Instant>>,
    mut scheduled: Query<(Entity, &mut ScheduledMidi)>,
    targets: Query<&MidiSynthMarker>,
) {
    let Some(midi) = midi else { return };

    let now = Instant::now();
    let dt = match *last_tick {
        Some(prev) => now.duration_since(prev).as_secs_f32(),
        None => 0.0,
    };
    *last_tick = Some(now);

    for (entity, mut sched) in scheduled.iter_mut() {
        sched.remaining_secs -= dt;
        if sched.remaining_secs > 0.0 {
            continue;
        }

        let Ok(marker) = targets.get(sched.target) else {
            bevy_log::warn!(
                "tick_scheduled_midi: target {:?} has no MidiSynthMarker; dropping event",
                sched.target
            );
            commands.entity(entity).despawn();
            continue;
        };

        midi.0.queue(marker.midi_unit_id, &[sched.event]);
        commands.entity(entity).despawn();
    }
}
