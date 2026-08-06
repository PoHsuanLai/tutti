//! Native-UMP virtual MIDI endpoints (macOS / CoreMIDI).
//!
//! Both directions carry UMP words end to end, so MIDI-2-only messages — per-note
//! controllers, per-note pitch bend, and **JR Timestamps** — survive to the wire.
//!
//! The MIDI-1.0 pair that used to live here ([`VirtualMidiSource`] /
//! `VirtualMidiDestination`, byte-oriented `PacketBuffer`) is deleted: it had no
//! consumers, and a MIDI-1.0 endpoint's only send path forces every event through
//! `MidiEvent::to_midi1_bytes`, which returns `None` for exactly the messages this
//! engine exists to carry.
//!
//! [`VirtualMidiSource`]: https://docs.rs/coremidi/latest/coremidi/struct.VirtualSource.html

#![cfg(target_os = "macos")]

mod ump_destination;
mod ump_source;
pub use ump_destination::UmpVirtualDestination;
pub use ump_source::UmpVirtualSource;
