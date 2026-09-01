//! Beat-scheduled MIDI clip playback as a [`MidiUnitIn`].
//!
//! A `MidiClipSource` holds a sorted `Vec<TimedClipEvent>` (events tagged
//! with absolute beats) and a [`Timeline`]. On each
//! `poll_into(unit_id, block_size, …)` it reads the transport beat,
//! computes the beat range covered by the upcoming audio block, and emits
//! events whose beat falls in that range with their `frame_offset` set to
//! the sample-accurate position inside the block.
//!
//! Installing a source on a [`MidiInPort`](crate::MidiInPort) *layers* it over
//! that port's live receiver, so a synth plays its clip and still answers the
//! keyboard.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::transport::{BeatCursor, BeatWindow, BeatWindowSync, Timeline};
use tutti_core::{Beat, SampleRate};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;
use tutti_midi_types::{MidiOut, MidiUnitIn};

/// One MIDI event scheduled at an absolute beat — the clip player's name for the
/// runtime's one timed-event type, [`TimedMidiEvent`](crate::TimedMidiEvent).
///
/// It is the *same type*, so a `MidiSnapshot`'s events, a parsed clip file's
/// `(beat, event)` tuples, and a clip all speak one currency and move between
/// each other without repacking. Build with `TimedMidiEvent::new(beat, event)`,
/// `.into()` from a `(beat, event)` tuple, or the field literal (`beat`/`event`).
pub type TimedClipEvent = super::snapshot::TimedMidiEvent;

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
    /// The live transport plus this source's last-block beat. Owns the
    /// backwards-seek detection, so the rewind rule lives in one place rather
    /// than at each poll site.
    beats: BeatCursor,
    /// Index into `events` of the first event not yet emitted.
    /// Atomic so `poll_into` is `&self`.
    cursor: Arc<AtomicU64>,
    /// Only events targeting this unit are emitted. Events whose unit
    /// doesn't match are skipped (a single composite source can fan to
    /// many synths via per-unit clip players).
    target_unit: MidiUnitId,
    /// Optional **hardware-out tap**. When present, every event this source emits
    /// to the synth is *also* handed to this [`MidiOut`] (already sample-stamped),
    /// for an off-RT pump to forward to external MIDI. `None` for the ordinary
    /// case.
    ///
    /// It is a `dyn MidiOut` — a terminal sink (typically a
    /// [`MidiSender`](crate::MidiSender)) whose `queue(&self, …)` is contractually
    /// lock-free — so the clip player needs no ring/lock machinery of its own.
    /// Shared across fundsp's clone-on-commit like [`Self::cursor`]. Being a sink,
    /// it carries its own address; the tee needs no id.
    out_tap: Option<Arc<dyn MidiOut>>,
}

impl MidiClipSource {
    /// Build a clip source. `events` must be sorted ascending by beat.
    pub fn new(
        target_unit: MidiUnitId,
        events: impl IntoIterator<Item = TimedClipEvent>,
        transport: Arc<dyn Timeline>,
        sample_rate: SampleRate,
    ) -> Self {
        let mut v: Vec<TimedClipEvent> = events.into_iter().collect();
        v.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            events: v.into(),
            beats: BeatCursor::new(transport, sample_rate),
            cursor: Arc::new(AtomicU64::new(0)),
            target_unit,
            out_tap: None,
        }
    }

    /// Attach a hardware-out tap: every event this source emits to the synth is
    /// *also* handed to `tap` (already sample-stamped). Use when the parent track
    /// routes its clip MIDI out to hardware; the `MidiOut` is typically a
    /// [`MidiSender`](crate::MidiSender) whose receiver an off-RT pump drains.
    pub fn with_out_tap(mut self, tap: Arc<dyn MidiOut>) -> Self {
        self.out_tap = Some(tap);
        self
    }

    /// The [`MidiUnitId`] this source emits for. Events in the shared list
    /// addressed to any other unit are skipped, which is what lets one event
    /// list back several per-unit players.
    pub fn target_unit(&self) -> MidiUnitId {
        self.target_unit
    }

    /// How many events the backing list holds **in total**, across every unit —
    /// not the count this source will emit. Filtering to
    /// [`target_unit`](Self::target_unit) happens per poll.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Reset the cursor to the start. Used after a transport seek.
    fn rewind_to(&self, beat: Beat) {
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
    fn sync_to_transport(&self, block_size: usize) -> Option<BeatWindow> {
        // `BeatCursor` owns the paused check, the seek epsilon, the tempo guard,
        // the offset clamp, and publishing the cursor; all that is left here is
        // the clip-specific part — rewinding on a backwards jump.
        let (window, sync) = self.beats.advance(block_size)?;
        if sync == BeatWindowSync::Rewound {
            self.rewind_to(window.start_beat);
        }
        Some(window)
    }

    /// Emit events whose beat falls in `window`, stamping each with a
    /// sample-accurate `frame_offset`. Pure with respect to the transport —
    /// it reads the clip's own event list and cursor only.
    ///
    /// Advances the persisted cursor solely past events actually written to
    /// `out`; if `out` fills up, the remainder reappear on the next poll at
    /// the same beat.
    fn emit_window(&self, window: &BeatWindow, out: &mut [MidiEvent]) -> usize {
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
            event.frame_offset = window.offset_of(beat);
            out[written] = event;
            // Hardware-out tap: forward the same sample-stamped event through the
            // `MidiOut` sink (lock-free, drops if full — benign backpressure).
            if let Some(tap) = &self.out_tap {
                tap.queue(&[event]);
            }
            written += 1;
            cursor += 1;
        }

        self.cursor.store(cursor as u64, Ordering::Release);
        written
    }
}

impl MidiUnitIn for MidiClipSource {
    /// The `unit_id` check against `target_unit` is the selector contract, not a
    /// redundant guard: a clip is addressed to one unit, and polling it for
    /// another must yield nothing.
    fn poll_unit(&self, unit_id: MidiUnitId, block_size: usize, out: &mut [MidiEvent]) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF64;
    use std::sync::atomic::AtomicBool;
    use tutti_core::BeatDuration;
    use tutti_core::Bpm;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// Minimal `Timeline` for tests: tempo + beat under a switch.
    struct TestTransport {
        beat: AtomicF64,
        tempo: Bpm,
        playing: AtomicBool,
    }

    impl TestTransport {
        fn new(tempo: Bpm) -> Self {
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

    impl Timeline for TestTransport {
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(self.beat.load(Ordering::Acquire))
        }
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn tempo(&self) -> Bpm {
            self.tempo
        }
    }

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
        )
    }

    #[test]
    fn emits_event_at_correct_frame_offset() {
        let unit = MidiUnitId::new(1);
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        // 120 BPM @ 44.1kHz → 22050 samples/beat → ~22.05 samples per 0.001 beat.
        let sample_rate = SampleRate::from(44100.0);

        let events = vec![
            TimedClipEvent {
                beat: Beat(0.0),
                event: note_on(60, 100),
            },
            TimedClipEvent {
                beat: Beat(0.5),
                event: note_on(64, 100),
            },
        ];
        let source = MidiClipSource::new(
            unit,
            events,
            Arc::clone(&transport) as Arc<dyn Timeline>,
            sample_rate,
        );

        // First block: cover [0.0, 1.0) beats = [0, 22050) samples.
        let mut buf = [MidiEvent::noop(); 8];
        let n = source.poll_unit(unit, 22050, &mut buf);
        assert_eq!(n, 2);
        assert_eq!(buf[0].frame_offset, 0);
        // Second event at beat 0.5 → 11025 samples.
        assert!(
            (buf[1].frame_offset as i64 - 11025).abs() < 4,
            "got {}",
            buf[1].frame_offset
        );

        // Polling again at the same beat: cursor advanced, no new events.
        let n2 = source.poll_unit(unit, 22050, &mut buf);
        assert_eq!(n2, 0);
    }

    #[test]
    fn ignores_other_unit_ids() {
        let unit = MidiUnitId::new(1);
        let other = MidiUnitId::new(2);
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        let source = MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: Beat(0.0),
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            SampleRate::from(44100.0),
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_unit(other, 1024, &mut buf), 0);
    }

    #[test]
    fn paused_transport_emits_nothing() {
        let unit = MidiUnitId::new(1);
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        transport.playing.store(false, Ordering::Release);
        let source = MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: Beat(0.0),
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            SampleRate::from(44100.0),
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_unit(unit, 1024, &mut buf), 0);
    }

    #[test]
    fn seek_backwards_replays_events() {
        let unit = MidiUnitId::new(1);
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        let source = MidiClipSource::new(
            unit,
            vec![
                TimedClipEvent {
                    beat: Beat(0.0),
                    event: note_on(60, 100),
                },
                TimedClipEvent {
                    beat: Beat(0.25),
                    event: note_on(64, 100),
                },
            ],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            SampleRate::from(44100.0),
        );

        let mut buf = [MidiEvent::noop(); 4];

        // First block @ beat 0
        assert_eq!(source.poll_unit(unit, 22050, &mut buf), 2);
        // Move forward — cursor exhausted, nothing emitted.
        transport.set_beat(2.0);
        assert_eq!(source.poll_unit(unit, 22050, &mut buf), 0);
        // Seek back to start — events should fire again.
        transport.set_beat(0.0);
        assert_eq!(source.poll_unit(unit, 22050, &mut buf), 2);
    }

    #[test]
    fn out_tap_forwards_the_same_stamped_events() {
        use crate::block::registry::MidiMailbox;

        let unit = MidiUnitId::new(3);
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        // The tap is a plain MidiOut → mailbox (the real wiring): the clip
        // pushes into the sender, an off-RT drain reads the receiver. The sender is
        // a terminal sink — the tee pushes at it with no id.
        let (sender, receiver) = MidiMailbox::pair(MidiUnitId::new(999));

        let source = MidiClipSource::new(
            unit,
            vec![
                TimedClipEvent {
                    beat: Beat(0.0),
                    event: note_on(60, 100),
                },
                TimedClipEvent {
                    beat: Beat(0.25),
                    event: note_on(64, 100),
                },
            ],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            SampleRate::from(44100.0),
        )
        .with_out_tap(Arc::new(sender));

        // Poll one block wide enough to cover both events.
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_unit(unit, 22050, &mut buf), 2);

        // The tap received the *same* events the synth did, sample-stamped.
        let mut tapped = [MidiEvent::noop(); 4];
        let n = receiver.poll_into(&mut tapped);
        assert_eq!(n, 2, "tap forwards both emitted events");
        assert_eq!(tapped[0].note(), buf[0].note());
        assert_eq!(tapped[1].note(), buf[1].note());
        assert_eq!(
            tapped[0].frame_offset, buf[0].frame_offset,
            "stamp preserved"
        );
        assert_eq!(
            tapped[1].frame_offset, buf[1].frame_offset,
            "stamp preserved"
        );
    }

    #[test]
    fn no_tap_emits_only_to_synth() {
        // Without a tap, behavior is unchanged (regression guard).
        let unit = MidiUnitId::new(4);
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        let source = MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: Beat(0.0),
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            SampleRate::from(44100.0),
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_unit(unit, 22050, &mut buf), 1);
    }

    // --- isolated-half tests for the poll_into decomposition ----------------

    fn one_note_source(transport: &Arc<TestTransport>) -> MidiClipSource {
        MidiClipSource::new(
            MidiUnitId::new(1),
            vec![
                TimedClipEvent {
                    beat: Beat(0.0),
                    event: note_on(60, 100),
                },
                TimedClipEvent {
                    beat: Beat(0.5),
                    event: note_on(64, 100),
                },
            ],
            Arc::clone(transport) as Arc<dyn Timeline>,
            SampleRate::from(44100.0),
        )
    }

    #[test]
    fn sync_to_transport_gates_and_rewinds() {
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        let source = one_note_source(&transport);

        // Paused → no window, but the beat watermark is still tracked so a
        // seek-while-paused does not surprise the next resume. Observable through
        // the rewind below: if the paused poll had skipped the cursor write,
        // resuming lower would not register as a backwards jump.
        transport.playing.store(false, Ordering::Release);
        transport.set_beat(3.0);
        assert!(source.sync_to_transport(512).is_none());

        // Playing, and *below* the paused watermark → a window, and the
        // backwards jump is detected.
        transport.playing.store(true, Ordering::Release);
        transport.set_beat(0.5);
        let w = source.sync_to_transport(22050).expect("playing → window");
        assert_eq!(w.start_beat, Beat(0.5));
        assert!(w.end_beat > Beat(0.5));

        // Advance through both events (playhead moves forward to 2.0), so the
        // cursor is exhausted and last_beat is high. Then seek backward to 0:
        // the next sync must rewind the cursor so the events replay.
        let mut buf = [MidiEvent::noop(); 8];
        transport.set_beat(0.0);
        let _ = source.poll_unit(MidiUnitId::new(1), 44100, &mut buf); // drains both
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
        let transport = Arc::new(TestTransport::new(Bpm(0.0))); // zero tempo
        let source = one_note_source(&transport);
        assert!(source.sync_to_transport(512).is_none());
    }

    // NOTE: `emit_window`'s beat→frame mapping is pinned publicly by
    // `emits_event_at_correct_frame_offset`, which drives the same two events
    // through `poll_unit` (whose whole body is `sync_to_transport` + this) and
    // asserts the same two offsets — 0 and ~11025.

    #[test]
    fn emit_window_respects_out_buffer_capacity() {
        let transport = Arc::new(TestTransport::new(Bpm(120.0)));
        let source = one_note_source(&transport);
        let beats_per_sample = BeatDuration(120.0 / 60.0 / 44100.0);
        let window = BeatWindow {
            start_beat: Beat(0.0),
            end_beat: Beat(1.0),
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
