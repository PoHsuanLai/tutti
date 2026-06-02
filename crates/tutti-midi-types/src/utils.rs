//! Small MIDI utility functions used across tutti crates.

use libm::{log2f, powf};

/// Convert a MIDI note number (possibly fractional) to frequency in Hz
/// (A4 = 440 Hz, equal temperament).
#[inline]
pub fn note_to_hz(note: f32) -> f32 {
    440.0 * powf(2.0, (note - 69.0) / 12.0)
}

/// Convert frequency in Hz to a (possibly fractional) MIDI note number.
#[inline]
pub fn hz_to_note(hz: f32) -> f32 {
    69.0 + 12.0 * log2f(hz / 440.0)
}

/// Normalize a 7-bit MIDI value (0-127) to 0.0..=1.0.
#[inline]
pub fn normalize_u7(value: u8) -> f32 {
    value as f32 / 127.0
}
