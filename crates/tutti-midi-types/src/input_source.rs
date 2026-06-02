//! Abstraction for MIDI input sources.
//!
//! This trait allows the audio callback to read MIDI events from various sources
//! (hardware ports, virtual ports, WASM Web MIDI, etc.) without depending on
//! specific implementations or platform timestamps.
//!
//! Implementations are responsible for converting platform-specific timestamps
//! (e.g. `Instant`, `performance.now()`) into `frame_offset` internally.

use crate::ump::MidiEvent;

/// RT-safe MIDI input source that can be polled from the audio callback.
///
/// Implementations must be lock-free and safe to call from the audio thread.
/// `cycle_read` is called once per audio buffer to collect all pending MIDI events.
///
/// Frame offsets must already be computed by the implementation — the processor
/// does not know about platform timestamps.
pub trait MidiInputSource: Send + Sync {
    /// Returns `(port_index, event)` tuples with `event.frame_offset` already set.
    /// Valid until the next call.
    fn cycle_read(&self, nframes: usize) -> &[(usize, MidiEvent)];
}

/// No-op source for when MIDI hardware is not connected.
#[derive(Debug, Default)]
pub struct NoMidiInput;

impl MidiInputSource for NoMidiInput {
    fn cycle_read(&self, _nframes: usize) -> &[(usize, MidiEvent)] {
        &[]
    }
}
