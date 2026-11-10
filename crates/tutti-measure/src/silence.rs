//! Strip silence: the regions of a buffer that hold signal.
//!
//! A sample is *loud* if its magnitude reaches the threshold. A run of
//! non-loud samples at least `min_silence` long cuts the audio in two; shorter
//! runs (a sine's zero crossings, a drum's decay dipping under the line) are
//! kept inside the region. Each region is then widened by the padding and
//! regions that touch after widening are joined.
//!
//! Shaped like onset detection: [`step_silence`] folds chunks into a
//! [`SilenceState`] and [`finish`] reads the regions, and
//! [`strip_silence`] is that fold over one buffer, so batch and streaming
//! cannot drift.

use core::ops::Range;

use tutti_types::{Db, SampleRate, Samples, Seconds};

use crate::{Error, Result};

/// A half-open span of samples, `start..end`.
///
/// Built only by [`finish`], so `start <= end` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    start: Samples,
    end: Samples,
}

impl Region {
    /// The first sample in the region, in [`Samples`].
    #[inline]
    pub fn start(&self) -> Samples {
        self.start
    }

    /// One past the last sample in the region, in [`Samples`].
    #[inline]
    pub fn end(&self) -> Samples {
        self.end
    }

    /// How many samples the region spans.
    #[inline]
    pub fn len(&self) -> Samples {
        Samples(self.end.get() - self.start.get())
    }

    /// Whether the region spans nothing.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The region as a slice index.
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.start.get()..self.end.get()
    }
}

/// Strip-silence parameters. Immutable once built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceConfig {
    sample_rate: SampleRate,
    threshold: Db,
    /// `threshold` as a linear magnitude, resolved once.
    floor: f32,
    min_silence: Samples,
    pre_pad: Samples,
    post_pad: Samples,
}

impl SilenceConfig {
    /// A threshold and the shortest run below it that cuts the audio, with no
    /// padding.
    ///
    /// A `min_silence` under one sample cuts at every sample below the
    /// threshold.
    ///
    /// # Errors
    ///
    /// [`Error::NonPositiveSampleRate`] for a rate that is zero, negative or
    /// NaN, and [`Error::InvalidThreshold`] for a threshold that is not finite
    /// (a NaN would make every config unequal to itself and restart the scan
    /// on each chunk).
    pub fn new(
        sample_rate: impl Into<SampleRate>,
        threshold: Db,
        min_silence: impl Into<Seconds>,
    ) -> Result<Self> {
        let rate = sample_rate.into();
        #[allow(
            clippy::neg_cmp_op_on_partial_ord,
            reason = "the negation is the NaN guard: `rate <= 0.0` would accept a NaN rate"
        )]
        if !(rate.get() > 0.0) {
            return Err(Error::NonPositiveSampleRate);
        }
        if !threshold.get().is_finite() {
            return Err(Error::InvalidThreshold { threshold });
        }
        Ok(Self {
            sample_rate: rate,
            threshold,
            floor: threshold.to_amplitude().get(),
            min_silence: min_silence.into().to_samples(rate).max(Samples(1)),
            pre_pad: Samples::ZERO,
            post_pad: Samples::ZERO,
        })
    }

    /// Audio kept before the first loud sample of a region and after its last,
    /// so an attack or a tail is not clipped.
    #[must_use]
    pub fn with_padding(mut self, pre: impl Into<Seconds>, post: impl Into<Seconds>) -> Self {
        self.pre_pad = pre.into().to_samples(self.sample_rate);
        self.post_pad = post.into().to_samples(self.sample_rate);
        self
    }

    /// The level at or above which a sample counts as signal.
    #[inline]
    pub fn threshold(&self) -> Db {
        self.threshold
    }
}

/// What the scan carries between chunks.
///
/// Valid for one config; a different one starts over, as in
/// [`OnsetState`](crate::OnsetState).
#[derive(Debug, Clone, Default)]
pub struct SilenceState {
    config: Option<SilenceConfig>,
    /// Samples consumed so far.
    seen: usize,
    /// The region being grown: first and last loud sample.
    open: Option<(usize, usize)>,
    /// Regions already cut off, as `(first loud, one past last loud)`.
    closed: Vec<(usize, usize)>,
}

impl SilenceState {
    /// An empty carry, before the first sample.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets everything. Equivalent to a fresh state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// How many samples have been fed.
    #[inline]
    pub fn samples_seen(&self) -> Samples {
        Samples(self.seen)
    }
}

/// Feeds the next chunk of the buffer, in order.
///
/// Not for the audio thread: the state keeps one entry per region found, so it
/// grows with the audio and allocates. Feed it from the offline or UI side.
pub fn step_silence(cfg: &SilenceConfig, state: &mut SilenceState, samples: &[f32]) {
    if state.config != Some(*cfg) {
        state.reset();
        state.config = Some(*cfg);
    }
    for (offset, &sample) in samples.iter().enumerate() {
        // Below the floor, or NaN, is silence.
        if sample.abs() < cfg.floor || sample.is_nan() {
            continue;
        }
        let at = state.seen + offset;
        state.open = match state.open {
            None => Some((at, at)),
            // The gap is `at - last - 1` samples: split once it reaches the minimum.
            Some((first, last)) if at - last > cfg.min_silence.get() => {
                state.closed.push((first, last + 1));
                Some((at, at))
            }
            Some((first, _)) => Some((first, at)),
        };
    }
    state.seen += samples.len();
}

/// The regions of the audio fed so far, padded, clamped to the buffer and
/// joined where they touch. Ascending and disjoint.
///
/// Uses the config the state was stepped with, so the padding cannot disagree
/// with the scan. A state that has not been stepped has no regions.
pub fn finish(state: &SilenceState) -> Vec<Region> {
    let Some(cfg) = state.config else {
        return Vec::new();
    };
    let open = state.open.map(|(first, last)| (first, last + 1));
    let mut regions: Vec<Region> = Vec::new();
    for &(first, end) in state.closed.iter().chain(open.iter()) {
        let start = Samples(first.saturating_sub(cfg.pre_pad.get()));
        let end = Samples(end.saturating_add(cfg.post_pad.get()).min(state.seen));
        match regions.last_mut() {
            Some(prev) if start <= prev.end => prev.end = prev.end.max(end),
            _ => regions.push(Region { start, end }),
        }
    }
    regions
}

/// The signal regions of a whole buffer: [`step_silence`] over it, then
/// [`finish`].
///
/// Mono. Fold a multichannel buffer first (`Interleaved::fold_into`), or take the
/// union of per-channel results.
pub fn strip_silence(cfg: &SilenceConfig, samples: &[f32]) -> Vec<Region> {
    let mut state = SilenceState::new();
    step_silence(cfg, &mut state, samples);
    finish(&state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 1000.0; // 1 sample = 1 ms keeps the arithmetic readable

    fn cfg(threshold: Db, min_silence_ms: f32) -> SilenceConfig {
        SilenceConfig::new(
            SampleRate::new(RATE),
            threshold,
            Seconds::from_f32(min_silence_ms / 1000.0),
        )
        .unwrap()
    }

    fn r(start: usize, end: usize) -> Region {
        Region {
            start: Samples(start),
            end: Samples(end),
        }
    }

    /// tone(100) silence(300) tone(100) silence(50), with nothing padded: two
    /// regions with exact boundaries, the trailing silence dropped.
    ///
    /// Mutation checked: ending a region at the last loud sample (`last`, not
    /// `last + 1`) fails the ends; starting a region after the gap's first
    /// loud sample fails the second start.
    #[test]
    fn tone_gap_tone_gives_two_exact_regions() {
        let mut x = vec![0.0f32; 550];
        x[..100].fill(0.5);
        x[400..500].fill(0.5);
        let regions = strip_silence(&cfg(Db(-20.0), 50.0), &x);
        assert_eq!(regions, vec![r(0, 100), r(400, 500)]);
        assert_eq!(regions[0].len(), Samples(100));
        assert_eq!(&x[regions[1].range()], &[0.5f32; 100][..]);
    }

    /// Gaps shorter than the minimum do not split; one at the minimum does.
    ///
    /// Mutation checked: a strict gap comparison (`at - last > min + 1`) fails the
    /// 50-sample gap; the off-by-one `at - last >= min` splits the 49-sample gap
    /// and fails.
    #[test]
    fn a_gap_splits_only_at_the_minimum_length() {
        let build = |gap: usize| {
            let mut x = vec![0.0f32; 200 + gap];
            x[..100].fill(0.5);
            x[100 + gap..].fill(0.5);
            x
        };
        let c = cfg(Db(-20.0), 50.0);
        assert_eq!(strip_silence(&c, &build(49)), vec![r(0, 249)]);
        assert_eq!(strip_silence(&c, &build(50)), vec![r(0, 100), r(150, 250)]);
    }

    /// A sample exactly at the threshold is loud; the next float below is not.
    ///
    /// Mutation checked: `<=` for `<` in the silence test (so the exact sample
    /// is silent) fails the first assertion.
    #[test]
    fn the_threshold_edge_is_inclusive() {
        let c = cfg(Db(-20.0), 10.0);
        let floor = Db(-20.0).to_amplitude().get();
        let below = f32::from_bits(floor.to_bits() - 1);
        let mut x = vec![0.0f32; 100];
        x[10] = floor;
        x[60] = below;
        assert_eq!(strip_silence(&c, &x), vec![r(10, 11)]);
        // Sign does not matter.
        x[10] = -floor;
        assert_eq!(strip_silence(&c, &x), vec![r(10, 11)]);
    }

    /// Padding widens each side, clamps at the buffer and joins regions that
    /// meet.
    ///
    /// Mutation checked: dropping the `.min(state.seen)` clamp fails the tail;
    /// the `saturating_sub` replaced by a plain subtraction panics on the head;
    /// dropping the join fails the gap-50 pair, and `<` for `<=` in it (so
    /// exactly-touching regions stay apart) fails the same pair.
    #[test]
    fn padding_widens_clamps_and_joins() {
        let mut x = vec![0.0f32; 1000];
        x[5..10].fill(1.0);
        x[500..510].fill(1.0);
        x[990..995].fill(1.0);
        let c = cfg(Db(-20.0), 10.0).with_padding(Seconds(0.02), Seconds(0.03));
        // 5..10 -> 0..40; 500..510 -> 480..540; 990..995 -> 970..1000.
        assert_eq!(
            strip_silence(&c, &x),
            vec![r(0, 40), r(480, 540), r(970, 1000)]
        );

        // Bursts 100..110 and then `gap` later: the first pads to 80..140, the
        // second starts at `90 + gap`. They meet exactly at gap 50 and join;
        // at 51 they stay one sample apart.
        let pair = |gap: usize| {
            let mut x = vec![0.0f32; 400];
            x[100..110].fill(1.0);
            x[110 + gap..120 + gap].fill(1.0);
            strip_silence(&c, &x)
        };
        assert_eq!(pair(50), vec![r(80, 200)]);
        assert_eq!(pair(51), vec![r(80, 140), r(141, 201)]);
    }

    /// Silence only, empty input and NaN give no regions.
    ///
    /// Mutation checked: treating NaN as loud (testing `!(abs < floor)` without
    /// the NaN clause) fails the last assertion.
    #[test]
    fn nothing_loud_gives_no_regions() {
        let c = cfg(Db(-20.0), 10.0);
        assert!(strip_silence(&c, &[0.0; 64]).is_empty());
        assert!(strip_silence(&c, &[]).is_empty());
        assert!(strip_silence(&c, &[f32::NAN; 8]).is_empty());
    }

    /// Feeding in chunks of any size gives the batch result, including a chunk
    /// edge inside a gap and inside a region, and a region open at the end.
    ///
    /// Mutation checked: resetting `open` at the start of each `step_silence`
    /// fails the chunked runs; indexing positions from the chunk (without
    /// `state.seen`) fails them too.
    #[test]
    fn chunked_feeding_matches_the_batch() {
        let mut x = vec![0.0f32; 700];
        x[3..40].fill(0.9);
        x[200..260].fill(-0.9);
        x[400..401].fill(0.9);
        x[690..].fill(0.9);
        let c = cfg(Db(-30.0), 40.0).with_padding(Seconds(0.005), Seconds(0.01));
        let batch = strip_silence(&c, &x);
        assert_eq!(batch.len(), 4);
        for chunk in [1usize, 7, 64, 199, 700] {
            let mut state = SilenceState::new();
            for part in x.chunks(chunk) {
                step_silence(&c, &mut state, part);
            }
            assert_eq!(state.samples_seen(), Samples(700));
            assert_eq!(finish(&state), batch, "chunk {chunk}");
        }
    }

    /// A different config restarts the carry.
    ///
    /// Mutation checked: removing the config comparison in `step_silence`
    /// carries the 100-sample burst across and fails.
    #[test]
    fn a_changed_config_restarts_the_scan() {
        let mut state = SilenceState::new();
        step_silence(&cfg(Db(-20.0), 10.0), &mut state, &[0.5; 100]);
        let other = cfg(Db(-10.0), 10.0);
        step_silence(&other, &mut state, &[0.5; 10]);
        assert_eq!(finish(&state), vec![r(0, 10)]);
    }

    /// A state that was never stepped has no regions, and the padding comes
    /// from the config the scan used: `finish` takes nothing else.
    ///
    /// Mutation checked: `finish` returning the unpadded regions (ignoring
    /// `cfg.pre_pad`/`post_pad`) fails the second assertion. (The unstepped
    /// case is also empty under a default config, so the first assertion pins
    /// the defined behaviour rather than catching a mutation.)
    #[test]
    fn finish_reads_the_scanned_config_and_an_unstepped_state_is_empty() {
        assert!(finish(&SilenceState::new()).is_empty());
        let c = cfg(Db(-20.0), 10.0).with_padding(Seconds(0.004), Seconds(0.006));
        let mut state = SilenceState::new();
        let mut x = vec![0.0f32; 100];
        x[50..60].fill(1.0);
        step_silence(&c, &mut state, &x);
        assert_eq!(finish(&state), vec![r(46, 66)]);
        state.reset();
        assert!(finish(&state).is_empty());
    }

    /// A threshold that is not finite is refused: NaN would never equal itself.
    ///
    /// Mutation checked: removing the `is_finite` check fails all three.
    #[test]
    fn a_non_finite_threshold_is_refused() {
        for t in [f32::NAN, f32::NEG_INFINITY, f32::INFINITY] {
            assert!(matches!(
                SilenceConfig::new(SampleRate::new(RATE), Db(t), Seconds(0.1)),
                Err(Error::InvalidThreshold { .. })
            ));
        }
    }

    #[test]
    fn a_bad_rate_is_refused() {
        for rate in [0.0, -1.0, f64::NAN] {
            assert_eq!(
                SilenceConfig::new(SampleRate::new(rate), Db(-20.0), Seconds(0.1)),
                Err(Error::NonPositiveSampleRate)
            );
        }
    }
}
