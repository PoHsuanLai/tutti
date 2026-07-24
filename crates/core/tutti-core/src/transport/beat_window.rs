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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatWindowSync {
    /// Transport is rolling and the window advances forward from the last block.
    Rolling,
    /// Transport jumped backwards — the caller should rewind its cursors to
    /// [`BeatWindow::start_beat`] before emitting.
    Rewound,
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
        sample_rate: f64,
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
        // Tolerate a tiny epsilon so float jitter at exactly-equal beats doesn't
        // trigger a spurious reseek.
        let sync = if start_beat + SEEK_EPSILON < *last_beat {
            BeatWindowSync::Rewound
        } else {
            BeatWindowSync::Rolling
        };
        *last_beat = start_beat;

        let tempo_bpm = timeline.tempo().get();
        if tempo_bpm <= 0.0 || sample_rate <= 0.0 || block_size == 0 {
            return None;
        }
        let beats_per_sample = tempo_bpm / 60.0 / sample_rate;
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
    sample_rate: f64,
    last_beat: Arc<AtomicF64>,
}

impl BeatCursor {
    pub fn new(transport: Arc<dyn Timeline>, sample_rate: f64) -> Self {
        Self {
            transport,
            sample_rate,
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

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}
