//! MIDI track for offline export.
//!
//! [`MidiTrack`] is a thin, fluent wrapper around [`MidiSnapshot`]. Public
//! users build one with [`MidiTrack::new`] + [`note_on`](Self::note_on) /
//! [`note_off`](Self::note_off) / [`cc`](Self::cc) / [`raw`](Self::raw),
//! then hand it to [`crate::GraphExport::with_midi`].
//!
//! The [`from_snapshot`](Self::from_snapshot) and [`snapshot`](Self::snapshot)
//! bridges exist so the umbrella `tutti` engine can populate the snapshot
//! from its live MIDI registry and inject a [`MidiSnapshotReader`] into
//! MIDI-consuming nodes — operations that public users do not perform.
//!
//! [`MidiSnapshot`]: tutti_midi_runtime::MidiSnapshot
//! [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader

use tutti_midi_runtime::tutti_midi_types::ump::MidiEvent;
use tutti_midi_runtime::tutti_midi_types::MidiUnitId;
use tutti_midi_runtime::MidiSnapshot;

#[derive(Debug, Default)]
pub struct MidiTrack {
    snapshot: MidiSnapshot,
}

impl MidiTrack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a note-on at `beat` for `unit`.
    /// `velocity` is a 16-bit MIDI 2.0 value (0..=0xFFFF; use 0x8000 for mf).
    pub fn note_on(&mut self, beat: f64, unit: MidiUnitId, note: u8, velocity: u16) -> &mut Self {
        self.snapshot
            .add_event(unit, beat, MidiEvent::note_on(0, 0, note, velocity));
        self
    }

    /// Schedule a note-off at `beat` for `unit`. Velocity defaults to 0.
    pub fn note_off(&mut self, beat: f64, unit: MidiUnitId, note: u8) -> &mut Self {
        self.snapshot
            .add_event(unit, beat, MidiEvent::note_off(0, 0, note, 0));
        self
    }

    /// Schedule a control-change at `beat` for `unit`.
    pub fn cc(&mut self, beat: f64, unit: MidiUnitId, controller: u8, value: u8) -> &mut Self {
        // Promote 7-bit CC to MIDI 2.0 32-bit value.
        let v32 = (value as u32) << 25;
        self.snapshot
            .add_event(unit, beat, MidiEvent::cc(0, 0, controller, v32));
        self
    }

    /// Schedule an arbitrary MIDI event at `beat` for `unit`.
    pub fn raw(&mut self, beat: f64, unit: MidiUnitId, event: MidiEvent) -> &mut Self {
        self.snapshot.add_event(unit, beat, event);
        self
    }

    /// Wrap an externally-prepared [`MidiSnapshot`]. Used by the umbrella
    /// `tutti::Engine::export()` to seed live registry events into the
    /// snapshot before injecting a `MidiSnapshotReader` into the cloned
    /// net's MIDI-consuming nodes.
    pub fn from_snapshot(snapshot: MidiSnapshot) -> Self {
        Self { snapshot }
    }

    /// Borrow the inner snapshot. Engine-level callers use this to
    /// construct a [`tutti_midi_runtime::MidiSnapshotReader`] over the same
    /// underlying state. Cloning the reference is cheap; the snapshot
    /// itself uses interior `Arc` for the cursor map.
    pub fn snapshot(&self) -> &MidiSnapshot {
        &self.snapshot
    }

    /// Consume and return the inner snapshot.
    pub fn into_snapshot(self) -> MidiSnapshot {
        self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_track_new_is_empty() {
        let track = MidiTrack::new();
        // Snapshot exposes no `len`; round-trip through into_snapshot ensures
        // the wrapper is valid.
        let _snapshot = track.into_snapshot();
    }

    #[test]
    fn note_on_off_chain() {
        let unit = MidiUnitId::new(1);
        let mut track = MidiTrack::new();
        track
            .note_on(0.0, unit, 60, 0x8000)
            .note_off(1.0, unit, 60)
            .cc(0.5, unit, 7, 100);
        let snap = track.into_snapshot();
        assert!(snap.has_events(unit));
    }
}
