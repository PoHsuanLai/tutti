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
#[derive(Clone)]
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

    /// Reconcile internal state against the live transport and compute the
    /// beat window the upcoming audio block covers.
    ///
    /// This is the live-transport-facing half of [`Self::poll_into`]: it owns
    /// the `is_playing` check, backward-seek detection (with cursor rewind),
    /// the `last_beat` bookkeeping, and the tempo read + guard. It mutates only
    /// the atomic cursors (so it stays `&self` / RT-safe) and returns the
    /// window for [`Self::emit_window`] to walk.
    ///
    /// Returns `None` when nothing should be emitted this block — the
    /// transport is paused, or the tempo/sample-rate is non-positive.
    fn sync_to_transport(&self, block_size: usize) -> Option<PollWindow> {
        if !self.transport.is_playing() {
            // Track the beat anyway so a seek-while-paused doesn't
            // surprise us when playback resumes.
            self.last_beat
                .store(self.transport.current_beat(), Ordering::Release);
            return None;
        }

        let block_start_beat = self.transport.current_beat();
        let last_beat = self.last_beat.load(Ordering::Acquire);
        // Detect rewinds / seeks. Tolerate a tiny epsilon so float
        // jitter at exactly-equal beats doesn't trigger reseeking.
        if block_start_beat + 1e-9 < last_beat {
            self.rewind_to(block_start_beat);
        }
        self.last_beat.store(block_start_beat, Ordering::Release);

        let tempo_bpm = self.transport.tempo().get();
        if tempo_bpm <= 0.0 || self.sample_rate <= 0.0 {
            return None;
        }
        let beats_per_sample = tempo_bpm / 60.0 / self.sample_rate;
        Some(PollWindow {
            start_beat: block_start_beat,
            end_beat: block_start_beat + (block_size as f64) * beats_per_sample,
            beats_per_sample,
            max_offset: (block_size - 1) as u32,
        })
    }

    /// Emit events whose beat falls in `window`, stamping each with a
    /// sample-accurate `frame_offset`. Pure with respect to the transport —
    /// it reads the clip's own event list and cursor only.
    ///
    /// Advances the persisted cursor solely past events actually written to
    /// `out`; if `out` fills up, the remainder reappear on the next poll at
    /// the same beat.
    fn emit_window(&self, window: &PollWindow, out: &mut [MidiEvent]) -> usize {
        // Skip past anything before the window (cursor may have lagged due to
        // a seek, looping, or a buffer that filled up earlier).
        let mut cursor = self.cursor.load(Ordering::Relaxed) as usize;
        while cursor < self.events.len() && self.events[cursor].beat < window.start_beat {
            cursor += 1;
        }

        let mut written = 0;
        while cursor < self.events.len()
            && self.events[cursor].beat < window.end_beat
            && written < out.len()
        {
            let TimedClipEvent { beat, mut event } = self.events[cursor];
            let beat_delta = (beat - window.start_beat).max(0.0);
            let sample_offset = (beat_delta / window.beats_per_sample) as u32;
            event.frame_offset = sample_offset.min(window.max_offset);
            out[written] = event;
            written += 1;
            cursor += 1;
        }

        self.cursor.store(cursor as u64, Ordering::Release);
        written
    }
}

/// The beat range an audio block covers, plus the conversion factors needed to
/// place events inside it. Produced by [`MidiClipSource::sync_to_transport`],
/// consumed by [`MidiClipSource::emit_window`] — the explicit hand-off between
/// "what time is it" and "what to emit".
struct PollWindow {
    start_beat: f64,
    end_beat: f64,
    beats_per_sample: f64,
    max_offset: u32,
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
        let Some(window) = self.sync_to_transport(block_size) else {
            return 0;
        };
        self.emit_window(&window, out)
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
        MidiEvent::note_on(0, 0, note, tutti_midi_types::convert::midi1_velocity_to_midi2(vel))
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

    // --- isolated-half tests for the poll_into decomposition ----------------

    fn one_note_source(transport: &Arc<TestTransport>) -> MidiClipSource {
        MidiClipSource::new(
            MidiUnitId::new(1),
            vec![
                TimedClipEvent { beat: 0.0, event: note_on(60, 100) },
                TimedClipEvent { beat: 0.5, event: note_on(64, 100) },
            ],
            Arc::clone(transport) as Arc<dyn TransportReader>,
            44100.0,
        )
    }

    #[test]
    fn sync_to_transport_gates_and_rewinds() {
        let transport = Arc::new(TestTransport::new(120.0));
        let source = one_note_source(&transport);

        // Paused → no window, but the beat watermark is still tracked so a
        // seek-while-paused doesn't surprise us on resume.
        transport.playing.store(false, Ordering::Release);
        transport.set_beat(3.0);
        assert!(source.sync_to_transport(512).is_none());
        assert_eq!(source.last_beat.load(Ordering::Acquire), 3.0);

        // Playing → a window covering this block.
        transport.playing.store(true, Ordering::Release);
        transport.set_beat(0.5);
        let w = source.sync_to_transport(22050).expect("playing → window");
        assert_eq!(w.start_beat, 0.5);
        assert!(w.end_beat > 0.5);

        // Advance through both events (playhead moves forward to 2.0), so the
        // cursor is exhausted and last_beat is high. Then seek backward to 0:
        // the next sync must rewind the cursor so the events replay.
        let mut buf = [MidiEvent::noop(); 8];
        transport.set_beat(0.0);
        let _ = source.poll_into(MidiUnitId::new(1), 0, 44100, &mut buf); // drains both
        transport.set_beat(2.0);
        let _ = source.sync_to_transport(22050); // last_beat now ~2.0
        assert!(source.cursor.load(Ordering::Relaxed) >= 2);
        transport.set_beat(0.0); // genuine backward seek
        let _ = source.sync_to_transport(22050);
        assert_eq!(
            source.cursor.load(Ordering::Relaxed),
            0,
            "backward seek must rewind the cursor to the start"
        );
    }

    #[test]
    fn sync_to_transport_rejects_bad_tempo() {
        let transport = Arc::new(TestTransport::new(0.0)); // zero tempo
        let source = one_note_source(&transport);
        assert!(source.sync_to_transport(512).is_none());
    }

    #[test]
    fn emit_window_is_pure_and_offsets_correctly() {
        // emit_window touches no transport — feed it a hand-built window.
        let transport = Arc::new(TestTransport::new(120.0));
        let source = one_note_source(&transport);

        // 120 BPM @ 44.1kHz → 22050 samples/beat. Window [0.0, 1.0) covers both.
        let beats_per_sample = 120.0 / 60.0 / 44100.0;
        let window = PollWindow {
            start_beat: 0.0,
            end_beat: 1.0,
            beats_per_sample,
            max_offset: 22049,
        };
        let mut buf = [MidiEvent::noop(); 8];
        let n = source.emit_window(&window, &mut buf);
        assert_eq!(n, 2);
        assert_eq!(buf[0].frame_offset, 0); // beat 0.0
        assert!(
            (buf[1].frame_offset as i64 - 11025).abs() < 4, // beat 0.5
            "got {}",
            buf[1].frame_offset
        );
    }

    #[test]
    fn emit_window_respects_out_buffer_capacity() {
        let transport = Arc::new(TestTransport::new(120.0));
        let source = one_note_source(&transport);
        let beats_per_sample = 120.0 / 60.0 / 44100.0;
        let window = PollWindow {
            start_beat: 0.0,
            end_beat: 1.0,
            beats_per_sample,
            max_offset: 22049,
        };

        // out holds only 1 — the second event stays for the next poll.
        let mut buf = [MidiEvent::noop(); 1];
        assert_eq!(source.emit_window(&window, &mut buf), 1);
        // Cursor persisted only past the written event; the rest replays.
        let mut buf2 = [MidiEvent::noop(); 4];
        assert_eq!(source.emit_window(&window, &mut buf2), 1);
    }
}
