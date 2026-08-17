//! Inter-channel phase correlation.
//!
//! The measurement a mastering engineer watches to catch mono-compatibility
//! problems: `+1` is identical channels, `0` uncorrelated, `-1` polarity
//! inverted and cancelling.
//!
//! Split into a pure kernel ([`correlate`]) and an explicit smoother
//! ([`step_ballistics`]). Folding the two together invites a `width` stored
//! beside the correlation it is defined from and smoothed independently of it,
//! so the smoothed pair contradicts the invariant the kernel guarantees.

use tutti_types::{Amplitude, Correlation, Db, Pan, Seconds, StereoPlanes, StereoWidth};

/// Mid/side and per-channel levels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StereoLevels {
    /// RMS [`Amplitude`] of the mid (sum) component.
    pub mid: Amplitude,
    /// RMS [`Amplitude`] of the side (difference) component.
    pub side: Amplitude,
    /// RMS [`Amplitude`] of the left channel.
    pub left: Amplitude,
    /// RMS [`Amplitude`] of the right channel.
    pub right: Amplitude,
}

impl StereoLevels {
    /// Mid-to-side ratio in dB. Positive is more mid — a narrower image.
    ///
    /// Clamped to ±60 dB and never infinite: a function whose name promises dB
    /// must not hand a meter an infinity to propagate.
    pub fn ms_ratio(&self) -> Db {
        // A meter range, deliberately not `Db::FLOOR`: this is a ratio between
        // two amplitudes on a ±60 dB scale, not a level pinned at the noise
        // floor. `Db::from_amplitude` leaves the floor to its consumer, and
        // this consumer's floor is the `-60` below, applied by the clamp.
        //
        // The three early returns cover every non-positive input, so the
        // conversion never sees one — the clamp bounds the finite range, not
        // the silent case.
        const FLOOR: Db = Db(-60.0);
        const CEILING: Db = Db(60.0);

        if self.mid.get() <= 0.0 && self.side.get() <= 0.0 {
            return Db(0.0);
        }
        if self.side.get() <= 0.0 {
            return CEILING;
        }
        if self.mid.get() <= 0.0 {
            return FLOOR;
        }
        Db::from_amplitude(Amplitude(self.mid.get() / self.side.get())).clamp(FLOOR, CEILING)
    }
}

/// One correlation reading.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StereoReading {
    /// Inter-channel phase [`Correlation`], `-1..=1`: `+1` identical channels,
    /// `0` uncorrelated, `-1` polarity inverted. A measurement, deliberately
    /// not `Depth` despite the coinciding range.
    pub correlation: Correlation,
    /// Where the energy sits on the left/right axis, as a [`Pan`] reading.
    pub balance: Pan,
    /// Mid/side and per-channel RMS levels for this block.
    pub levels: StereoLevels,
}

impl StereoReading {
    /// The image width this correlation implies.
    ///
    /// Derived, not stored — a stored field can be smoothed independently of
    /// the correlation, letting the pair violate `width == 1 - correlation`.
    #[inline]
    pub fn width(&self) -> StereoWidth {
        self.correlation.to_stereo_width()
    }

    /// Whether the correlation is negative enough that a mono fold would
    /// cancel audibly.
    #[inline]
    pub fn has_phase_issues(&self) -> bool {
        self.correlation.has_phase_issues()
    }

    /// Whether the channels are near-identical (correlation above 0.95).
    #[inline]
    pub fn is_mono(&self) -> bool {
        self.correlation > Correlation(0.95)
    }
}

/// Correlate one block of stereo audio. Stateless.
///
/// Takes a [`StereoPlanes`] rather than two loose slices: mid/side and L/R
/// correlation are only defined at exactly two channels, and every statistic
/// below divides by a single frame count. Reconciling the pair here with
/// `left.len().min(right.len())` silently measures the shorter of two
/// mismatched blocks; the pairing cannot be formed at all unless they agree.
pub fn correlate(planes: StereoPlanes<'_>) -> StereoReading {
    let n = planes.frames();
    if n == 0 {
        return StereoReading::default();
    }
    let (left, right) = (planes.left(), planes.right());

    // f64 accumulators: these sums run over whole blocks and f32 loses
    // precision quickly on the squared terms.
    let (mut sum_l, mut sum_r, mut sum_lr, mut sum_ll, mut sum_rr) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    let (mut sum_mid, mut sum_side) = (0.0f64, 0.0f64);

    for i in 0..n {
        let (l, r) = (left[i] as f64, right[i] as f64);
        sum_l += l * l;
        sum_r += r * r;
        sum_lr += l * r;
        sum_ll += l * l;
        sum_rr += r * r;

        let mid = (l + r) * 0.5;
        let side = (l - r) * 0.5;
        sum_mid += mid * mid;
        sum_side += side * side;
    }

    let count = n as f64;
    let rms = |sum: f64| Amplitude((sum / count).sqrt() as f32);

    let denominator = (sum_ll * sum_rr).sqrt();
    let correlation = if denominator > 1e-20 {
        Correlation::new_clamped((sum_lr / denominator) as f32)
    } else {
        Correlation::UNCORRELATED
    };

    let (level_l, level_r) = (rms(sum_l), rms(sum_r));
    let total = level_l.get() + level_r.get();
    let balance = if total > 1e-20 {
        Pan::new_clamped((level_r.get() - level_l.get()) / total)
    } else {
        Pan::CENTER
    };

    StereoReading {
        correlation,
        balance,
        levels: StereoLevels {
            mid: rms(sum_mid),
            side: rms(sum_side),
            left: level_l,
            right: level_r,
        },
    }
}

/// Meter ballistics: how fast a reading rises and falls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ballistics {
    attack: Seconds,
    release: Seconds,
}

impl Ballistics {
    /// Both times in [`Seconds`], like every other time-valued setter in the
    /// engine. As two adjacent bare `f32` milliseconds, transposing them
    /// compiles and makes the meter sluggish to peaks and instant to release —
    /// under-reporting exactly the problems it exists to show.
    pub fn new(attack: impl Into<Seconds>, release: impl Into<Seconds>) -> Self {
        Self {
            attack: attack.into(),
            release: release.into(),
        }
    }

    /// 10 ms attack, 100 ms release — standard meter ballistics.
    pub fn standard() -> Self {
        Self::new(Seconds(0.01), Seconds(0.1))
    }

    /// How far to move toward a new value over `dt`, in `[0, 1]`.
    ///
    /// Public because ballistics are not stereo-specific: a level meter smooths
    /// [`MeterReading`](tutti_core::MeterReading)'s four amplitudes
    /// with the same attack and release, and the alternative is a second copy
    /// of `1 - exp(-dt/t)` that can drift from this one. [`step_ballistics`]
    /// stays the convenience for the stereo case.
    ///
    /// A zero or negative time means "no smoothing" and returns 1.0, so a
    /// caller that has not configured a rise or fall gets the instantaneous
    /// value rather than a division by zero.
    #[inline]
    pub fn coefficient(&self, rising: bool, dt: Seconds) -> f32 {
        let time = if rising { self.attack } else { self.release };
        if time.get() <= 0.0 {
            return 1.0;
        }
        1.0 - (-dt.get() / time.get()).exp()
    }

    /// Move `old` toward `new` by one step of `dt`.
    ///
    /// The rise/fall decision is made from the two values rather than passed
    /// in, which is what stops a caller smoothing a falling level with the
    /// attack coefficient — the transposition that makes a meter instant to
    /// release and sluggish to peaks, under-reporting what it exists to show.
    #[inline]
    pub fn step(&self, old: f32, new: f32, dt: Seconds) -> f32 {
        old + (new - old) * self.coefficient(new > old, dt)
    }
}

/// The smoothed reading carried between blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct BallisticsState {
    current: StereoReading,
}

impl BallisticsState {
    /// A state seeded with a default (silent, uncorrelated) reading.
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest smoothed reading.
    #[inline]
    pub fn current(&self) -> StereoReading {
        self.current
    }

    /// Discard the smoothed reading, so the next step starts from silence
    /// rather than decaying from the previous signal.
    pub fn reset(&mut self) {
        self.current = StereoReading::default();
    }
}

/// Smooth an instantaneous reading and return the result.
///
/// Returns the *smoothed* value, which is the useful one — returning the
/// instantaneous reading instead and hiding the smoothed one behind a separate
/// `current()` call makes the obvious use of the return value the wrong one.
///
/// Only `correlation` and the levels are smoothed; width is derived from the
/// smoothed correlation afterwards, so the two cannot disagree.
pub fn step_ballistics(
    cfg: &Ballistics,
    state: &mut BallisticsState,
    instant: StereoReading,
    dt: Seconds,
) -> StereoReading {
    let smooth = |old: f32, new: f32| cfg.step(old, new, dt);

    let correlation = Correlation::new_clamped(smooth(
        state.current.correlation.get(),
        instant.correlation.get(),
    ));
    let balance = Pan::new_clamped(smooth(state.current.balance.get(), instant.balance.get()));

    let levels = StereoLevels {
        mid: Amplitude(smooth(
            state.current.levels.mid.get(),
            instant.levels.mid.get(),
        )),
        side: Amplitude(smooth(
            state.current.levels.side.get(),
            instant.levels.side.get(),
        )),
        left: Amplitude(smooth(
            state.current.levels.left.get(),
            instant.levels.left.get(),
        )),
        right: Amplitude(smooth(
            state.current.levels.right.get(),
            instant.levels.right.get(),
        )),
    };

    state.current = StereoReading {
        correlation,
        balance,
        levels,
    };
    state.current
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pair two equal-length planes, panicking if they disagree.
    ///
    /// Test-only: a test that hands over mismatched planes has a bug in the
    /// test, and saying so loudly beats threading an `Option` through every
    /// assertion. Production callers use `StereoPlanes::new` and handle `None`.
    fn pair<'a>(l: &'a [f32], r: &'a [f32]) -> StereoPlanes<'a> {
        StereoPlanes::new(l, r).expect("test planes must be the same length")
    }

    fn tone(n: usize, scale: f32) -> Vec<f32> {
        (0..n).map(|i| (i as f32 / 20.0).sin() * scale).collect()
    }

    #[test]
    fn identical_channels_are_mono() {
        let signal = tone(1000, 1.0);
        let reading = correlate(pair(&signal, &signal));

        assert!((reading.correlation.get() - 1.0).abs() < 1e-4);
        assert!(reading.is_mono());
        assert!(!reading.has_phase_issues());
        assert_eq!(reading.width(), StereoWidth::MONO);
        assert!(reading.levels.side.get() < 1e-4, "no side content");
    }

    #[test]
    fn inverted_channels_cancel() {
        let signal = tone(1000, 1.0);
        let inverted: Vec<f32> = signal.iter().map(|s| -s).collect();
        let reading = correlate(pair(&signal, &inverted));

        assert!((reading.correlation.get() + 1.0).abs() < 1e-4);
        assert!(reading.has_phase_issues());
        assert!(reading.levels.mid.get() < 1e-4, "mid cancels to silence");
    }

    #[test]
    fn balance_follows_the_louder_channel() {
        let signal = tone(1000, 1.0);
        let silence = vec![0.0f32; 1000];

        assert!(correlate(pair(&signal, &silence)).balance < Pan(-0.9));
        assert!(correlate(pair(&silence, &signal)).balance > Pan(0.9));
        assert_eq!(correlate(pair(&silence, &silence)).balance, Pan::CENTER);
    }

    /// The invariant the old meter could break: width is derived, so it cannot
    /// drift from the correlation it is defined from — even after smoothing.
    #[test]
    fn width_cannot_contradict_correlation_even_when_smoothed() {
        let cfg = Ballistics::standard();
        let mut state = BallisticsState::new();
        let mono = tone(512, 1.0);
        let inverted: Vec<f32> = mono.iter().map(|s| -s).collect();

        // Flip from correlated to anti-correlated: the case that desynced the
        // old stored pair.
        let smoothed = step_ballistics(
            &cfg,
            &mut state,
            correlate(pair(&mono, &mono)),
            Seconds(0.01),
        );
        assert_eq!(smoothed.width(), smoothed.correlation.to_stereo_width());

        let smoothed = step_ballistics(
            &cfg,
            &mut state,
            correlate(pair(&mono, &inverted)),
            Seconds(0.01),
        );
        assert_eq!(smoothed.width(), smoothed.correlation.to_stereo_width());
    }

    /// The smoothed value is what comes back, not the instantaneous one.
    #[test]
    fn the_return_value_is_the_smoothed_reading() {
        let cfg = Ballistics::new(Seconds(1.0), Seconds(1.0));
        let mut state = BallisticsState::new();
        let mono = tone(512, 1.0);

        let instant = correlate(pair(&mono, &mono));
        let smoothed = step_ballistics(&cfg, &mut state, instant, Seconds(0.001));

        assert_eq!(smoothed, state.current());
        assert!(
            smoothed.correlation < instant.correlation,
            "a 1 s attack should lag a step input"
        );
    }

    /// Ballistics that actually differ, unlike the dead `set_smoothing` field.
    #[test]
    fn attack_and_release_have_distinct_effects() {
        let mono = tone(512, 1.0);
        let inverted: Vec<f32> = mono.iter().map(|s| -s).collect();

        let mut fast = BallisticsState::new();
        let mut slow = BallisticsState::new();
        let fast_cfg = Ballistics::new(Seconds(0.001), Seconds(0.001));
        let slow_cfg = Ballistics::new(Seconds(1.0), Seconds(1.0));

        let target = correlate(pair(&mono, &mono));
        let fast_out = step_ballistics(&fast_cfg, &mut fast, target, Seconds(0.01));
        let slow_out = step_ballistics(&slow_cfg, &mut slow, target, Seconds(0.01));

        assert!(
            fast_out.correlation > slow_out.correlation,
            "fast ballistics must approach the target sooner"
        );

        // And the release path differs from the attack path.
        let falling = correlate(pair(&mono, &inverted));
        let asymmetric = Ballistics::new(Seconds(0.001), Seconds(1.0));
        let mut state = BallisticsState::new();
        step_ballistics(&asymmetric, &mut state, target, Seconds(0.01));
        let before = state.current().correlation;
        step_ballistics(&asymmetric, &mut state, falling, Seconds(0.01));
        let after = state.current().correlation;
        assert!(
            (before.get() - after.get()).abs() < 0.05,
            "a 1 s release should barely move in 10 ms"
        );
    }

    #[test]
    fn ms_ratio_is_finite_at_both_extremes() {
        let mono = tone(1000, 1.0);
        let silence = vec![0.0f32; 1000];

        // All mid, no side.
        let all_mid = correlate(pair(&mono, &mono)).levels.ms_ratio();
        assert!(all_mid.get().is_finite() && all_mid.get() > 0.0);

        // All side, no mid.
        let inverted: Vec<f32> = mono.iter().map(|s| -s).collect();
        let all_side = correlate(pair(&mono, &inverted)).levels.ms_ratio();
        assert!(all_side.get().is_finite() && all_side.get() < 0.0);

        // Silence is neither.
        assert_eq!(
            correlate(pair(&silence, &silence)).levels.ms_ratio(),
            Db(0.0)
        );
    }

    #[test]
    fn an_empty_block_reads_as_the_default() {
        assert_eq!(correlate(pair(&[], &[])), StereoReading::default());
    }

    /// Ragged planes are **unrepresentable** rather than silently truncated.
    ///
    /// Reconciling a mismatch with `left.len().min(right.len())` makes a
    /// three-frame left against a one-frame right report a correlation of 1.0
    /// from a single frame, with the caller never learning that two thirds of
    /// its left channel went unmeasured. `StereoPlanes` refuses the pairing
    /// instead, which is what this test pins.
    #[test]
    fn ragged_planes_cannot_be_paired() {
        assert!(
            StereoPlanes::new(&[1.0, 1.0, 1.0], &[1.0]).is_none(),
            "a mismatched pair must be rejected, not silently truncated to the shorter"
        );
    }

    #[test]
    fn independent_noise_is_near_zero() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE);
        let left: Vec<f32> = (0..20000).map(|_| rng.gen_range(-1.0..1.0)).collect();
        let right: Vec<f32> = (0..20000).map(|_| rng.gen_range(-1.0..1.0)).collect();

        let reading = correlate(pair(&left, &right));
        assert!(reading.correlation.get().abs() < 0.05);
        assert!((reading.width().get() - 1.0).abs() < 0.05);
    }
}
