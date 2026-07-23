//! Offline-mode MIDI source backed by [`MidiSnapshot`] + [`OfflineTimeline`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_core::transport::OfflineTimeline;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiSource, MidiUnitId};

use crate::snapshot::MidiSnapshot;

/// Export-mode MIDI source that reads from a snapshot based on transport beat.
///
/// Wraps a [`MidiSnapshot`] and an [`OfflineTimeline`] to provide the same
/// `poll_into()` interface as `MidiBus`, but for offline rendering.
///
/// Each call to `poll_into` reads the current beat from the timeline,
/// polls events in `[last_beat, current_beat)`, and advances the internal
/// cursor. The caller (the offline export pipeline) is responsible for
/// advancing the timeline between calls.
pub struct MidiSnapshotReader {
    snapshot: MidiSnapshot,
    timeline: Arc<OfflineTimeline>,
    last_poll_beat: AtomicF64,
}

impl MidiSnapshotReader {
    pub fn new(snapshot: MidiSnapshot, timeline: Arc<OfflineTimeline>) -> Self {
        let start_beat = timeline.beat().get();
        Self {
            snapshot,
            timeline,
            last_poll_beat: AtomicF64::new(start_beat),
        }
    }
}

impl MidiSource for MidiSnapshotReader {
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        _block_start_sample: u64,
        block_size: usize,
        buffer: &mut [MidiEvent],
    ) -> usize {
        let current_beat = self.timeline.beat().get();
        let last_beat = self.last_poll_beat.load(Ordering::Acquire);

        if current_beat <= last_beat {
            return 0;
        }

        let beats_per_sample = self.timeline.beats_per_sample();
        let count = self.snapshot.poll_range_timed(
            unit_id,
            last_beat,
            current_beat,
            beats_per_sample,
            buffer,
        );

        // Clamp `frame_offset` so the synth's process loop never tries
        // to split past the end of its block. Events whose computed
        // sample offset exceeds the block size are pinned to the last
        // sample of the block — vanishingly rare in practice (would
        // require the timeline to advance more than one block in a
        // single tick).
        if block_size > 0 {
            let max_offset = (block_size - 1) as u32;
            for ev in buffer[..count].iter_mut() {
                if ev.frame_offset > max_offset {
                    ev.frame_offset = max_offset;
                }
            }
        }

        // Only advance the watermark once the caller's buffer could hold
        // everything in this range. If `buffer` filled up, the snapshot's
        // cursor moved forward only for the events we wrote — the rest will
        // come out on the next call at the same `current_beat`.
        if count < buffer.len() {
            self.last_poll_beat.store(current_beat, Ordering::Release);
        }
        count
    }
}

impl Clone for MidiSnapshotReader {
    fn clone(&self) -> Self {
        Self {
            snapshot: self.snapshot.clone(),
            timeline: Arc::clone(&self.timeline),
            last_poll_beat: AtomicF64::new(self.last_poll_beat.load(Ordering::Acquire)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::transport::OfflineTimelineConfig;

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            0,
            0,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
        )
    }

    #[test]
    fn test_snapshot_reader_polls_on_advance() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(42);
        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 1.0, note_on(64, 100));
        snapshot.add_event(unit_id, 2.0, note_on(67, 100));

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: 0.0,
            tempo: tutti_core::Bpm(120.0),
            sample_rate: tutti_core::SampleRate(44100.0),
            loop_range: None,
        }));

        let reader = MidiSnapshotReader::new(snapshot, Arc::clone(&timeline));

        let mut buffer = [MidiEvent::noop(); 16];

        let samples_per_beat = 44100.0 / 2.0; // 120 BPM
        let block = samples_per_beat as usize; // pretend each call covers one beat-sized block

        // At beat 0, nothing yet (no advance)
        let count = reader.poll_into(unit_id, 0, block, &mut buffer);
        assert_eq!(count, 0);

        // Advance to beat 0.5 — should get event at beat 0
        timeline.advance((0.5 * samples_per_beat) as usize);
        let count = reader.poll_into(unit_id, 0, block, &mut buffer);
        assert_eq!(count, 1);

        // Advance to beat 1.5 — should get event at beat 1
        timeline.advance((1.0 * samples_per_beat) as usize);
        let count = reader.poll_into(unit_id, block as u64, block, &mut buffer);
        assert_eq!(count, 1);

        // Advance to beat 3.0 — should get event at beat 2
        timeline.advance((1.5 * samples_per_beat) as usize);
        let count = reader.poll_into(unit_id, (block * 2) as u64, block, &mut buffer);
        assert_eq!(count, 1);

        // No more events
        timeline.advance((1.0 * samples_per_beat) as usize);
        let count = reader.poll_into(unit_id, (block * 3) as u64, block, &mut buffer);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_snapshot_reader_buffer_overflow_resumes() {
        // Three events all within one beat-block. A buffer of 2 forces an
        // overflow: the third event must come out on a follow-up poll, with
        // no duplicates and nothing dropped.
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(5);
        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 0.1, note_on(62, 100));
        snapshot.add_event(unit_id, 0.2, note_on(64, 100));

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: 0.0,
            tempo: tutti_core::Bpm(120.0),
            sample_rate: tutti_core::SampleRate(44100.0),
            loop_range: None,
        }));
        let reader = MidiSnapshotReader::new(snapshot, Arc::clone(&timeline));

        // Advance one full beat so all three events fall in the range.
        timeline.advance(22050);

        // First poll: buffer holds only 2 → overflow.
        let mut small = [MidiEvent::noop(); 2];
        assert_eq!(reader.poll_into(unit_id, 0, 22050, &mut small), 2);

        // Second poll at the SAME beat: the remaining event must surface.
        let mut rest = [MidiEvent::noop(); 8];
        assert_eq!(
            reader.poll_into(unit_id, 0, 22050, &mut rest),
            1,
            "the overflowed event must be delivered, not dropped"
        );

        // No further events at this beat.
        assert_eq!(reader.poll_into(unit_id, 0, 22050, &mut rest), 0);
    }

    #[test]
    fn test_snapshot_reader_no_events_for_unknown_unit() {
        let snapshot = MidiSnapshot::new();
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig::default()));
        let reader = MidiSnapshotReader::new(snapshot, Arc::clone(&timeline));

        let mut buffer = [MidiEvent::noop(); 16];
        timeline.advance(1000);
        let count = reader.poll_into(MidiUnitId::new(999), 0, 1024, &mut buffer);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_snapshot_reader_stamps_frame_offset() {
        let mut snapshot = MidiSnapshot::new();
        let unit_id = MidiUnitId::new(7);
        // Three events spaced quarter-beats apart starting at 0.
        snapshot.add_event(unit_id, 0.0, note_on(60, 100));
        snapshot.add_event(unit_id, 0.25, note_on(62, 100));
        snapshot.add_event(unit_id, 0.5, note_on(64, 100));

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: 0.0,
            tempo: tutti_core::Bpm(120.0),
            sample_rate: tutti_core::SampleRate(44100.0),
            loop_range: None,
        }));
        let reader = MidiSnapshotReader::new(snapshot, Arc::clone(&timeline));

        // Advance the timeline by exactly one beat (22050 samples at
        // 120 BPM / 44.1 kHz). Poll one block covering the same range.
        let samples_per_beat = 22050usize;
        timeline.advance(samples_per_beat);

        let mut buffer = [MidiEvent::noop(); 8];
        let count = reader.poll_into(unit_id, 0, samples_per_beat, &mut buffer);
        assert_eq!(count, 3);

        // First event sits at the head of the block.
        assert_eq!(buffer[0].frame_offset, 0);
        // Second event is a quarter beat in: 22050 / 4 = 5512 samples.
        assert!(
            (buffer[1].frame_offset as i64 - 5512).abs() < 4,
            "expected ≈5512, got {}",
            buffer[1].frame_offset
        );
        // Third event is half a beat in: 22050 / 2 = 11025 samples.
        assert!(
            (buffer[2].frame_offset as i64 - 11025).abs() < 4,
            "expected ≈11025, got {}",
            buffer[2].frame_offset
        );
    }
}
