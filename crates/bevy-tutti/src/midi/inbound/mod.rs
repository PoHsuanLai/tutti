//! Where MIDI entering from the hardware input goes.
//!
//! [`route`] is the ECS declaration: rules that wire the hardware input node's
//! per-channel ports to the entities that should hear them. Anything already
//! bound to a node (clip playback, a keyboard, a plugin's MIDI out) is wired
//! to it directly and never asks a route.

pub mod route;
