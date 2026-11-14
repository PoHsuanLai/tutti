//! The logic of a MIDI 1.0 byte port, with no OS call in it.
//!
//! A byte-oriented MIDI API (Windows Multimedia is the one today) has no UMP
//! and no Endpoint discovery: a port is a list position, a name and a stream of
//! MIDI 1.0 bytes. Everything a backend over one decides lives here, so it is
//! compiled and tested on every platform, and the backend's own file holds only
//! the calls into the OS:
//!
//! - [`ports`]: a port's identity and the id that names it, stale-proof.
//! - [`bytes`]: bytes to MIDI 1.0 UMP (type 2, type 1, SysEx7) on the way in,
//!   and UMP to bytes on the way out, SysEx reassembled whole.
//! - [`anchor`]: an arrival stamp in milliseconds since the port started, as an
//!   `Instant`.
//! - [`poll`]: a device watch for an OS that cannot say a device came or went,
//!   by listing again on a timer.
//!
//! A port speaks MIDI 1.0 and is listed as such, so the session never offers it
//! a MIDI 2.0 message it cannot carry, and what arrives is type 2 packets that
//! the ingest node's `Translator` widens, as it does for a legacy ALSA port.

pub(super) mod anchor;
pub(super) mod bytes;
pub(super) mod poll;
pub(super) mod ports;

#[cfg(test)]
mod tests;
