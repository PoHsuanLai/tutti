//! The protocol-transparent hardware seam.
//!
//! A [`MidiPort`] is a live connection to a MIDI device that speaks
//! [`MidiEvent`] — the engine's MIDI-2-native event — in both directions. Whether
//! the wire underneath is a MIDI 1.0 byte stream or a MIDI 2.0 UMP stream is the
//! *implementation's* private concern; nothing above the port needs to know. This
//! is the boundary the whole engine is built around: everything internal is
//! MIDI-2, and each port translates at its own edge (M2-104 §4.1, the Translator).
//!
//! Today the only implementation is [`Midi1Port`], which wraps a `midir`
//! connection — every OS MIDI API `midir` targets (CoreMIDI, ALSA seq, WinMM)
//! presents a MIDI 1.0 byte stream, so a `midir` port *is* a MIDI 1.0 endpoint and
//! translates via [`MidiEvent::to_midi1_bytes`] / [`MidiEvent::from_midi1_bytes`].
//!
//! A future `Midi2Port` — a UMP-native backend over CoreMIDI's `MIDIEventList`
//! (macOS 11+), ALSA UMP (kernel 6.5+), or Windows MIDI Services — would implement
//! this same trait, pass `data_words()` straight through, and run the
//! [`EndpointNegotiator`](tutti_midi_types) handshake. Nothing above the port would
//! change: callers already speak `MidiEvent`, and the protocol stays a private
//! property of the concrete port.

use tutti_midi_types::{MidiEvent, Protocol};

/// A live MIDI connection that speaks [`MidiEvent`] regardless of the wire
/// protocol. Implementors translate at their own edge; callers never touch bytes
/// or UMP words.
pub trait MidiPort {
    /// Send one event to the device. Translation to the port's wire form happens
    /// here; a message with no representation on that wire is surfaced as
    /// [`SendError::NoWireForm`] rather than silently dropped.
    fn send(&mut self, event: &MidiEvent) -> Result<(), SendError>;

    /// The wire protocol this port speaks. Callers rarely need it — the point of
    /// the trait is that they don't — but it's available for diagnostics and for
    /// UI that wants to show whether a device negotiated MIDI 2.0.
    fn protocol(&self) -> Protocol;
}

/// Why a [`MidiPort::send`] could not deliver an event.
#[derive(Debug)]
pub enum SendError {
    /// The event has no representation on this port's wire protocol, so it was
    /// not sent. On a [`Midi1Port`] this is the MIDI-2-only message set —
    /// per-note pitch bend, per-note controllers, RPN/NRPN, utility, UMP Stream —
    /// none of which has a single MIDI 1.0 status. Carries the event so a caller
    /// can log, count, or route it elsewhere instead of losing it silently.
    NoWireForm(MidiEvent),

    /// The underlying driver rejected the write (device unplugged mid-send, OS
    /// buffer error, …).
    Wire(crate::error::Error),
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SendError::NoWireForm(_) => {
                f.write_str("event has no representation on this port's wire protocol")
            }
            SendError::Wire(e) => write!(f, "MIDI wire send failed: {e}"),
        }
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SendError::NoWireForm(_) => None,
            SendError::Wire(e) => Some(e),
        }
    }
}
