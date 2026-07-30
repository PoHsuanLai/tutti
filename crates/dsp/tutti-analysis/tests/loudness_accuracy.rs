//! Value-based tests for EBU R128 loudness metering.
//!
//! `loudness.rs`'s own tests cover the plumbing — streaming folds to one-shot, a
//! ragged chunk is handled, the configured rate reaches the meter — but not the
//! *numbers*. Nothing asserts that a signal of known level reads the LUFS the
//! spec says it should, and nothing pins true peak to a calculable value.
//!
//! R128 is a published standard, so unlike pitch these have absolute answers
//! that can be derived on paper. That is what this file checks. The
//! cross-implementation half (against `pyloudnorm`, the reference Python
//! implementation) lives in `examples/verify_analysis.py`.
//!
//! Confirmed to fail against a meter with the sample rate hardcoded to 48 kHz —
//! the defect `LoudnessConfig` was introduced to make unrepresentable.

use tutti_analysis::{
    loudness::{measure_loudness, LoudnessConfig},
    ChannelLayout,
};
use tutti_core::SampleRate;
use tutti_types::{Db, Interleaved};

/// Interleaved stereo sine, both channels identical.
fn stereo_sine(rate: f64, freq: f64, secs: f64, amp: f64) -> Vec<f32> {
    let n = (rate * secs) as usize;
    (0..n)
        .flat_map(|i| {
            let s = (amp * (std::f64::consts::TAU * freq * i as f64 / rate).sin()) as f32;
            [s, s]
        })
        .collect()
}

fn measure(rate: f64, amp: f64) -> tutti_analysis::loudness::Loudness {
    let cfg = LoudnessConfig::new(SampleRate(rate), ChannelLayout::Stereo);
    let buf = stereo_sine(rate, 1000.0, 3.0, amp);
    measure_loudness(&cfg, Interleaved::new(&buf, ChannelLayout::Stereo)).expect("stereo meters")
}

/// True peak of a sine is its amplitude — a value with a closed form.
///
/// Checked before anything gated, because it isolates the peak path from the
/// gating path: if this is wrong, an LUFS discrepancy has two possible causes
/// rather than one.
#[test]
fn true_peak_is_the_signals_amplitude() {
    for &amp in &[1.0f64, 0.5, 0.25, 0.1, 0.01] {
        let got = measure(48_000.0, amp).true_peak.get();
        let want = 20.0 * amp.log10();
        assert!(
            (got as f64 - want).abs() < 0.3,
            "amplitude {amp} should peak at {want:.3} dBTP, got {got:.3} — \
             true peak is 4x oversampled so a small overshoot is expected, \
             0.3 dB is not"
        );
    }
}

/// A 1 kHz stereo sine at 0.5 amplitude reads about -6 LUFS.
///
/// Derivable on paper, which is why it is asserted as a number rather than
/// against another implementation: a sine of amplitude `a` has RMS `a/sqrt(2)`,
/// so 0.5 gives -9.03 dBFS. R128 sums the two channels (+3.01 dB), and the
/// K-weighting curve is ~0 dB at 1 kHz by construction. -9.03 + 3.01 = -6.02.
///
/// The 0.5 LU tolerance is what R128 itself allows between conforming meters.
#[test]
fn a_known_signal_reads_the_loudness_the_spec_says() {
    for &(amp, want) in &[
        (1.0f64, 0.0f64),
        (0.5, -6.02),
        (0.25, -12.04),
        (0.1, -20.0),
        (0.01, -40.0),
    ] {
        let got = measure(48_000.0, amp).lufs.get();
        assert!(
            (got as f64 - want).abs() < 0.5,
            "a 1 kHz stereo sine at amplitude {amp} should read {want:.2} LUFS, \
             got {got:.3} — beyond the 0.5 LU R128 allows between meters"
        );
    }
}

/// Loudness must scale linearly with amplitude, in dB.
///
/// Independent of absolute calibration: a meter reading uniformly 3 dB high
/// still passes this, and a meter that is correct at one level but compresses
/// elsewhere fails it. Together with the absolute test above, the two pin both
/// the offset and the slope.
#[test]
fn loudness_tracks_amplitude_in_db() {
    for &(hi, lo) in &[(1.0f64, 0.5f64), (0.5, 0.25), (0.1, 0.01)] {
        let delta = measure(48_000.0, hi).lufs.get() - measure(48_000.0, lo).lufs.get();
        let want = 20.0 * (hi / lo).log10();
        assert!(
            (delta as f64 - want).abs() < 0.15,
            "going from amplitude {hi} to {lo} should drop {want:.3} dB, \
             got {delta:.3}"
        );
    }
}

/// The same musical signal at different rates must read the same loudness.
///
/// The defect this guards against actually shipped: two true-peak sites
/// hardcoded 48 kHz while a sibling threaded the real rate, so a 44.1 kHz render
/// was metered through a filter built for the wrong rate and normalised to a
/// biased target. `LoudnessConfig` carries the rate to make that
/// unrepresentable — this asserts the carrying works.
///
/// Note this is a *different* claim from `the_configured_rate_is_used` in the
/// crate's own tests. That one feeds identical *samples* at two rates and checks
/// the readings differ (they should — it is a different signal). This feeds the
/// same *signal*, resynthesised per rate, and checks the readings agree.
#[test]
fn loudness_is_independent_of_sample_rate() {
    let readings: Vec<(f64, f32, f32)> = [44_100.0f64, 48_000.0, 96_000.0]
        .iter()
        .map(|&rate| {
            let l = measure(rate, 0.5);
            (rate, l.lufs.get(), l.true_peak.get())
        })
        .collect();

    let lufs: Vec<f32> = readings.iter().map(|r| r.1).collect();
    let spread = lufs.iter().cloned().fold(f32::MIN, f32::max)
        - lufs.iter().cloned().fold(f32::MAX, f32::min);
    assert!(
        spread < 0.2,
        "the same 1 kHz tone reads {spread:.3} LU differently across sample \
         rates: {readings:?}"
    );

    let peaks: Vec<f32> = readings.iter().map(|r| r.2).collect();
    let peak_spread = peaks.iter().cloned().fold(f32::MIN, f32::max)
        - peaks.iter().cloned().fold(f32::MAX, f32::min);
    assert!(
        peak_spread < 0.3,
        "true peak varies by {peak_spread:.3} dB across sample rates: \
         {readings:?} — this is the hardcoded-48k defect's signature"
    );
}

/// A steady tone has no loudness *range*.
///
/// LRA measures variation over time; a signal that never changes has none. A
/// non-zero reading would mean the gating blocks see something moving that is
/// not.
#[test]
fn a_steady_tone_has_no_loudness_range() {
    for &amp in &[0.5f64, 0.1] {
        let range = measure(48_000.0, amp).range.get();
        assert!(
            range.abs() < 0.5,
            "a constant 1 kHz tone at {amp} reported {range:.3} LU of range"
        );
    }
}

/// Silence must be finite, not `-inf`.
///
/// The meter reports `-inf` for a signal that never passes the absolute gate.
/// An infinite loudness poisons every gain derived from it into `NaN`, so the
/// module clamps to R128's -70 LUFS gate; this pins that it does.
#[test]
fn silence_reads_the_gate_not_negative_infinity() {
    let cfg = LoudnessConfig::new(SampleRate(48_000.0), ChannelLayout::Stereo);
    let silence = vec![0.0f32; 48_000 * 2];
    let l = measure_loudness(&cfg, Interleaved::new(&silence, ChannelLayout::Stereo))
        .expect("stereo meters");

    assert!(
        l.lufs.get().is_finite(),
        "silence reported {} LUFS — an infinite reading turns any derived gain \
         into NaN",
        l.lufs.get()
    );
    assert!(
        (l.lufs.get() - (-70.0)).abs() < 0.01,
        "silence should clamp to R128's -70 LUFS absolute gate, got {}",
        l.lufs.get()
    );
    assert_eq!(
        l.true_peak,
        Db::FLOOR,
        "silence should peak at the shared silence floor"
    );
}

/// `gain_to` must respect the ceiling, and only pull down.
///
/// The whole of "normalise" is this one function. Two behaviours matter: a
/// signal that would clip after the loudness-derived gain gets pulled back by
/// exactly the overshoot, and a signal comfortably under the ceiling keeps its
/// full gain rather than being pushed up to meet it.
#[test]
fn gain_to_honours_the_ceiling_without_pushing_up() {
    // Quiet signal, generous ceiling: the gain is purely loudness-driven.
    let quiet = measure(48_000.0, 0.01); // ~-40 LUFS, -40 dBTP
    let g = quiet.gain_to(Db(-23.0), Db(-1.0));
    let want = -23.0 - quiet.lufs.get();
    assert!(
        (g.get() - want).abs() < 0.01,
        "a signal peaking at {} dBTP has {} dB of headroom under a -1 dBTP \
         ceiling, so the gain should be the full {want:.3} dB, got {}",
        quiet.true_peak.get(),
        -1.0 - quiet.true_peak.get(),
        g.get()
    );

    // Loud signal, tight ceiling: the gain must be limited by the peak.
    let loud = measure(48_000.0, 1.0); // ~0 LUFS, 0 dBTP
    let g = loud.gain_to(Db(6.0), Db(-1.0));
    let projected = loud.true_peak.get() + g.get();
    assert!(
        projected <= -1.0 + 0.01,
        "asking for +6 LUFS on a signal already at {} dBTP must be limited by \
         the -1 dBTP ceiling, but the projected peak is {projected}",
        loud.true_peak.get()
    );
}
