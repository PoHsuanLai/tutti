//! Non-destructive MIDI event storage for offline export.
//!
//! Unlike the real-time MIDI registry which uses destructive reads,
//! MidiSnapshot allows events to be polled multiple times without
//! consuming them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use tutti_core::{Beat, BeatDuration};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;

/// One [`MidiEvent`] tagged with the absolute beat at which it fires.
///
/// The single "timed event" currency across the runtime: the snapshot store,
/// the clip player (re-exported there as `TimedClipEvent`), and clip-file import
/// all speak this one type, so events move between them without repacking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedMidiEvent {
    /// The event itself. Its own `frame_offset` is meaningless in storage —
    /// [`beat`](Self::beat) is the authority, and a reader stamps the offset
    /// from it at poll time.
    pub event: MidiEvent,
    /// Beat position when this event should trigger.
    pub beat: Beat,
}

impl TimedMidiEvent {
    /// A timed event at `beat`. Field-order-independent, so callers never have
    /// to remember whether `beat` or `event` comes first.
    #[inline]
    pub const fn new(beat: Beat, event: MidiEvent) -> Self {
        Self { event, beat }
    }
}

impl From<(f64, MidiEvent)> for TimedMidiEvent {
    /// `(beat, event)` — matches the tuples [`crate::tutti_midi_types::ParsedClipFile::timed`]
    /// yields, so a parsed clip file drops straight into the player/snapshot.
    ///
    /// The bare `f64` is the SMF edge: `ParsedClipFile` divides absolute ticks by
    /// ticks-per-quarter and has no beat vocabulary of its own. Converted here,
    /// once, on the way in.
    #[inline]
    fn from((beat, event): (f64, MidiEvent)) -> Self {
        Self {
            event,
            beat: Beat(beat),
        }
    }
}

impl From<(Beat, MidiEvent)> for TimedMidiEvent {
    #[inline]
    fn from((beat, event): (Beat, MidiEvent)) -> Self {
        Self { event, beat }
    }
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
    /// An empty snapshot holding no units and no events.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one event, keeping the unit's stream beat-sorted.
    ///
    /// Appending at or after the last event — the usual case, since callers
    /// build a track forward in time — is O(1) and skips the sort entirely.
    /// Only an out-of-order insert pays to re-sort, so building a whole clip
    /// through this is O(n) sorted / O(n log n) worst case, not the O(n²) a
    /// sort-every-call would cost. [`add_events`](Self::add_events) is still
    /// preferable when the events are already in hand.
    pub fn add_event(&mut self, unit_id: MidiUnitId, beat: Beat, event: MidiEvent) {
        let events = self.events.entry(unit_id).or_default();
        // NaN never compares >=, so a NaN beat takes the re-sort path, where
        // `sort_by_beat`'s total order handles it.
        let in_order = events.last().is_none_or(|last| beat >= last.beat);
        events.push(TimedMidiEvent::new(beat, event));
        if !in_order {
            sort_by_beat(events);
        }
        self.cursors
            .entry(unit_id)
            .or_insert_with(|| AtomicUsize::new(0));
    }

    /// Bulk-insert many events for one unit, sorting **once** at the end — the
    /// O(n log n) path for building a clip, versus [`add_event`](Self::add_event)'s
    /// per-call re-sort. Accepts anything that converts into a [`TimedMidiEvent`],
    /// including `(beat, event)` tuples and the output of
    /// [`ParsedClipFile::timed`](crate::tutti_midi_types::ParsedClipFile::timed).
    pub fn add_events(
        &mut self,
        unit_id: MidiUnitId,
        events: impl IntoIterator<Item = impl Into<TimedMidiEvent>>,
    ) {
        let slot = self.events.entry(unit_id).or_default();
        slot.extend(events.into_iter().map(Into::into));
        sort_by_beat(slot);
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
        start_beat: Beat,
        end_beat: Beat,
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
        start_beat: Beat,
        end_beat: Beat,
        beats_per_sample: BeatDuration,
        out: &mut [MidiEvent],
    ) -> usize {
        self.poll_range_inner(unit_id, start_beat, end_beat, out, Some(beats_per_sample))
    }

    /// `timing` is `Some(beats_per_sample)` to stamp frame offsets, `None` to
    /// leave each event's own offset alone.
    ///
    /// The rate alone is sufficient: an origin would only ever be a copy of
    /// `start_beat`, and a same-typed pair with no field names is exactly the
    /// shape that gets swapped silently. [`BeatDuration`] names it as a *span*
    /// of beats per sample — not an [`Hz`](tutti_core::Hz), which is its
    /// inverse.
    fn poll_range_inner(
        &self,
        unit_id: MidiUnitId,
        start_beat: Beat,
        end_beat: Beat,
        out: &mut [MidiEvent],
        timing: Option<BeatDuration>,
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
            if let Some(beats_per_sample) = timing {
                if beats_per_sample > BeatDuration(0.0) {
                    let beat_delta = (events[pos].beat - start_beat).max(BeatDuration(0.0));
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

    /// Whether `unit_id` has any stored events at all. Ignores cursors, so it
    /// stays true for a fully-replayed unit until [`reset`](Self::reset).
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
        all.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(core::cmp::Ordering::Equal)
        });
        all
    }
}

/// Stable NaN-safe beat sort — a `NaN` beat compares Equal (kept in place)
/// rather than panicking, unlike a bare `partial_cmp(..).unwrap()`.
#[inline]
fn sort_by_beat(events: &mut [TimedMidiEvent]) {
    events.sort_by(|a, b| {
        a.beat
            .partial_cmp(&b.beat)
            .unwrap_or(core::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
        )
    }

    fn note_off(note: u8) -> MidiEvent {
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, note, 0)
    }

    fn buf16() -> [MidiEvent; 16] {
        [MidiEvent::noop(); 16]
    }

    /// `add_event` skips the sort when events arrive in beat order, so the
    /// out-of-order path is the one that can regress: inserting backwards must
    /// still yield a beat-sorted stream.
    #[test]
    fn add_event_sorts_out_of_order_inserts() {
        let mut snapshot = MidiSnapshot::new();
        let unit = MidiUnitId::new(1);

        // Deliberately backwards, plus a duplicate beat.
        snapshot.add_event(unit, Beat(3.0), note_on(67, 100));
        snapshot.add_event(unit, Beat(1.0), note_on(60, 100));
        snapshot.add_event(unit, Beat(2.0), note_on(64, 100));
        snapshot.add_event(unit, Beat(1.0), note_off(60));

        let mut out = buf16();
        let n = snapshot.poll_range(unit, Beat(0.0), Beat(10.0), &mut out);
        assert_eq!(n, 4);
        let notes: Vec<_> = out[..n].iter().filter_map(|e| e.note()).collect();
        // Beat order: 1.0 (note-on 60), 1.0 (note-off 60, stable), 2.0, 3.0.
        assert_eq!(notes, vec![60, 60, 64, 67]);
    }

    #[test]
    fn test_snapshot_basic() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(123);

        snapshot.add_event(unit_id, Beat(0.0), note_on(60, 100));
        snapshot.add_event(unit_id, Beat(1.0), note_off(60));

        let mut out = buf16();
        let n = snapshot.poll_range(unit_id, Beat(0.0), Beat(2.0), &mut out);
        assert_eq!(n, 2);
    }

    #[test]
    fn test_snapshot_poll_range() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(123);

        snapshot.add_event(unit_id, Beat(0.0), note_on(60, 100));
        snapshot.add_event(unit_id, Beat(0.5), note_on(64, 100));
        snapshot.add_event(unit_id, Beat(1.0), note_off(60));
        snapshot.add_event(unit_id, Beat(1.0), note_off(64));

        let mut out = buf16();

        // Poll first half beat
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.0), Beat(0.5), &mut out),
            1
        );

        // Poll next half beat
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.5), Beat(1.0), &mut out),
            1
        );

        // Poll beat 1.0
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(1.0), Beat(1.5), &mut out),
            2
        );
    }

    #[test]
    fn test_snapshot_reset() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(123);

        snapshot.add_event(unit_id, Beat(0.0), note_on(60, 100));
        let mut out = buf16();

        // First poll
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.0), Beat(1.0), &mut out),
            1
        );

        // Second poll without reset — no events (cursor advanced)
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.0), Beat(1.0), &mut out),
            0
        );

        // Reset and poll again
        snapshot.reset();
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.0), Beat(1.0), &mut out),
            1
        );
    }

    #[test]
    fn test_snapshot_multiple_units() {
        let mut snapshot = MidiSnapshot::new();
        let u1 = MidiUnitId::new(1);
        let u2 = MidiUnitId::new(2);

        snapshot.add_event(u1, Beat(0.0), note_on(60, 100));
        snapshot.add_event(u2, Beat(0.0), note_on(72, 100));

        let mut out = buf16();
        assert_eq!(snapshot.poll_range(u1, Beat(0.0), Beat(1.0), &mut out), 1);
        assert_eq!(snapshot.poll_range(u2, Beat(0.0), Beat(1.0), &mut out), 1);
    }

    #[test]
    fn test_has_events() {
        let mut snapshot = MidiSnapshot::new();
        let u1 = MidiUnitId::new(1);
        let u2 = MidiUnitId::new(2);

        assert!(!snapshot.has_events(u1));

        snapshot.add_event(u1, Beat(0.0), note_on(60, 100));

        assert!(snapshot.has_events(u1));
        assert!(!snapshot.has_events(u2));
    }

    #[test]
    fn test_poll_nonexistent_unit() {
        let snapshot = MidiSnapshot::new();
        let mut out = buf16();
        assert_eq!(
            snapshot.poll_range(MidiUnitId::new(999), Beat(0.0), Beat(1.0), &mut out),
            0
        );
    }

    #[test]
    fn test_poll_skips_events_before_start() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(1);

        snapshot.add_event(unit_id, Beat(0.0), note_on(60, 100));
        snapshot.add_event(unit_id, Beat(1.0), note_on(62, 100));
        snapshot.add_event(unit_id, Beat(2.0), note_on(64, 100));
        snapshot.add_event(unit_id, Beat(3.0), note_on(65, 100));

        let mut out = buf16();
        // Poll starting at beat 2 — should skip beats 0 and 1
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(2.0), Beat(4.0), &mut out),
            2
        );
    }

    #[test]
    fn test_poll_range_respects_buffer_len() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(1);

        snapshot.add_event(unit_id, Beat(0.0), note_on(60, 100));
        snapshot.add_event(unit_id, Beat(0.25), note_on(62, 100));
        snapshot.add_event(unit_id, Beat(0.5), note_on(64, 100));

        // Buffer holds 2 — expect two events out, third left for next call
        let mut small = [MidiEvent::noop(); 2];
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.0), Beat(1.0), &mut small),
            2
        );

        let mut out = buf16();
        assert_eq!(
            snapshot.poll_range(unit_id, Beat(0.0), Beat(1.0), &mut out),
            1
        );
    }

    #[test]
    fn add_events_bulk_matches_per_event_and_sorts_once() {
        let unit = MidiUnitId::new(7);
        // Feed out-of-order via the bulk path (tuples → TimedMidiEvent).
        let mut bulk = MidiSnapshot::new();
        bulk.add_events(
            unit,
            [
                (2.0, note_off(60)),
                (0.0, note_on(60, 100)),
                (1.0, note_on(64, 90)),
            ],
        );
        // Same events via the per-event path.
        let mut one_by_one = MidiSnapshot::new();
        one_by_one.add_event(unit, Beat(0.0), note_on(60, 100));
        one_by_one.add_event(unit, Beat(1.0), note_on(64, 90));
        one_by_one.add_event(unit, Beat(2.0), note_off(60));

        // Both end beat-sorted and identical.
        assert_eq!(
            bulk.events_in_beat_order(),
            one_by_one.events_in_beat_order()
        );
        let beats: Vec<f64> = bulk
            .events_in_beat_order()
            .iter()
            .map(|e| e.beat.get())
            .collect();
        assert_eq!(beats, [0.0, 1.0, 2.0]);
    }

    #[test]
    fn timed_event_from_tuple_and_new_agree() {
        let ev = note_on(60, 100);
        assert_eq!(
            TimedMidiEvent::new(Beat(1.5), ev),
            TimedMidiEvent::from((1.5, ev))
        );
    }

    /// Frame offsets are measured from the range start, and the range start is
    /// the only origin there is.
    ///
    /// `poll_range_inner` took an `Option<(f64, f64)>` — an unnamed pair whose
    /// first element every caller filled with a copy of `start_beat`. Two
    /// same-typed unnamed fields, one redundant: transposing them was silent,
    /// and the redundancy meant the two copies could disagree. Only the rate
    /// survives, so there is nothing left to transpose.
    #[test]
    fn frame_offsets_are_measured_from_the_range_start() {
        let mut snap = MidiSnapshot::new();
        let unit = MidiUnitId::new(3);
        snap.add_event(unit, Beat(2.0), note_on(60, 100));
        snap.add_event(unit, Beat(2.5), note_on(62, 100));

        // Half a beat per 100 samples => 0.005 beats/sample.
        let mut out = [note_off(0); 4];
        let n = snap.poll_range_timed(unit, Beat(2.0), Beat(3.0), BeatDuration(0.005), &mut out);

        assert_eq!(n, 2);
        assert_eq!(out[0].frame_offset, 0, "the event at the origin is at 0");
        assert_eq!(
            out[1].frame_offset, 100,
            "half a beat later, at 0.005 beats/sample, is 100 samples in"
        );
    }

    /// A non-positive rate cannot place anything, so offsets are left alone.
    #[test]
    fn a_stalled_rate_leaves_frame_offsets_untouched() {
        let mut snap = MidiSnapshot::new();
        let unit = MidiUnitId::new(4);
        snap.add_event(unit, Beat(1.0), note_on(60, 100));

        let mut out = [note_off(0); 2];
        let n = snap.poll_range_timed(unit, Beat(0.0), Beat(2.0), BeatDuration(0.0), &mut out);

        assert_eq!(n, 1);
        assert_eq!(out[0].frame_offset, 0);
    }

    #[test]
    fn nan_beat_does_not_panic_the_sort() {
        // A NaN beat must not reach a `partial_cmp(..).unwrap()`: the builder
        // is audio-adjacent, so an unorderable beat is kept in place rather
        // than panicking.
        let mut snap = MidiSnapshot::new();
        let unit = MidiUnitId::new(9);
        snap.add_event(unit, Beat(0.0), note_on(60, 100));
        snap.add_event(unit, Beat(f64::NAN), note_on(62, 100)); // must not panic
        snap.add_event(unit, Beat(1.0), note_off(60));
        assert_eq!(snap.events_in_beat_order().len(), 3);
    }
}
