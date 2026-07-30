//! [`BeatWindow`] — the beat range one audio block covers.
//!
//! Every beat-scheduled source (MIDI clips, harmony chord/scale changes) has to
//! answer the same question each block: *given the live transport, which beats
//! does this block span, and where inside the block does a given beat land?*
//! The arithmetic is small but easy to get subtly wrong — the paused case, the
//! backward-seek epsilon, the tempo guard, the offset clamp — so it lives here
//! once rather than being re-derived per source.
//!
//! [`BeatCursor`] pairs that arithmetic with the caller's persisted
//! last-block beat, so the source keeps only what is genuinely its own.

use std::sync::Arc;

use crate::transport::Timeline;
use crate::AtomicF64;
use crate::SampleRate;

/// Beat range one audio block covers, plus the factors to place an event inside
/// it. Produced by [`BeatWindow::from_timeline`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatWindow {
    /// First beat of the block (inclusive).
    pub start_beat: f64,
    /// One past the last beat of the block (exclusive).
    pub end_beat: f64,
    /// Beats advanced per output sample — the beat↔sample conversion factor.
    pub beats_per_sample: f64,
    /// Largest in-block sample offset, i.e. `block_size - 1`. Offsets are
    /// clamped to this so a caller never splits past the end of its buffer.
    pub max_offset: u32,
}

/// What [`BeatWindow::from_timeline`] observed about the transport, so the
/// caller can react to a seek without re-reading the beat itself.
///
/// The two discontinuity variants stay distinct rather than collapsing into one
/// `Discontinuous`, because the two callers that predate them react differently.
/// A cursor into a sorted event list (`MidiClipSource`, `HarmonySource`) must
/// *rewind* on a backward jump, but a forward jump is self-correcting for it —
/// the cursor walks past stale events on its own. Merging them would force a
/// needless backward rescan on every forward scrub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatWindowSync {
    /// Transport is rolling and the window advances forward from the last block.
    Rolling,
    /// Transport jumped backwards — the caller should rewind its cursors to
    /// [`BeatWindow::start_beat`] before emitting.
    Rewound,
    /// Transport jumped forwards, past where continuous playback would have
    /// reached.
    ///
    /// Matters to anything holding *buffered audio* rather than a position:
    /// a phase vocoder's FIFO, an overlap-add tail, an interpolator's history.
    /// Those are not self-correcting — after a forward jump the buffer drains the
    /// old region's material over the new one — so they must flush, exactly as
    /// they would on [`Rewound`].
    Jumped,
}

impl BeatWindowSync {
    /// Whether the playhead moved discontinuously, in either direction.
    ///
    /// The question buffered state should ask. A cursor that only needs to know
    /// about rewinds should match on [`Rewound`](Self::Rewound) instead.
    #[inline]
    pub fn is_discontinuous(self) -> bool {
        matches!(self, Self::Rewound | Self::Jumped)
    }
}

impl BeatWindow {
    /// Reconcile against `timeline` and compute this block's window.
    ///
    /// Returns `None` when nothing should be emitted — the transport is paused,
    /// or the tempo / sample rate is non-positive. In the paused case the
    /// caller's `last_beat` is still updated (via `last_beat`'s in/out role
    /// below) so a seek-while-paused doesn't surprise playback on resume.
    ///
    /// `last_beat` is read *and* written: pass the caller's persisted
    /// last-block beat; it is updated to this block's start. A backward jump of
    /// more than `SEEK_EPSILON` reports [`BeatWindowSync::Rewound`] so the
    /// caller can reset its cursors.
    pub fn from_timeline(
        timeline: &dyn Timeline,
        sample_rate: SampleRate,
        block_size: usize,
        last_beat: &mut f64,
    ) -> Option<(Self, BeatWindowSync)> {
        if !timeline.is_rolling() {
            // Track the beat anyway so a seek-while-paused doesn't surprise us
            // when playback resumes.
            *last_beat = timeline.beat().get();
            return None;
        }

        let start_beat = timeline.beat().get();

        let tempo_bpm = timeline.tempo().get();
        if tempo_bpm <= 0.0 || sample_rate.get() <= 0.0 || block_size == 0 {
            // Still publish the beat: a caller resuming after a tempo glitch must
            // not read a stale `last_beat` as a jump.
            *last_beat = start_beat;
            return None;
        }
        let beats_per_sample = super::state::beats_per_sample(tempo_bpm, sample_rate).get();

        // Tolerate a tiny epsilon so float jitter at exactly-equal beats doesn't
        // trigger a spurious reseek.
        let sync = if start_beat + SEEK_EPSILON < *last_beat {
            BeatWindowSync::Rewound
        } else if *last_beat > f64::NEG_INFINITY
            && start_beat > *last_beat + forward_slack(beats_per_sample, block_size)
        {
            BeatWindowSync::Jumped
        } else {
            BeatWindowSync::Rolling
        };
        *last_beat = start_beat;
        Some((
            Self {
                start_beat,
                end_beat: start_beat + (block_size as f64) * beats_per_sample,
                beats_per_sample,
                max_offset: (block_size - 1) as u32,
            },
            sync,
        ))
    }

    /// Where `beat` lands inside this block, as a sample offset clamped to
    /// [`max_offset`](Self::max_offset). Beats at or before the window start
    /// map to 0.
    #[inline]
    pub fn offset_of(&self, beat: f64) -> u32 {
        let beat_delta = (beat - self.start_beat).max(0.0);
        ((beat_delta / self.beats_per_sample) as u32).min(self.max_offset)
    }

    /// Whether `beat` falls inside this block's `[start, end)` range.
    #[inline]
    pub fn contains(&self, beat: f64) -> bool {
        beat >= self.start_beat && beat < self.end_beat
    }
}

/// Backward-jump tolerance, in beats. Below this a beat decrease is treated as
/// float jitter rather than a seek.
const SEEK_EPSILON: f64 = 1e-9;

/// How far past the previous block's start the playhead may legitimately land
/// before it counts as a forward jump.
///
/// **Deliberately not [`SEEK_EPSILON`].** Backward and forward need different
/// tolerances, because normal playback advances forward: the expected step is
/// one block, and several ordinary things perturb it — a tempo ramp mid-block, a
/// changed block size, a dropped callback, a host that batches two blocks. A
/// 1e-9 threshold would call all of those a seek and flush on nearly every
/// block, which for a phase vocoder is a stutter.
///
/// `SLACK_BLOCKS` blocks of headroom, then. This is the beat-domain twin of the
/// sampler's `SEEK_EPSILON_SAMPLES = 4096` — one policy expressed in two units —
/// and like it, generous on purpose: a real seek moves the playhead far, so
/// there is no need to resolve small ones.
#[inline]
fn forward_slack(beats_per_sample: f64, block_size: usize) -> f64 {
    const SLACK_BLOCKS: f64 = 4.0;
    beats_per_sample * block_size as f64 * SLACK_BLOCKS
}

/// A beat-scheduled source's transport reading plus its persisted last-block
/// beat.
///
/// Every such source needs exactly this triple, and each one used to hold it
/// separately: an `Arc<dyn Timeline>`, a sample rate, and an `Arc<AtomicF64>`
/// cursor that it hand-lowered to a `&mut f64` for [`BeatWindow::from_timeline`]
/// and hand-raised afterwards. That dance had a trap in it — the paused path
/// still advances the cursor, so the store had to happen *before* the `?`, and
/// writing the call as one line silently broke seek-while-paused. Both existing
/// callers carried a comment warning about it.
///
/// Owning the cursor here removes the trap rather than documenting it: there is
/// no local to forget, and `?` cannot skip a write that happens inside
/// [`advance`](Self::advance).
///
/// Cheap to clone — shares the cursor, so fundsp's clone-on-commit of the
/// parent node does not restart playback.
#[derive(Clone)]
pub struct BeatCursor {
    transport: Arc<dyn Timeline>,
    sample_rate: SampleRate,
    last_beat: Arc<AtomicF64>,
}

impl BeatCursor {
    pub fn new(transport: Arc<dyn Timeline>, sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            transport,
            sample_rate: sample_rate.into(),
            last_beat: Arc::new(AtomicF64::new(f64::NEG_INFINITY)),
        }
    }

    /// This block's beat window, advancing the cursor.
    ///
    /// `None` when nothing should be emitted — the transport is paused, or the
    /// tempo / sample rate / block size is non-positive. The cursor advances on
    /// the paused path too, so a seek-while-paused does not surprise playback on
    /// resume.
    ///
    /// RT-safe: `&self`, no allocation, no locks.
    pub fn advance(&self, block_size: usize) -> Option<(BeatWindow, BeatWindowSync)> {
        let mut last = self.last_beat.load(crate::Ordering::Acquire);
        let out = BeatWindow::from_timeline(
            self.transport.as_ref(),
            self.sample_rate,
            block_size,
            &mut last,
        );
        // Written even on the paused path, so it is stored unconditionally —
        // this is the ordering the old call sites had to remember by hand.
        self.last_beat.store(last, crate::Ordering::Release);
        out
    }

    /// The live transport this cursor reads.
    pub fn timeline(&self) -> &Arc<dyn Timeline> {
        &self.transport
    }

    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Re-point at a new sample rate, as `AudioUnit::set_sample_rate` requires.
    ///
    /// Affects `beats_per_sample`, hence `end_beat` — and hence the forward-jump
    /// threshold, which is derived from it. A cursor left at a stale rate would
    /// mis-scale that slack: too small and ordinary playback reads as a seek, too
    /// large and a real seek goes unnoticed.
    pub fn set_sample_rate(&mut self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate = sample_rate.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Beat, Bpm};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Mock {
        rolling: AtomicBool,
        beat: AtomicU64,
        tempo: AtomicU64,
    }

    impl Mock {
        fn new(beat: f64, tempo: f64) -> Arc<Self> {
            Arc::new(Self {
                rolling: AtomicBool::new(true),
                beat: AtomicU64::new(beat.to_bits()),
                tempo: AtomicU64::new(tempo.to_bits()),
            })
        }
        fn set_beat(&self, beat: f64) {
            self.beat.store(beat.to_bits(), Ordering::Relaxed);
        }
        fn set_rolling(&self, rolling: bool) {
            self.rolling.store(rolling, Ordering::Relaxed);
        }
    }

    impl Timeline for Mock {
        fn beat(&self) -> Beat {
            Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
        }
        fn is_rolling(&self) -> bool {
            self.rolling.load(Ordering::Relaxed)
        }
        fn tempo(&self) -> Bpm {
            Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
    }

    const SR: f64 = 44_100.0;
    const BLOCK: usize = 64;

    /// One block of beats at 120 BPM and this sample rate.
    fn block_beats() -> f64 {
        BLOCK as f64 * 120.0 / 60.0 / SR
    }

    /// Contiguous playback must never report a discontinuity. This is the false
    /// positive that matters: a spurious flush every block turns a phase vocoder
    /// into a stutter, and no output-level test would catch it.
    #[test]
    fn contiguous_playback_stays_rolling() {
        let t = Mock::new(0.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);

        // First call has no previous beat, so it cannot judge continuity.
        let (_, first) = cursor.advance(BLOCK).unwrap();
        assert_eq!(first, BeatWindowSync::Rolling);

        for i in 1..200 {
            t.set_beat(i as f64 * block_beats());
            let (_, sync) = cursor.advance(BLOCK).unwrap();
            assert_eq!(sync, BeatWindowSync::Rolling, "block {i} misread as a seek");
        }
    }

    #[test]
    fn backward_jump_is_rewound() {
        let t = Mock::new(64.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);
        cursor.advance(BLOCK).unwrap();

        t.set_beat(8.0);
        let (_, sync) = cursor.advance(BLOCK).unwrap();
        assert_eq!(sync, BeatWindowSync::Rewound);
        assert!(sync.is_discontinuous());
    }

    /// The variant this commit adds. Before it, a forward seek was
    /// indistinguishable from normal advance — so buffered state never flushed
    /// and the fix built on `Rewound` alone would have covered only half the
    /// gesture.
    #[test]
    fn forward_jump_is_jumped() {
        let t = Mock::new(0.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);
        cursor.advance(BLOCK).unwrap();

        t.set_beat(64.0);
        let (_, sync) = cursor.advance(BLOCK).unwrap();
        assert_eq!(sync, BeatWindowSync::Jumped);
        assert!(sync.is_discontinuous());
    }

    /// A step slightly larger than one block is ordinary — a tempo ramp, a
    /// changed block size, a late callback — and must not read as a seek.
    #[test]
    fn a_small_forward_overshoot_is_not_a_jump() {
        let t = Mock::new(0.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);
        cursor.advance(BLOCK).unwrap();

        // Two blocks' worth: inside the slack.
        t.set_beat(block_beats() * 2.0);
        let (_, sync) = cursor.advance(BLOCK).unwrap();
        assert_eq!(sync, BeatWindowSync::Rolling);
    }

    /// Rolling and Rewound are the only two things a cursor-style consumer
    /// matches on today, so `Jumped` must not silently read as a rewind.
    #[test]
    fn jumped_is_not_rewound() {
        assert_ne!(BeatWindowSync::Jumped, BeatWindowSync::Rewound);
        assert!(!BeatWindowSync::Rolling.is_discontinuous());
    }

    /// A seek made while paused is **absorbed**, not reported.
    ///
    /// The paused path keeps updating `last_beat` (`from_timeline`'s first
    /// branch), so by the time playback resumes the cursor already agrees with
    /// the playhead and the first rolling block reads `Rolling`. That is the
    /// documented intent — "so a seek-while-paused doesn't surprise playback on
    /// resume" — and it is the right behaviour: nothing was mid-flight while
    /// stopped, so there is nothing stale to flush.
    ///
    /// The bug this guards against is the *opposite* one: if the paused branch
    /// ever stops publishing the beat, resume would report a phantom
    /// discontinuity on every stop/start cycle, flushing buffered state that was
    /// perfectly valid.
    #[test]
    fn a_seek_while_paused_is_absorbed_not_reported() {
        let t = Mock::new(64.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);
        cursor.advance(BLOCK).unwrap();

        t.set_rolling(false);
        assert!(cursor.advance(BLOCK).is_none(), "paused emits nothing");
        t.set_beat(8.0);
        assert!(cursor.advance(BLOCK).is_none());

        t.set_rolling(true);
        let (w, sync) = cursor.advance(BLOCK).unwrap();
        assert_eq!(
            sync,
            BeatWindowSync::Rolling,
            "the paused path reconciled the beat, so resume is continuous"
        );
        assert_eq!(w.start_beat, 8.0, "and it resumes at the sought position");
    }

    /// A plain stop/start with no seek must also be continuous — the same
    /// property, exercised the way a user actually hits the spacebar.
    #[test]
    fn stop_then_start_without_seeking_is_continuous() {
        let t = Mock::new(16.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);
        cursor.advance(BLOCK).unwrap();

        t.set_rolling(false);
        assert!(cursor.advance(BLOCK).is_none());
        t.set_rolling(true);

        let (_, sync) = cursor.advance(BLOCK).unwrap();
        assert_eq!(sync, BeatWindowSync::Rolling);
    }

    /// A non-positive tempo yields no window, but must still track the beat —
    /// otherwise the first good block after the glitch reads a stale `last_beat`
    /// and reports a phantom jump.
    #[test]
    fn a_tempo_glitch_does_not_produce_a_phantom_jump() {
        let t = Mock::new(0.0, 120.0);
        let cursor = BeatCursor::new(t.clone(), SR);
        cursor.advance(BLOCK).unwrap();

        t.tempo.store(0.0f64.to_bits(), Ordering::Relaxed);
        t.set_beat(64.0);
        assert!(cursor.advance(BLOCK).is_none(), "no tempo, no window");

        // Tempo returns; the playhead has not moved since.
        t.tempo.store(120.0f64.to_bits(), Ordering::Relaxed);
        let (_, sync) = cursor.advance(BLOCK).unwrap();
        assert_eq!(sync, BeatWindowSync::Rolling);
    }

    #[test]
    fn window_spans_one_block_and_places_offsets() {
        let t = Mock::new(4.0, 120.0);
        let cursor = BeatCursor::new(t, SR);
        let (w, _) = cursor.advance(BLOCK).unwrap();

        assert_eq!(w.start_beat, 4.0);
        assert!((w.end_beat - (4.0 + block_beats())).abs() < 1e-12);
        assert_eq!(w.max_offset, BLOCK as u32 - 1);
        assert_eq!(w.offset_of(4.0), 0);
        assert_eq!(w.offset_of(0.0), 0, "beats before the window clamp to 0");
        assert_eq!(
            w.offset_of(1_000.0),
            w.max_offset,
            "beats past the window clamp to the last offset"
        );
        assert!(w.contains(4.0));
        assert!(!w.contains(w.end_beat));
    }
}
