//! Beat-scheduled MIDI clip playback as a [`MidiSource`].
//!
//! A `MidiClipSource` holds a sorted `Vec<TimedClipEvent>` (events tagged
//! with absolute beats) and a [`TransportReader`]. On each
//! `poll_into(unit_id, block_start_sample, block_size, …)` it reads the
//! transport beat, computes the beat range covered by the upcoming
//! audio block, and emits events whose beat falls in that range with
//! their `frame_offset` set to the sample-accurate position inside the
//! block.
//!
//! Multiple sources can be merged via [`CompositeMidiSource`] so a
//! single synth can receive both live preview events (from the
//! ordinary `MidiBus` registry) and clip-driven events at the same time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_core::transport::TransportReader;
use tutti_midi_types::source::MidiSource;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::unit_id::MidiUnitId;

/// One MIDI event scheduled at an absolute beat position. Note-on
/// and note-off both arrive as fully-formed [`MidiEvent`]s so the
/// player is agnostic to message shape.
#[derive(Clone, Copy, Debug)]
pub struct TimedClipEvent {
    pub beat: f64,
    pub event: MidiEvent,
}

/// MIDI clip player. Constructed with a sorted-by-beat event list,
/// a transport reader, and the audio sample rate. Events are emitted
/// when their beat falls inside the block range covered by a
/// `poll_into` call.
///
/// Cheap to clone — internal state shares atomic cursors so the
/// graph commit's clone of the parent unit doesn't restart playback.
pub struct MidiClipSource {
    events: Arc<[TimedClipEvent]>,
    transport: Arc<dyn TransportReader>,
    sample_rate: f64,
    /// Index into `events` of the first event we haven't emitted yet.
    /// Atomic so `poll_into` is `&self`.
    cursor: Arc<AtomicU64>,
    /// The transport beat at the most recent poll. We compare against
    /// this to detect a backwards seek (transport rewound) and reset
    /// the cursor accordingly.
    last_beat: Arc<AtomicF64>,
    /// Only events targeting this unit are emitted. Events whose unit
    /// doesn't match are skipped (a single composite source can fan to
    /// many synths via per-unit clip players).
    target_unit: MidiUnitId,
}

impl MidiClipSource {
    /// Build a clip source. `events` must be sorted ascending by beat.
    pub fn new(
        target_unit: MidiUnitId,
        events: impl IntoIterator<Item = TimedClipEvent>,
        transport: Arc<dyn TransportReader>,
        sample_rate: f64,
    ) -> Self {
        let mut v: Vec<TimedClipEvent> = events.into_iter().collect();
        v.sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            events: v.into(),
            transport,
            sample_rate,
            cursor: Arc::new(AtomicU64::new(0)),
            last_beat: Arc::new(AtomicF64::new(f64::NEG_INFINITY)),
            target_unit,
        }
    }

    pub fn target_unit(&self) -> MidiUnitId {
        self.target_unit
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Reset the cursor to the start. Used after a transport seek.
    fn rewind_to(&self, beat: f64) {
        let mut idx = 0usize;
        while idx < self.events.len() && self.events[idx].beat < beat {
            idx += 1;
        }
        self.cursor.store(idx as u64, Ordering::Release);
    }
}

impl Clone for MidiClipSource {
    fn clone(&self) -> Self {
        Self {
            events: Arc::clone(&self.events),
            transport: Arc::clone(&self.transport),
            sample_rate: self.sample_rate,
            cursor: Arc::clone(&self.cursor),
            last_beat: Arc::clone(&self.last_beat),
            target_unit: self.target_unit,
        }
    }
}

impl MidiSource for MidiClipSource {
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        _block_start_sample: u64,
        block_size: usize,
        out: &mut [MidiEvent],
    ) -> usize {
        if unit_id != self.target_unit {
            return 0;
        }
        if block_size == 0 || out.is_empty() || self.events.is_empty() {
            return 0;
        }
        if !self.transport.is_playing() {
            // Track the beat anyway so a seek-while-paused doesn't
            // surprise us when playback resumes.
            self.last_beat
                .store(self.transport.current_beat(), Ordering::Release);
            return 0;
        }

        let block_start_beat = self.transport.current_beat();
        let last_beat = self.last_beat.load(Ordering::Acquire);
        // Detect rewinds / seeks. Tolerate a tiny epsilon so float
        // jitter at exactly-equal beats doesn't trigger reseeking.
        if block_start_beat + 1e-9 < last_beat {
            self.rewind_to(block_start_beat);
        }
        self.last_beat
            .store(block_start_beat, Ordering::Release);

        let tempo_bpm = self.transport.tempo().get();
        if tempo_bpm <= 0.0 || self.sample_rate <= 0.0 {
            return 0;
        }
        let beats_per_sample = tempo_bpm / 60.0 / self.sample_rate;
        let block_end_beat = block_start_beat + (block_size as f64) * beats_per_sample;
        let max_offset = (block_size - 1) as u32;

        // Walk the event list from the cursor position, emitting any
        // event whose beat falls in `[block_start_beat, block_end_beat)`.
        let mut cursor = self.cursor.load(Ordering::Relaxed) as usize;
        // Skip past anything before the window (cursor may have lagged
        // due to a seek, looping, or a buffer that filled up earlier).
        while cursor < self.events.len() && self.events[cursor].beat < block_start_beat {
            cursor += 1;
        }

        let mut written = 0;
        while cursor < self.events.len()
            && self.events[cursor].beat < block_end_beat
            && written < out.len()
        {
            let TimedClipEvent { beat, mut event } = self.events[cursor];
            let beat_delta = (beat - block_start_beat).max(0.0);
            let sample_offset = (beat_delta / beats_per_sample) as u32;
            event.frame_offset = sample_offset.min(max_offset);
            out[written] = event;
            written += 1;
            cursor += 1;
        }

        // Only persist the cursor advance for events we actually wrote
        // out — the rest will come back on the next poll.
        self.cursor.store(cursor as u64, Ordering::Release);
        written
    }
}

/// Fan multiple [`MidiSource`]s into one. Used to merge live preview
/// events (from `MidiBus` registry) with clip playback for the same
/// synth. Events are concatenated in source order; the receiving
/// synth re-sorts by `frame_offset` before processing.
pub struct CompositeMidiSource {
    sources: Vec<Box<dyn MidiSource>>,
}

impl CompositeMidiSource {
    pub fn new(sources: Vec<Box<dyn MidiSource>>) -> Self {
        Self { sources }
    }

    pub fn push(&mut self, source: Box<dyn MidiSource>) {
        self.sources.push(source);
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

impl MidiSource for CompositeMidiSource {
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
        block_start_sample: u64,
        block_size: usize,
        out: &mut [MidiEvent],
    ) -> usize {
        let mut written = 0;
        for src in &self.sources {
            if written >= out.len() {
                break;
            }
            let n =
                src.poll_into(unit_id, block_start_sample, block_size, &mut out[written..]);
            written += n;
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tutti_core::params::Bpm;

    /// Minimal `TransportReader` for tests: tempo + beat under a switch.
    struct TestTransport {
        beat: AtomicF64,
        tempo: f64,
        playing: AtomicBool,
    }

    impl TestTransport {
        fn new(tempo: f64) -> Self {
            Self {
                beat: AtomicF64::new(0.0),
                tempo,
                playing: AtomicBool::new(true),
            }
        }
        fn set_beat(&self, b: f64) {
            self.beat.store(b, Ordering::Release);
        }
    }

    impl TransportReader for TestTransport {
        fn current_beat(&self) -> f64 {
            self.beat.load(Ordering::Acquire)
        }
        fn is_loop_enabled(&self) -> bool {
            false
        }
        fn get_loop_range(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_playing(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn is_in_preroll(&self) -> bool {
            false
        }
        fn tempo(&self) -> Bpm {
            Bpm(self.tempo)
        }
    }

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(0, 0, note, (vel as u16) << 9)
    }

    #[test]
    fn emits_event_at_correct_frame_offset() {
        let unit = MidiUnitId::new(1);
        let transport = Arc::new(TestTransport::new(120.0));
        // 120 BPM @ 44.1kHz → 22050 samples/beat → ~22.05 samples per 0.001 beat.
        let sample_rate = 44100.0;

        let events = vec![
            TimedClipEvent {
                beat: 0.0,
                event: note_on(60, 100),
            },
            TimedClipEvent {
                beat: 0.5,
                event: note_on(64, 100),
            },
        ];
        let source = MidiClipSource::new(
            unit,
            events,
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            sample_rate,
        );

        // First block: cover [0.0, 1.0) beats = [0, 22050) samples.
        let mut buf = [MidiEvent::noop(); 8];
        let n = source.poll_into(unit, 0, 22050, &mut buf);
        assert_eq!(n, 2);
        assert_eq!(buf[0].frame_offset, 0);
        // Second event at beat 0.5 → 11025 samples.
        assert!(
            (buf[1].frame_offset as i64 - 11025).abs() < 4,
            "got {}",
            buf[1].frame_offset
        );

        // Polling again at the same beat: cursor advanced, no new events.
        let n2 = source.poll_into(unit, 22050, 22050, &mut buf);
        assert_eq!(n2, 0);
    }

    #[test]
    fn ignores_other_unit_ids() {
        let unit = MidiUnitId::new(1);
        let other = MidiUnitId::new(2);
        let transport = Arc::new(TestTransport::new(120.0));
        let source = MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: 0.0,
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_into(other, 0, 1024, &mut buf), 0);
    }

    #[test]
    fn paused_transport_emits_nothing() {
        let unit = MidiUnitId::new(1);
        let transport = Arc::new(TestTransport::new(120.0));
        transport.playing.store(false, Ordering::Release);
        let source = MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: 0.0,
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_into(unit, 0, 1024, &mut buf), 0);
    }

    #[test]
    fn seek_backwards_replays_events() {
        let unit = MidiUnitId::new(1);
        let transport = Arc::new(TestTransport::new(120.0));
        let source = MidiClipSource::new(
            unit,
            vec![
                TimedClipEvent {
                    beat: 0.0,
                    event: note_on(60, 100),
                },
                TimedClipEvent {
                    beat: 0.25,
                    event: note_on(64, 100),
                },
            ],
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        );

        let mut buf = [MidiEvent::noop(); 4];

        // First block @ beat 0
        assert_eq!(source.poll_into(unit, 0, 22050, &mut buf), 2);
        // Move forward — cursor exhausted, nothing emitted.
        transport.set_beat(2.0);
        assert_eq!(source.poll_into(unit, 22050, 22050, &mut buf), 0);
        // Seek back to start — events should fire again.
        transport.set_beat(0.0);
        assert_eq!(source.poll_into(unit, 0, 22050, &mut buf), 2);
    }

    #[test]
    fn composite_concatenates_outputs() {
        let unit = MidiUnitId::new(7);
        let transport = Arc::new(TestTransport::new(120.0));

        let s1 = Box::new(MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: 0.0,
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        )) as Box<dyn MidiSource>;
        let s2 = Box::new(MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: 0.25,
                event: note_on(64, 100),
            }],
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            44100.0,
        )) as Box<dyn MidiSource>;

        let comp = CompositeMidiSource::new(vec![s1, s2]);
        let mut buf = [MidiEvent::noop(); 8];
        let n = comp.poll_into(unit, 0, 22050, &mut buf);
        assert_eq!(n, 2);
    }
}
