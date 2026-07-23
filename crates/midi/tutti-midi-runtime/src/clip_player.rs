//! Beat-scheduled MIDI clip playback as a [`MidiIn`].
//!
//! A `MidiClipSource` holds a sorted `Vec<TimedClipEvent>` (events tagged
//! with absolute beats) and a [`Timeline`]. On each
//! `poll_into(unit_id, block_size, …)` it reads the transport beat,
//! computes the beat range covered by the upcoming audio block, and emits
//! events whose beat falls in that range with their `frame_offset` set to
//! the sample-accurate position inside the block.
//!
//! Installing a source on a [`MidiInPort`](crate::MidiInPort) *replaces* its
//! live receiver, so a synth plays either its clip or live preview events, not
//! both. Layering the two would be a change to `MidiInPort::poll`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_core::transport::{BeatWindow, BeatWindowSync, Timeline};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::unit_id::MidiUnitId;
use tutti_midi_types::{MidiIn, MidiOut};

/// One MIDI event scheduled at an absolute beat — the clip player's name for the
/// runtime's one timed-event type, [`TimedMidiEvent`](crate::TimedMidiEvent).
///
/// It is the *same type*, so a `MidiSnapshot`'s events, a parsed clip file's
/// `(beat, event)` tuples, and a clip all speak one currency and move between
/// each other without repacking. Build with `TimedMidiEvent::new(beat, event)`,
/// `.into()` from a `(beat, event)` tuple, or the field literal (`beat`/`event`).
pub type TimedClipEvent = crate::snapshot::TimedMidiEvent;

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
    transport: Arc<dyn Timeline>,
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
    /// Optional **hardware-out tap**. When present, every event this source emits
    /// to the synth is *also* handed to this [`MidiOut`] (already sample-stamped),
    /// for an off-RT pump to forward to external MIDI. `None` for the ordinary
    /// case.
    ///
    /// It is a `dyn MidiOut` — the audio-thread write side of the two MIDI traits,
    /// whose `queue(&self, …)` is contractually lock-free — so the clip player
    /// needs no ring/lock machinery of its own. Shared across fundsp's
    /// clone-on-commit like [`Self::cursor`].
    ///
    /// The paired `out_tap_unit` is the [`MidiUnitId`] the tap routes on — the
    /// *mailbox's* id, distinct from [`Self::target_unit`] (the synth address).
    /// The tee queues to this id so a [`MidiSender`](crate::MidiSender) tap
    /// accepts it (its `MidiOut` impl drops mismatched ids).
    out_tap: Option<(Arc<dyn MidiOut>, MidiUnitId)>,
}

impl MidiClipSource {
    /// Build a clip source. `events` must be sorted ascending by beat.
    pub fn new(
        target_unit: MidiUnitId,
        events: impl IntoIterator<Item = TimedClipEvent>,
        transport: Arc<dyn Timeline>,
        sample_rate: f64,
    ) -> Self {
        let mut v: Vec<TimedClipEvent> = events.into_iter().collect();
        v.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            events: v.into(),
            transport,
            sample_rate,
            cursor: Arc::new(AtomicU64::new(0)),
            last_beat: Arc::new(AtomicF64::new(f64::NEG_INFINITY)),
            target_unit,
            out_tap: None,
        }
    }

    /// Attach a hardware-out tap: every event this source emits to the synth is
    /// *also* handed to `tap`, addressed to `tap_unit` (already sample-stamped).
    /// Use when the parent track routes its clip MIDI out to hardware; the
    /// `MidiOut` is typically a [`MidiSender`](crate::MidiSender) whose receiver
    /// an off-RT pump drains, and `tap_unit` is that sender's own id (not this
    /// source's `target_unit`).
    pub fn with_out_tap(mut self, tap: Arc<dyn MidiOut>, tap_unit: MidiUnitId) -> Self {
        self.out_tap = Some((tap, tap_unit));
        self
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
    fn sync_to_transport(&self, block_size: usize) -> Option<BeatWindow> {
        // `BeatWindow` owns the paused check, the seek epsilon, the tempo guard
        // and the offset clamp; this method keeps only what is clip-specific —
        // publishing `last_beat` back to the shared atomic and rewinding the
        // cursor when the transport jumped backwards.
        let mut last_beat = self.last_beat.load(Ordering::Acquire);
        let synced = BeatWindow::from_timeline(
            self.transport.as_ref(),
            self.sample_rate,
            block_size,
            &mut last_beat,
        );
        // `from_timeline` updates `last_beat` even on the paused path, which is
        // why it is stored before the `?`.
        self.last_beat.store(last_beat, Ordering::Release);

        let (window, sync) = synced?;
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
            // `MidiOut` trait (lock-free, drops if full — benign backpressure),
            // addressed to the tap's own mailbox id (not the synth target).
            if let Some((tap, tap_unit)) = &self.out_tap {
                tap.queue(*tap_unit, &[event]);
            }
            written += 1;
            cursor += 1;
        }

        self.cursor.store(cursor as u64, Ordering::Release);
        written
    }
}

impl MidiIn for MidiClipSource {
    fn poll_into(
        &self,
        unit_id: MidiUnitId,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tutti_core::params::Bpm;

    /// Minimal `Timeline` for tests: tempo + beat under a switch.
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

    impl Timeline for TestTransport {
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(self.beat.load(Ordering::Acquire))
        }
        fn loop_range(&self) -> Option<tutti_core::LoopRange> {
            None
        }
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn tempo(&self) -> Bpm {
            Bpm(self.tempo)
        }
    }

    fn note_on(note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(
            0,
            0,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(vel),
        )
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
            Arc::clone(&transport) as Arc<dyn Timeline>,
            sample_rate,
        );

        // First block: cover [0.0, 1.0) beats = [0, 22050) samples.
        let mut buf = [MidiEvent::noop(); 8];
        let n = source.poll_into(unit, 22050, &mut buf);
        assert_eq!(n, 2);
        assert_eq!(buf[0].frame_offset, 0);
        // Second event at beat 0.5 → 11025 samples.
        assert!(
            (buf[1].frame_offset as i64 - 11025).abs() < 4,
            "got {}",
            buf[1].frame_offset
        );

        // Polling again at the same beat: cursor advanced, no new events.
        let n2 = source.poll_into(unit, 22050, &mut buf);
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
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_into(other, 1024, &mut buf), 0);
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
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_into(unit, 1024, &mut buf), 0);
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
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );

        let mut buf = [MidiEvent::noop(); 4];

        // First block @ beat 0
        assert_eq!(source.poll_into(unit, 22050, &mut buf), 2);
        // Move forward — cursor exhausted, nothing emitted.
        transport.set_beat(2.0);
        assert_eq!(source.poll_into(unit, 22050, &mut buf), 0);
        // Seek back to start — events should fire again.
        transport.set_beat(0.0);
        assert_eq!(source.poll_into(unit, 22050, &mut buf), 2);
    }

    #[test]
    fn out_tap_forwards_the_same_stamped_events() {
        use crate::registry::MidiMailbox;

        let unit = MidiUnitId::new(3);
        let transport = Arc::new(TestTransport::new(120.0));
        // The tap is a plain MidiOut → MidiIn mailbox (the real wiring): the clip
        // pushes into the sender, an off-RT drain reads the receiver. The mailbox
        // has its *own* id, distinct from the clip's synth-routing `target_unit`
        // — the tee addresses the mailbox id, not `target_unit`.
        let tap_unit = MidiUnitId::new(999);
        let (sender, receiver) = MidiMailbox::pair(tap_unit);

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
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        )
        .with_out_tap(Arc::new(sender), tap_unit);

        // Poll one block wide enough to cover both events.
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_into(unit, 22050, &mut buf), 2);

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
        let transport = Arc::new(TestTransport::new(120.0));
        let source = MidiClipSource::new(
            unit,
            vec![TimedClipEvent {
                beat: 0.0,
                event: note_on(60, 100),
            }],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut buf = [MidiEvent::noop(); 4];
        assert_eq!(source.poll_into(unit, 22050, &mut buf), 1);
    }

    // --- isolated-half tests for the poll_into decomposition ----------------

    fn one_note_source(transport: &Arc<TestTransport>) -> MidiClipSource {
        MidiClipSource::new(
            MidiUnitId::new(1),
            vec![
                TimedClipEvent {
                    beat: 0.0,
                    event: note_on(60, 100),
                },
                TimedClipEvent {
                    beat: 0.5,
                    event: note_on(64, 100),
                },
            ],
            Arc::clone(transport) as Arc<dyn Timeline>,
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
        let _ = source.poll_into(MidiUnitId::new(1), 44100, &mut buf); // drains both
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
        let window = BeatWindow {
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
        let window = BeatWindow {
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
