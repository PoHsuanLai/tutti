//! MIDI through the session: clips, routes, held keys, the hardware out and the SoundFont unit.

// A held key across a retarget and a re-route.
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
// The hardware out as an event tap on the clock (doc 017, E3).
#[cfg(feature = "midi")]
mod midi_out_tap;
