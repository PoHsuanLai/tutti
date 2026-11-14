//! Network MIDI 2.0 (M2-124): the wire format, the session machines over a
//! simulated network, and the driver over real sockets.

mod auth;
mod discovery;
mod sessions;
mod sim;
mod udp;
mod wire;
