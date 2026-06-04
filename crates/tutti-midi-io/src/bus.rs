//! The MIDI fan-out bus resource — the shared audio-thread event-dispatch
//! surface every MIDI duty queues into.
//!
//! [`MidiBusRes`] wraps a [`tutti_midi_runtime::MidiBus`] (owned by
//! tutti-midi-runtime; this newtype only adds the Bevy `Resource` derive). It is
//! built once by bevy-tutti's RT-wiring transaction and claimed into the world
//! by [`TuttiMidiPlugin`](crate::TuttiMidiPlugin); the sequence, scheduled, and
//! MPE duties — plus dawai's track/effect graphs — all read it to push events.

use bevy_ecs::prelude::*;

/// MIDI fan-out bus — audio-thread event dispatch to per-unit inboxes.
#[derive(Resource, Clone)]
pub struct MidiBusRes(pub tutti_midi_runtime::MidiBus);

impl std::ops::Deref for MidiBusRes {
    type Target = tutti_midi_runtime::MidiBus;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
