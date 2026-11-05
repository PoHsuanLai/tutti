//! notes: part of the VST3 event vocabulary.

use super::*;

/// Note-on event. Velocity is normalized to `0.0..=1.0`.
#[derive(Debug, Clone, Copy)]
pub struct NoteOnEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Zero-based MIDI channel within the event bus.
    pub channel: i16,
    /// MIDI note number, 0..=127.
    pub pitch: i16,
    /// Fractional tuning offset from 12-TET, in semitones.
    pub tuning: f32,
    /// Normalized velocity (0.0 – 1.0).
    pub velocity: f32,
    /// Note length in samples; 0 if unknown.
    pub length: i32,
    /// Plugin-assigned note id, or `-1` if channel/pitch-based.
    pub note_id: i32,
}

/// Note-off event. Velocity is normalized to `0.0..=1.0`.
#[derive(Debug, Clone, Copy)]
pub struct NoteOffEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Zero-based MIDI channel within the event bus.
    pub channel: i16,
    /// MIDI note number, 0..=127; must match the originating note-on.
    pub pitch: i16,
    /// Normalized release velocity (0.0 – 1.0).
    pub velocity: f32,
    /// Plugin-assigned note id matching the originating note-on, or `-1`.
    pub note_id: i32,
    /// Fractional tuning offset from 12-TET, in semitones.
    pub tuning: f32,
}

/// Polyphonic pressure (per-note aftertouch).
#[derive(Debug, Clone, Copy)]
pub struct PolyPressureEvent {
    /// Bus, frame offset and event-type discriminant.
    pub header: EventHeader,
    /// Zero-based MIDI channel within the event bus.
    pub channel: i16,
    /// MIDI note number the pressure applies to, 0..=127.
    pub pitch: i16,
    /// Normalized pressure (0.0 – 1.0).
    pub pressure: f32,
    /// Plugin-assigned note id, or `-1` if channel/pitch-based.
    pub note_id: i32,
}
