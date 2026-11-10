//! System Real-Time / System Common (M2-104 §7.6, UMP Message Type 0x1, one
//! 32-bit word): timing clock, transport start/stop/continue, MIDI Time Code
//! quarter-frame, song position/select, active sensing, reset and tune
//! request.
//!
//! One is built with `tutti_ump::Message::System(group, System::..)` and
//! `From<Message> for Ump`: there is no constructor here. Decode the other
//! direction with [`MidiMessage::from_ump`](crate::MidiMessage::from_ump), which
//! surfaces these as [`MidiMessage`](crate::MidiMessage) System variants.

#[cfg(test)]
mod tests;
