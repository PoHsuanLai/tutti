//! Beat-scheduled chord / scale playback as a harmony producer.
//!
//! The VST3 counterpart of [`tutti_midi_runtime::MidiClipSource`]: a
//! [`HarmonySource`] holds sorted, beat-tagged [`ChordValue`] / [`ScaleValue`]
//! changes and a [`Timeline`]. Each block it reads the transport beat,
//! computes the beat range the upcoming block covers, and fills a reused
//! [`HarmonyInputs`] with the changes whose beat falls in that range — their
//! `sample_offset` stamped to the sample-accurate position inside the block.
//!
//! Chord / scale are *stepwise context*, not discrete note events: a change
//! holds until the next one. So unlike MIDI, it is not re-emitted per poll —
//! only when a change boundary actually falls in the block. (A plugin that
//! joins mid-clip and wants the *current* context can be re-primed by a seek;
//! priming-on-join is out of scope for this first cut, matching how the host
//! only feeds harmony "when a producer supplies it.")

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::transport::{BeatCursor, BeatWindow, BeatWindowSync, Timeline};
use tutti_core::{Beat, SampleRate};

use crate::host::ipc_client::audio::HarmonyInputs;
use crate::host::node::input_slot::{BlockCtx, BlockInput, BlockReset};
use crate::protocol::{ChordValue, ScaleValue};

/// A chord change scheduled at an absolute beat. The `value`'s `sample_offset`
/// is filled per block by [`HarmonySource::refill`].
#[derive(Clone, Debug)]
pub struct TimedChord {
    /// Absolute timeline position of the change.
    pub beat: Beat,
    /// The chord taking effect, whose `sample_offset` is filled per block.
    pub value: ChordValue,
}

/// A scale change scheduled at an absolute beat.
#[derive(Clone, Debug)]
pub struct TimedScale {
    /// Absolute timeline position of the change.
    pub beat: Beat,
    /// The scale taking effect, whose `sample_offset` is filled per block.
    pub value: ScaleValue,
}

/// Beat-scheduled chord/scale producer. Cheap to clone (atomics shared) so the
/// fundsp graph-commit clone of the parent node doesn't restart playback.
#[derive(Clone)]
pub struct HarmonySource {
    chords: Arc<[TimedChord]>,
    scales: Arc<[TimedScale]>,
    /// The live transport plus this source's last-block beat.
    beats: BeatCursor,
    chord_cursor: Arc<AtomicU64>,
    scale_cursor: Arc<AtomicU64>,
}

impl HarmonySource {
    /// Build a harmony source. `chords` / `scales` need not be pre-sorted —
    /// the constructor sorts them ascending by beat.
    pub fn new(
        chords: impl IntoIterator<Item = TimedChord>,
        scales: impl IntoIterator<Item = TimedScale>,
        transport: Arc<dyn Timeline>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        let mut c: Vec<TimedChord> = chords.into_iter().collect();
        c.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut s: Vec<TimedScale> = scales.into_iter().collect();
        s.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            chords: c.into(),
            scales: s.into(),
            beats: BeatCursor::new(transport, sample_rate),
            chord_cursor: Arc::new(AtomicU64::new(0)),
            scale_cursor: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Update the stamped sample rate live (device / rate switch). Reaches the
    /// running box because the cursor's rate is a shared atomic.
    pub fn set_sample_rate(&self, sample_rate: impl Into<tutti_core::SampleRate>) {
        self.beats.set_sample_rate(sample_rate);
    }

    /// Whether this source holds no chords and no scales, in which case every
    /// block feeds empty harmony context.
    pub fn is_empty(&self) -> bool {
        self.chords.is_empty() && self.scales.is_empty()
    }

    /// Reconcile against the live transport and compute this block's beat
    /// window. The transport arithmetic — paused check, seek epsilon, tempo
    /// guard, offset clamp — is [`BeatWindow`]'s, shared with
    /// `MidiClipSource`; only the two-cursor rewind is harmony-specific.
    fn window(&self, block_size: usize) -> Option<BeatWindow> {
        let (window, sync) = self.beats.advance(block_size)?;
        if sync == BeatWindowSync::Rewound {
            // Backward seek: rewind both cursors to the new position.
            self.rewind(
                &self.chord_cursor,
                self.chords.iter().map(|c| c.beat),
                window.start_beat,
            );
            self.rewind(
                &self.scale_cursor,
                self.scales.iter().map(|s| s.beat),
                window.start_beat,
            );
        }
        Some(window)
    }

    fn rewind(&self, cursor: &AtomicU64, beats: impl Iterator<Item = Beat>, beat: Beat) {
        let mut idx = 0usize;
        for b in beats {
            if b < beat {
                idx += 1;
            } else {
                break;
            }
        }
        cursor.store(idx as u64, Ordering::Release);
    }

    /// Fill `out` (cleared first) with the chord/scale changes whose beat falls
    /// in this block's window. Returns the number of (chord + scale) changes
    /// written. RT-safe: no allocation beyond the `SmallVec` push, which only
    /// grows past inline capacity if many changes land in one block.
    pub fn refill(&self, block_size: usize, out: &mut HarmonyInputs) {
        out.chords.changes.clear();
        out.scales.changes.clear();
        if block_size == 0 {
            return;
        }
        let Some(window) = self.window(block_size) else {
            return;
        };
        Self::emit(&self.chords, &self.chord_cursor, &window, |tc, off| {
            let mut v = tc.value.clone();
            v.sample_offset = off;
            out.chords.changes.push(v);
        });
        Self::emit(&self.scales, &self.scale_cursor, &window, |ts, off| {
            let mut v = ts.value.clone();
            v.sample_offset = off;
            out.scales.changes.push(v);
        });
    }

    /// Walk a sorted change list, emitting every entry in the window with a
    /// sample-accurate offset; advances the persisted cursor past what it
    /// emitted. Generic over chord/scale via the `beat_of` + `push` closures.
    fn emit<T: HasBeat>(
        items: &[T],
        cursor: &AtomicU64,
        window: &BeatWindow,
        mut push: impl FnMut(&T, i32),
    ) {
        let mut idx = cursor.load(Ordering::Relaxed) as usize;
        while idx < items.len() && items[idx].beat() < window.start_beat {
            idx += 1;
        }
        while idx < items.len() && items[idx].beat() < window.end_beat {
            // Harmony change offsets are `i32` on the wire; the window clamps to
            // `block_size - 1`, so the cast is always in range.
            let off = window.offset_of(items[idx].beat()) as i32;
            push(&items[idx], off);
            idx += 1;
        }
        cursor.store(idx as u64, Ordering::Release);
    }
}

impl BlockInput for HarmonySource {
    type Out = HarmonyInputs;
    fn refill(&self, ctx: BlockCtx, out: &mut HarmonyInputs) {
        // Inherent `refill` self-clears chords/scales, so it satisfies the
        // "fully overwrite `out`" contract.
        HarmonySource::refill(self, ctx.block_size, out);
    }
}

impl BlockReset for HarmonyInputs {
    fn reset(&mut self) {
        self.chords.changes.clear();
        self.scales.changes.clear();
        self.expr_texts.changes.clear();
        self.expr_ints.changes.clear();
    }
}

/// Lets [`HarmonySource::emit`] read the beat of either change kind.
trait HasBeat {
    fn beat(&self) -> Beat;
}
impl HasBeat for TimedChord {
    fn beat(&self) -> Beat {
        self.beat
    }
}
impl HasBeat for TimedScale {
    fn beat(&self) -> Beat {
        self.beat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF64;
    use std::sync::atomic::AtomicBool;
    use tutti_core::Bpm;

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
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn tempo(&self) -> Bpm {
            Bpm(self.tempo)
        }
    }

    fn chord(beat: f64, root: i16, name: &str) -> TimedChord {
        TimedChord {
            beat: Beat(beat),
            value: ChordValue {
                sample_offset: 0,
                root,
                bass_note: root,
                mask: 0,
                text: name.to_string(),
            },
        }
    }
    fn scale(beat: f64, root: i16, name: &str) -> TimedScale {
        TimedScale {
            beat: Beat(beat),
            value: ScaleValue {
                sample_offset: 0,
                root,
                mask: 0,
                text: name.to_string(),
            },
        }
    }

    #[test]
    fn emits_change_in_window_with_offset() {
        let transport = Arc::new(TestTransport::new(120.0)); // 22050 samples/beat @ 44.1k
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C"), chord(0.5, 7, "G")],
            vec![scale(0.0, 0, "C major")],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        // Block covering [0.0, 1.0) beats = [0, 22050) samples → both chords.
        src.refill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 2);
        assert_eq!(out.scales.changes.len(), 1);
        assert_eq!(out.chords.changes[0].sample_offset, 0); // beat 0.0
                                                            // beat 0.5 → ~11025 samples.
        assert!((out.chords.changes[1].sample_offset - 11025).abs() < 4);
    }

    #[test]
    fn no_changes_after_window_passes() {
        let transport = Arc::new(TestTransport::new(120.0));
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C")],
            Vec::new(),
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        src.refill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 1);
        // Advance the transport past the only change — nothing more.
        transport.set_beat(2.0);
        src.refill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 0);
    }

    #[test]
    fn paused_emits_nothing() {
        let transport = Arc::new(TestTransport::new(120.0));
        transport.playing.store(false, Ordering::Release);
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C")],
            Vec::new(),
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        src.refill(512, &mut out);
        assert!(out.chords.changes.is_empty());
    }

    #[test]
    fn backward_seek_replays() {
        let transport = Arc::new(TestTransport::new(120.0));
        let src = HarmonySource::new(
            vec![chord(0.0, 0, "C"), chord(0.25, 7, "G")],
            Vec::new(),
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = HarmonyInputs::default();
        src.refill(22050, &mut out); // both
        assert_eq!(out.chords.changes.len(), 2);
        transport.set_beat(2.0);
        src.refill(22050, &mut out); // none
        assert_eq!(out.chords.changes.len(), 0);
        transport.set_beat(0.0); // seek back
        src.refill(22050, &mut out);
        assert_eq!(out.chords.changes.len(), 2);
    }
}
