//! Inter-channel phase correlation.
//!
//! The measurement a mastering engineer watches to catch mono-compatibility
//! problems: `+1` is identical channels, `0` uncorrelated, `-1` polarity
//! inverted and cancelling.
//!
//! Split into a pure kernel and an explicit smoother, which fixes two defects
//! the old meter had. Its `set_smoothing` stored a field nothing read, and its
//! `width` was a stored second field smoothed independently of the correlation
//! it is defined from — so the smoothed pair could contradict the invariant the
//! kernel guarantees.

use tutti_types::{Amplitude, Correlation, Db, Pan, Seconds, StereoPlanes, StereoWidth};

/// Mid/side and per-channel levels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StereoLevels {
    pub mid: Amplitude,
    pub side: Amplitude,
    pub left: Amplitude,
    pub right: Amplitude,
}

impl StereoLevels {
    /// Mid-to-side ratio in dB. Positive is more mid — a narrower image.
    ///
    /// Clamped, never infinite. The old version returned `f32::INFINITY` from
    /// a function whose name promises dB, and consumers propagated it into
    /// meters.
    pub fn ms_ratio(&self) -> Db {
        const FLOOR: f32 = -60.0;
        const CEILING: f32 = 60.0;

        if self.mid.get() <= 0.0 && self.side.get() <= 0.0 {
            return Db(0.0);
        }
        if self.side.get() <= 0.0 {
            return Db(CEILING);
        }
        if self.mid.get() <= 0.0 {
            return Db(FLOOR);
        }
        Db((20.0 * (self.mid.get() / self.side.get()).log10()).clamp(FLOOR, CEILING))
    }
}

/// One correlation reading.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StereoReading {
    pub correlation: Correlation,
    /// Where the energy sits on the left/right axis.
    pub balance: Pan,
    pub levels: StereoLevels,
}

impl StereoReading {
    /// The image width this correlation implies.
    ///
    /// Derived, not stored. The old struct kept it as a field that the
    /// smoother could move independently, letting `current()` return a pair
    /// violating `width == 1 - correlation`.
    #[inline]
    pub fn width(&self) -> StereoWidth {
        self.correlation.to_stereo_width()
    }

    #[inline]
    pub fn has_phase_issues(&self) -> bool {
        self.correlation.has_phase_issues()
    }

    #[inline]
    pub fn is_mono(&self) -> bool {
        self.correlation > Correlation(0.95)
    }
}

/// Correlate one block of stereo audio. Stateless.
///
/// Takes a [`StereoPlanes`] rather than two loose slices: mid/side and L/R
/// correlation are only defined at exactly two channels, and every statistic
/// below divides by a single frame count. The pair used to be reconciled here
/// with `left.len().min(right.len())`, which silently measured the shorter of
/// two mismatched blocks; the pairing now cannot be formed unless they agree.
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
    /// engine. The old pair took milliseconds and were two adjacent `f32`s, so
    /// transposing them compiled and made the meter sluggish to peaks and
    /// instant to release — under-reporting exactly the problems it exists to
    /// show.
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

    fn coefficient(&self, rising: bool, dt: Seconds) -> f32 {
        let time = if rising { self.attack } else { self.release };
        if time.get() <= 0.0 {
            return 1.0;
        }
        1.0 - (-dt.get() / time.get()).exp()
    }
}

/// The smoothed reading carried between blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct BallisticsState {
    current: StereoReading,
}

impl BallisticsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest smoothed reading.
    #[inline]
    pub fn current(&self) -> StereoReading {
        self.current
    }

    pub fn reset(&mut self) {
        self.current = StereoReading::default();
    }
}

/// Smooth an instantaneous reading and return the result.
///
/// Returns the *smoothed* value, which is the useful one. The old `process`
/// returned the instantaneous reading and hid the smoothed one behind a
/// separate `current()` call, so the obvious use of the return value was the
/// wrong one.
///
/// Only `correlation` and the levels are smoothed; width is derived from the
/// smoothed correlation afterwards, so the two cannot disagree.
pub fn step_ballistics(
    cfg: &Ballistics,
    state: &mut BallisticsState,
    instant: StereoReading,
    dt: Seconds,
) -> StereoReading {
    let smooth = |old: f32, new: f32| {
        let coefficient = cfg.coefficient(new > old, dt);
        old + (new - old) * coefficient
    };

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
        let smoothed = step_ballistics(&cfg, &mut state, correlate(pair(&mono, &mono)), Seconds(0.01));
        assert_eq!(smoothed.width(), smoothed.correlation.to_stereo_width());

        let smoothed =
            step_ballistics(&cfg, &mut state, correlate(pair(&mono, &inverted)), Seconds(0.01));
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
        assert_eq!(correlate(pair(&silence, &silence)).levels.ms_ratio(), Db(0.0));
    }

    #[test]
    fn an_empty_block_reads_as_the_default() {
        assert_eq!(correlate(pair(&[], &[])), StereoReading::default());
    }

    /// Ragged planes are now **unrepresentable** rather than silently truncated.
    ///
    /// This used to be `empty_and_ragged_input_do_not_panic`, and it pinned that
    /// `correlate` reconciled a mismatch with `left.len().min(right.len())` — so
    /// a three-frame left against a one-frame right reported a correlation of
    /// 1.0 from a single frame, and the caller never learned that two thirds of
    /// its left channel went unmeasured. `StereoPlanes` refuses the pairing
    /// instead, which is the behaviour change this test now records.
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
