//! MIDI through the session: clips, routes, held keys, the hardware out and the SoundFont unit.

// A held key across a retarget and a re-route.
#[cfg(feature = "midi")]
mod common;
#[cfg(feature = "midi")]
mod held_keys;
// Clips, live inputs, routes and the hardware out (`Session::midi`).
#[cfg(feature = "midi")]
mod midi;
// Route rules: the union of two on one target, and a disabled one.
#[cfg(feature = "midi")]
mod midi_routes;
// A clip's velocity keeps its 16 bits to the event input.
#[cfg(feature = "midi")]
mod midi_velocity;
// MIDI into a SoundFont unit, rendered: a clip on its beat, a live preview
// beside it, a routed hardware note.
#[cfg(feature = "soundfont")]
mod midi_soundfont;
// The General MIDI setup of a SoundFont unit (`Session::add_general_midi`).
#[cfg(feature = "soundfont")]
mod general_midi;
// The hardware out as an event tap on the clock.
#[cfg(feature = "midi")]
mod midi_out_tap;
// The session's MIDI front: hardware through the view, the session's MPE mode,
// a recorded take and its placement, a saved route rule.
#[cfg(feature = "midi")]
mod midi_front;
// File loads as jobs: a MIDI file and a SoundFont land as events in `update()`.
#[cfg(feature = "midi")]
mod loads;
// The session follows a live Flex Set Tempo and Set Time Signature.
#[cfg(feature = "midi-hardware")]
mod midi_follow;
// A map published with its step behind the playhead moves no beat.
#[cfg(feature = "midi")]
mod follow_late_publish;
