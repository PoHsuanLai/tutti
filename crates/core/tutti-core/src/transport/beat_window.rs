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
use tutti_types::value::units::{Beat, BeatDuration};

/// Beat range one audio block covers, plus the factors to place an event inside
/// it. Produced by [`BeatWindow::from_timeline`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatWindow {
    /// First beat of the block (inclusive).
    pub start_beat: Beat,
    /// One past the last beat of the block (exclusive).
    pub end_beat: Beat,
    /// Beats advanced per output sample — the beat↔sample conversion factor.
    ///
    /// A span, not a position: two `Beat`s and one `BeatDuration`, so plugging
    /// the rate into a position slot no longer compiles.
    pub beats_per_sample: BeatDuration,
    /// Largest in-block sample offset, i.e. `block_size - 1`. Offsets are
    /// clamped to this so a caller never splits past the end of its buffer.
    pub max_offset: u32,
}

/// What [`BeatWindow::from_timeline`] observed about the transport, so the
/// caller can react to a seek without re-reading the beat itself.
///
/// The two discontinuity variants stay distinct rather than collapsing into one
/// `Discontinuous`, because callers react to them differently. A cursor into a
/// sorted event list (`MidiClipSource`, `HarmonySource`) must *rewind* on a
/// backward jump, but a forward jump is self-correcting for it — the cursor
/// walks past stale events on its own. Merging them would force a needless
/// backward rescan on every forward scrub.
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
    /// they would on [`Rewound`](Self::Rewound).
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
        last_beat: &mut Beat,
    ) -> Option<(Self, BeatWindowSync)> {
        if !timeline.is_rolling() {
            // Track the beat anyway so a seek-while-paused does not read as a
            // discontinuity when playback resumes.
            *last_beat = timeline.beat();
            return None;
        }

        let start_beat = timeline.beat();

        let tempo_bpm = timeline.tempo().get();
        if tempo_bpm <= 0.0 || sample_rate.get() <= 0.0 || block_size == 0 {
            // Still publish the beat: a caller resuming after a tempo glitch must
            // not read a stale `last_beat` as a jump.
            *last_beat = start_beat;
            return None;
        }
        let beats_per_sample = super::state::beats_per_sample(tempo_bpm, sample_rate);

        // Tolerate a tiny epsilon so float jitter at exactly-equal beats doesn't
        // trigger a spurious reseek.
        let sync = if start_beat + SEEK_EPSILON < *last_beat {
            BeatWindowSync::Rewound
        } else if *last_beat > Beat(f64::NEG_INFINITY)
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
                end_beat: start_beat + beats_per_sample * block_size as f64,
                beats_per_sample,
                max_offset: (block_size - 1) as u32,
            },
            sync,
        ))
    }

    /// Where `beat` lands inside this block, as a sample offset clamped to
    /// [`max_offset`](Self::max_offset). Beats at or before the window start
    /// map to 0.
    ///
    /// The one home for beat→in-block-offset, so every caller gets the same
    /// zero-guard. A non-positive rate yields offset 0 rather than dividing to
    /// infinity, which the `max_offset` clamp would otherwise turn into "the
    /// last sample of the block" — every event in a stalled block bunched at
    /// its tail.
    #[inline]
    pub fn offset_of(&self, beat: Beat) -> u32 {
        if self.beats_per_sample <= BeatDuration(0.0) {
            return 0;
        }
        let beat_delta = (beat - self.start_beat).max(BeatDuration(0.0));
        ((beat_delta / self.beats_per_sample) as u32).min(self.max_offset)
    }

    /// Whether `beat` falls inside this block's `[start, end)` range.
    #[inline]
    pub fn contains(&self, beat: Beat) -> bool {
        beat >= self.start_beat && beat < self.end_beat
    }
}

/// Backward-jump tolerance, in beats. Below this a beat decrease is treated as
/// float jitter rather than a seek.
const SEEK_EPSILON: BeatDuration = BeatDuration(1e-9);

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
fn forward_slack(beats_per_sample: BeatDuration, block_size: usize) -> BeatDuration {
    const SLACK_BLOCKS: f64 = 4.0;
    beats_per_sample * block_size as f64 * SLACK_BLOCKS
}

/// A beat-scheduled source's transport reading plus its persisted last-block
/// beat.
///
/// Every such source needs exactly this triple, and holding it loose has a trap
/// in it: [`BeatWindow::from_timeline`] advances the cursor on the **paused**
/// path too, so a caller lowering its own `Arc<AtomicF64>` to a `&mut Beat` must
/// store the result *before* any `?` on the returned `Option`. Written as one
/// line, that silently breaks seek-while-paused.
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
    /// Shared for the same reason `last_beat` is: a cursor is cloned along with
    /// whatever owns it, and fundsp commits a *different clone* than a setter
    /// would touch. A plain field is not merely stale after a device rate
    /// change — it is unreachable, since the owner sits behind an `Arc` and
    /// there is no `&mut` to the running copy.
    sample_rate: Arc<AtomicF64>,
    last_beat: Arc<AtomicF64>,
}

impl BeatCursor {
    /// A cursor over `transport` with no previous block.
    ///
    /// The first [`advance`](Self::advance) has nothing to compare against, so
    /// it always reports [`BeatWindowSync::Rolling`] however far into a session
    /// the playhead already is.
    pub fn new(transport: Arc<dyn Timeline>, sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            transport,
            sample_rate: Arc::new(AtomicF64::new(sample_rate.into().get())),
            last_beat: Arc::new(AtomicF64::new(f64::NEG_INFINITY)),
        }
    }

    /// A cursor over `transport` that holds **no** sample rate: it is only
    /// advanced with [`advance_at`](Self::advance_at), which is handed the
    /// rate each block. [`advance`](Self::advance) on it emits nothing (its
    /// rate is zero, which the window refuses).
    pub fn unrated(transport: Arc<dyn Timeline>) -> Self {
        Self::new(transport, SampleRate(0.0))
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
        self.advance_at(block_size, self.sample_rate())
    }

    /// [`advance`](Self::advance) at `sample_rate`, handed in by the caller
    /// rather than read from the cursor: for a source polled by a unit that
    /// knows its own rate each block (a MIDI clip, polled through its unit's
    /// port), so no copy of the rate is kept to fall out of step with the
    /// unit's. Build such a cursor with [`unrated`](Self::unrated).
    pub fn advance_at(
        &self,
        block_size: usize,
        sample_rate: SampleRate,
    ) -> Option<(BeatWindow, BeatWindowSync)> {
        let mut last = Beat(self.last_beat.load(crate::Ordering::Acquire));
        let out =
            BeatWindow::from_timeline(self.transport.as_ref(), sample_rate, block_size, &mut last);
        // Stored unconditionally: `from_timeline` writes `last` on the paused
        // path too, so an early return here would drop that update and make the
        // resume look like a jump.
        self.last_beat.store(last.get(), crate::Ordering::Release);
        out
    }

    /// The live transport this cursor reads.
    pub fn timeline(&self) -> &Arc<dyn Timeline> {
        &self.transport
    }

    /// The rate this cursor converts beats against, as last set.
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate::from(self.sample_rate.load(crate::Ordering::Acquire))
    }

    /// Re-point at a new sample rate, as `AudioUnit::set_sample_rate` requires.
    ///
    /// Affects `beats_per_sample`, hence `end_beat` — and hence the forward-jump
    /// threshold, which is derived from it. A cursor left at a stale rate would
    /// mis-scale that slack: too small and ordinary playback reads as a seek, too
    /// large and a real seek goes unnoticed.
    ///
    /// `&self`, not `&mut`: the rate is a shared atomic, so this reaches every
    /// clone including the one fundsp is running.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate
            .store(sample_rate.into().get(), crate::Ordering::Release);
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

    /// A forward seek must be distinguishable from a normal advance. Without
    /// `Jumped` the two read alike, buffered state never flushes, and anything
    /// keyed on `Rewound` alone covers only half the gesture.
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
        assert_eq!(
            w.start_beat,
            Beat(8.0),
            "and it resumes at the sought position"
        );
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

        assert_eq!(w.start_beat, Beat(4.0));
        assert!((w.end_beat - Beat(4.0 + block_beats())).abs() < BeatDuration(1e-12));
        assert_eq!(w.max_offset, BLOCK as u32 - 1);
        assert_eq!(w.offset_of(Beat(4.0)), 0);
        assert_eq!(
            w.offset_of(Beat(0.0)),
            0,
            "beats before the window clamp to 0"
        );
        assert_eq!(
            w.offset_of(Beat(1_000.0)),
            w.max_offset,
            "beats past the window clamp to the last offset"
        );
        assert!(w.contains(Beat(4.0)));
        assert!(!w.contains(w.end_beat));
    }

    /// A zero rate places events at the *start* of the block, not the end.
    ///
    /// Without the zero-guard the division goes to infinity and the
    /// `min(max_offset)` clamp turns that into "last sample of the block", so
    /// every event in a stalled block bunches at its tail — the opposite end of
    /// the buffer from where a guarded conversion puts them.
    ///
    /// A window this degenerate only arises if one is built by hand —
    /// `from_timeline` rejects a non-positive tempo before constructing one —
    /// but the fields are `pub`, so the constructor is not the only way in.
    #[test]
    fn a_stalled_rate_puts_events_at_the_block_start() {
        let w = BeatWindow {
            start_beat: Beat(1.0),
            end_beat: Beat(2.0),
            beats_per_sample: BeatDuration(0.0),
            max_offset: 511,
        };
        assert_eq!(
            w.offset_of(Beat(1.5)),
            0,
            "a zero rate cannot place an event; it must not slam it to the tail"
        );
    }

    /// The three window fields are two positions and a span, so transposing a
    /// position and the rate is a type error rather than a silent misplacement.
    /// The fields are `pub` and the struct is built by hand outside this crate,
    /// so the types are the only thing standing between them.
    #[test]
    fn the_rate_and_the_bounds_are_not_the_same_type() {
        let w = BeatWindow {
            start_beat: Beat(0.0),
            end_beat: Beat(1.0),
            beats_per_sample: BeatDuration(0.5),
            max_offset: 1,
        };
        // Positions subtract to a span; the span is what the rate divides.
        assert_eq!((w.end_beat - w.start_beat) / w.beats_per_sample, 2.0);
    }
}
