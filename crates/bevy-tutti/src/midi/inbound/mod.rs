//! Where MIDI entering from outside goes.
//!
//! [`routing_table`] maps an inbound channel to the unit mailbox it feeds;
//! [`route`] is the ECS declaration that rebuilds it. Only the *inbound* edge
//! consults these — hardware in via the RT pre-block, and a plugin's MIDI-out
//! re-entering as if it were a device. Anything already bound to a unit (clip
//! playback, a preview, musical typing) writes to that unit's port directly
//! and never asks a route.

pub mod route;
pub mod routing_table;
