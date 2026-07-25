use tutti_core::{Amplitude, BufferRef, ChannelLayout, CompressionRatio, Db, SampleRate, Seconds};

/// Max-abs sidechain detector level for the `tick` (single-sample slice) path.
///
/// `input[0..ch]` are audio channels, `input[ch..2*ch]` sidechain channels.
/// A sidechain channel the caller didn't supply falls back to its paired audio
/// channel, so an unconnected sidechain detects the audio itself — matching
/// [`sidechain_level_buffer`] rather than reading silence.
#[inline]
pub(crate) fn sidechain_level_slice(input: &[f32], ch: usize) -> f32 {
    let mut level = 0.0f32;
    for c in 0..ch {
        let src = input.get(ch + c).copied().unwrap_or(input[c]);
        level = level.max(src.abs());
    }
    level
}

/// Max-abs sidechain detector level for one sample `i` of the `process`
/// (block) path. Same audio-fallback rule as [`sidechain_level_slice`].
#[inline]
pub(crate) fn sidechain_level_buffer(input: &BufferRef, ch: usize, i: usize) -> f32 {
    let in_channels = ChannelLayout::from(input.channels()).count() as usize;
    let mut level = 0.0f32;
    for c in 0..ch {
        let src = if ch + c < in_channels {
            input.at_f32(ch + c, i)
        } else {
            input.at_f32(c, i)
        };
        level = level.max(src.abs());
    }
    level
}

/// Amplitude as decibels, for the dynamics detectors.
///
/// Delegates to the shared converter. Note the floor moved: this used to pin
/// silence at `-96` while `tutti-export`'s copy used `-144` and a third site
/// used none. `Db::FLOOR` is `-144` (roughly the 24-bit noise floor), so a
/// detector now sees a *lower* value for true digital silence than before.
/// That is inaudible in a compressor — anything near either floor is far below
/// any usable threshold — and it removes a divergence that was never a
/// deliberate difference.
#[inline]
pub(crate) fn amplitude_to_db(amp: impl Into<Amplitude>) -> Db {
    Db::from_amplitude(amp.into())
}

#[inline]
pub(crate) fn db_to_amplitude(db: impl Into<Db>) -> Amplitude {
    db.into().to_amplitude()
}

#[inline]
pub(crate) fn time_to_coeff(time: impl Into<Seconds>, sample_rate: impl Into<SampleRate>) -> f32 {
    let time = time.into().get();
    let sample_rate = sample_rate.into().get();
    if time <= 0.0 {
        1.0
    } else {
        (-1.0 / (time * sample_rate as f32)).exp()
    }
}

/// Pure compressor gain reduction (hard/soft knee).
#[inline]
pub(crate) fn compute_compressor_gain_reduction(
    input_db: impl Into<Db>,
    threshold: impl Into<Db>,
    ratio: impl Into<CompressionRatio>,
    knee: impl Into<Db>,
) -> Db {
    let input_db = input_db.into().get();
    let threshold = threshold.into().get();
    let ratio = ratio.into().get();
    let knee = knee.into().get();
    if knee <= 0.0 {
        let over_db = (input_db - threshold).max(0.0);
        Db(over_db * (1.0 - 1.0 / ratio))
    } else {
        let half_knee = knee / 2.0;
        let below = threshold - half_knee;
        let above = threshold + half_knee;

        if input_db <= below {
            Db(0.0)
        } else if input_db >= above {
            let over_db = input_db - threshold;
            Db(over_db * (1.0 - 1.0 / ratio))
        } else {
            let x = input_db - below;
            let slope = (1.0 - 1.0 / ratio) / (2.0 * knee);
            Db(slope * x * x)
        }
    }
}

/// One-pole IIR envelope smoothing.
#[inline]
pub(crate) fn smooth_envelope(current: f32, target: f32, coeff: f32) -> f32 {
    coeff * current + (1.0 - coeff) * target
}

/// Gate gain from gate level (unitless 0..1) and range in dB.
#[inline]
pub(crate) fn compute_gate_gain(gate_level: f32, range_db: impl Into<Db>) -> Amplitude {
    let range_linear = db_to_amplitude(range_db).get();
    Amplitude(range_linear + gate_level * (1.0 - range_linear))
}

/// Pure limiter gain computation.
/// Returns the linear gain to apply given input peak level, threshold, and ceiling (all in dB).
#[inline]
pub(crate) fn compute_limiter_gain(
    peak_db: impl Into<Db>,
    threshold: impl Into<Db>,
    ceiling: impl Into<Db>,
) -> Amplitude {
    let peak_db = peak_db.into().get();
    let threshold = threshold.into().get();
    let ceiling = ceiling.into().get();
    if peak_db > threshold {
        let reduction = peak_db - threshold;
        db_to_amplitude(Db(-reduction + (ceiling - threshold).min(0.0)))
    } else {
        db_to_amplitude(Db(ceiling - threshold))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_amplitude_db_conversion() {
        assert!((amplitude_to_db(1.0).get() - 0.0).abs() < 0.001);
        assert!((amplitude_to_db(0.5).get() - (-6.02)).abs() < 0.1);
        assert!((db_to_amplitude(0.0).get() - 1.0).abs() < 0.001);
        assert!((db_to_amplitude(-6.0).get() - 0.501).abs() < 0.01);
    }

    #[test]
    fn test_compressor_gain_reduction_hard_knee() {
        // Below threshold: no reduction
        assert_eq!(
            compute_compressor_gain_reduction(-30.0, -20.0, 4.0, 0.0).get(),
            0.0
        );
        // Above threshold: ratio compression
        let gr = compute_compressor_gain_reduction(-10.0, -20.0, 4.0, 0.0).get();
        assert!((gr - 7.5).abs() < 0.01); // 10dB over * (1 - 1/4) = 7.5
    }

    #[test]
    fn test_compressor_gain_reduction_soft_knee() {
        // Well below: no reduction
        assert_eq!(
            compute_compressor_gain_reduction(-40.0, -20.0, 4.0, 6.0).get(),
            0.0
        );
        // Well above: same as hard knee
        let gr_hard = compute_compressor_gain_reduction(0.0, -20.0, 4.0, 0.0).get();
        let gr_soft = compute_compressor_gain_reduction(0.0, -20.0, 4.0, 6.0).get();
        assert!((gr_hard - gr_soft).abs() < 0.01);
        // In knee region: quadratic transition (non-zero, less than hard)
        let gr_knee = compute_compressor_gain_reduction(-20.0, -20.0, 4.0, 6.0).get();
        assert!(gr_knee > 0.0);
        assert!(gr_knee < gr_hard);
    }

    #[test]
    fn test_smooth_envelope() {
        // Attack: target > current
        let result = smooth_envelope(0.0, 1.0, 0.9);
        assert!((result - 0.1).abs() < 0.001);
        // Release: target < current
        let result = smooth_envelope(1.0, 0.0, 0.9);
        assert!((result - 0.9).abs() < 0.001);
        // Full attack (coeff=0): instant
        assert!((smooth_envelope(0.0, 1.0, 0.0) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_compute_gate_gain() {
        // Fully open (gate_level=1): gain=1.0 regardless of range
        assert!((compute_gate_gain(1.0, -80.0).get() - 1.0).abs() < 0.001);
        // Fully closed (gate_level=0): gain = range_linear
        let closed = compute_gate_gain(0.0, -80.0).get();
        assert!(closed < 0.001); // -80dB is very quiet
                                 // -12dB range, closed gate
        let closed_12 = compute_gate_gain(0.0, -12.0).get();
        assert!((closed_12 - db_to_amplitude(-12.0).get()).abs() < 0.001);
    }

    #[test]
    fn test_compute_limiter_gain() {
        // Below threshold: gain = ceiling - threshold offset
        let gain = compute_limiter_gain(-20.0, -6.0, -0.3).get();
        assert!((gain - db_to_amplitude(Db(-0.3 - (-6.0))).get()).abs() < 0.001);
        // Above threshold: reduces gain
        let gain = compute_limiter_gain(0.0, -6.0, -0.3).get();
        assert!(gain < 1.0);
    }
}
