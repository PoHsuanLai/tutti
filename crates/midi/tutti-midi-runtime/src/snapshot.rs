//! Non-destructive MIDI event storage for offline export.
//!
//! Unlike the real-time MIDI registry which uses destructive reads,
//! MidiSnapshot allows events to be polled multiple times without
//! consuming them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;

#[derive(Debug, Clone, Copy)]
pub struct TimedMidiEvent {
    pub event: MidiEvent,
    /// Beat position when this event should trigger.
    pub beat: f64,
}

/// Non-destructive snapshot of MIDI events for export.
///
/// Events are stored per unit ID and sorted by beat position.
/// Polling advances a cursor but doesn't remove events, allowing
/// the same snapshot to be used for multiple renders.
///
/// Note: `poll_range` uses atomic cursors so it can take `&self` (RT-safe)
/// and writes into a caller-owned slice (allocation-free).
#[derive(Debug, Default)]
pub struct MidiSnapshot {
    /// Events per unit ID, sorted by beat.
    events: HashMap<MidiUnitId, Vec<TimedMidiEvent>>,
    /// Current read cursor per unit (index into events vec).
    /// Atomic so `poll_range` can advance without `&mut self`.
    cursors: HashMap<MidiUnitId, AtomicUsize>,
}

impl Clone for MidiSnapshot {
    fn clone(&self) -> Self {
        let cursors = self
            .cursors
            .iter()
            .map(|(&k, v)| (k, AtomicUsize::new(v.load(Ordering::Relaxed))))
            .collect();
        Self {
            events: self.events.clone(),
            cursors,
        }
    }
}

impl MidiSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_event(&mut self, unit_id: MidiUnitId, beat: f64, event: MidiEvent) {
        let events = self.events.entry(unit_id).or_default();
        events.push(TimedMidiEvent { event, beat });
        events.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap());
        self.cursors
            .entry(unit_id)
            .or_insert_with(|| AtomicUsize::new(0));
    }

    /// Poll events in the given beat range [start, end) into a caller-owned buffer.
    ///
    /// Returns the number of events written. If `out.len()` is smaller than the
    /// number of available events in the range, the cursor only advances past
    /// the events that were written — the rest are delivered on the next call.
    ///
    /// Uses atomic cursors so this can be called with `&self` (RT-safe) and
    /// performs no allocation.
    pub fn poll_range(
        &self,
        unit_id: MidiUnitId,
        start_beat: f64,
        end_beat: f64,
        out: &mut [MidiEvent],
    ) -> usize {
        self.poll_range_inner(unit_id, start_beat, end_beat, out, None)
    }

    /// Poll events in `[start_beat, end_beat)` and stamp each event's
    /// `frame_offset` based on `beats_per_sample`. The offset is the
    /// number of samples between `start_beat` and the event's beat,
    /// mapping the beat-domain event to the audio block that
    /// `[start_beat, end_beat)` covers.
    pub fn poll_range_timed(
        &self,
        unit_id: MidiUnitId,
        start_beat: f64,
        end_beat: f64,
        beats_per_sample: f64,
        out: &mut [MidiEvent],
    ) -> usize {
        self.poll_range_inner(
            unit_id,
            start_beat,
            end_beat,
            out,
            Some((start_beat, beats_per_sample)),
        )
    }

    fn poll_range_inner(
        &self,
        unit_id: MidiUnitId,
        start_beat: f64,
        end_beat: f64,
        out: &mut [MidiEvent],
        timing: Option<(f64, f64)>,
    ) -> usize {
        let Some(events) = self.events.get(&unit_id) else {
            return 0;
        };

        let Some(cursor) = self.cursors.get(&unit_id) else {
            return 0;
        };

        let mut pos = cursor.load(Ordering::Relaxed);

        while pos < events.len() && events[pos].beat < start_beat {
            pos += 1;
        }

        let mut written = 0;
        while pos < events.len() && events[pos].beat < end_beat && written < out.len() {
            let mut ev = events[pos].event;
            if let Some((origin_beat, beats_per_sample)) = timing {
                if beats_per_sample > 0.0 {
                    let beat_delta = (events[pos].beat - origin_beat).max(0.0);
                    let sample_delta = beat_delta / beats_per_sample;
                    ev.frame_offset = sample_delta as u32;
                }
            }
            out[written] = ev;
            written += 1;
            pos += 1;
        }

        cursor.store(pos, Ordering::Relaxed);
        written
    }

    pub fn has_events(&self, unit_id: MidiUnitId) -> bool {
        self.events.get(&unit_id).is_some_and(|e| !e.is_empty())
    }

    /// Reset all cursors to the beginning.
    ///
    /// Call this before re-rendering to replay all events.
    pub fn reset(&self) {
        for cursor in self.cursors.values() {
            cursor.store(0, Ordering::Relaxed);
        }
    }

    /// Every event across all units, merged into one beat-ordered `Vec` (cursors
    /// untouched). For offline serialization (e.g. a MIDI Clip File) that needs
    /// the whole track as a single ordered stream rather than a cursor poll.
    pub fn events_in_beat_order(&self) -> Vec<TimedMidiEvent> {
        let mut all: Vec<TimedMidiEvent> = self.events.values().flatten().copied().collect();
        all.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap_or(core::cmp::Ordering::Equal));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            0,
            0,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
        )
    }

    fn note_off(note: u8) -> MidiEvent {
        MidiEvent::note_off(0, 0, note, 0)
    }

    fn buf16() -> [MidiEvent; 16] {
        [MidiEvent::noop(); 16]
    }

    #[test]
    fn test_snapshot_basic() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(123);

        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 1.0, note_off(60));

        let mut out = buf16();
        let n = snapshot.poll_range(unit_id, 0.0, 2.0, &mut out);
        assert_eq!(n, 2);
    }

    #[test]
    fn test_snapshot_poll_range() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(123);

        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 0.5, note_on(64, 100));
        snapshot.add_event(unit_id, 1.0, note_off(60));
        snapshot.add_event(unit_id, 1.0, note_off(64));

        let mut out = buf16();

        // Poll first half beat
        assert_eq!(snapshot.poll_range(unit_id, 0.0, 0.5, &mut out), 1);

        // Poll next half beat
        assert_eq!(snapshot.poll_range(unit_id, 0.5, 1.0, &mut out), 1);

        // Poll beat 1.0
        assert_eq!(snapshot.poll_range(unit_id, 1.0, 1.5, &mut out), 2);
    }

    #[test]
    fn test_snapshot_reset() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(123);

        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        let mut out = buf16();

        // First poll
        assert_eq!(snapshot.poll_range(unit_id, 0.0, 1.0, &mut out), 1);

        // Second poll without reset — no events (cursor advanced)
        assert_eq!(snapshot.poll_range(unit_id, 0.0, 1.0, &mut out), 0);

        // Reset and poll again
        snapshot.reset();
        assert_eq!(snapshot.poll_range(unit_id, 0.0, 1.0, &mut out), 1);
    }

    #[test]
    fn test_snapshot_multiple_units() {
        let mut snapshot = MidiSnapshot::new();
        let u1 = MidiUnitId::new(1);
        let u2 = MidiUnitId::new(2);

        snapshot.add_event(u1, 0.0, note_on(60, 100));
        snapshot.add_event(u2, 0.0, note_on(72, 100));

        let mut out = buf16();
        assert_eq!(snapshot.poll_range(u1, 0.0, 1.0, &mut out), 1);
        assert_eq!(snapshot.poll_range(u2, 0.0, 1.0, &mut out), 1);
    }

    #[test]
    fn test_has_events() {
        let mut snapshot = MidiSnapshot::new();
        let u1 = MidiUnitId::new(1);
        let u2 = MidiUnitId::new(2);

        assert!(!snapshot.has_events(u1));

        snapshot.add_event(u1, 0.0, note_on(60, 100));

        assert!(snapshot.has_events(u1));
        assert!(!snapshot.has_events(u2));
    }

    #[test]
    fn test_poll_nonexistent_unit() {
        let snapshot = MidiSnapshot::new();
        let mut out = buf16();
        assert_eq!(
            snapshot.poll_range(MidiUnitId::new(999), 0.0, 1.0, &mut out),
            0
        );
    }

    #[test]
    fn test_poll_skips_events_before_start() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(1);

        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 1.0, note_on(62, 100));
        snapshot.add_event(unit_id, 2.0, note_on(64, 100));
        snapshot.add_event(unit_id, 3.0, note_on(65, 100));

        let mut out = buf16();
        // Poll starting at beat 2 — should skip beats 0 and 1
        assert_eq!(snapshot.poll_range(unit_id, 2.0, 4.0, &mut out), 2);
    }

    #[test]
    fn test_poll_range_respects_buffer_len() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(1);

        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 0.25, note_on(62, 100));
        snapshot.add_event(unit_id, 0.5, note_on(64, 100));

        // Buffer holds 2 — expect two events out, third left for next call
        let mut small = [MidiEvent::noop(); 2];
        assert_eq!(snapshot.poll_range(unit_id, 0.0, 1.0, &mut small), 2);

        let mut out = buf16();
        assert_eq!(snapshot.poll_range(unit_id, 0.0, 1.0, &mut out), 1);
    }
}
