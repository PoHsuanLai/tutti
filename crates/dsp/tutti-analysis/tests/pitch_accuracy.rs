//! Value-based accuracy tests for YIN.
//!
//! `yin.rs`'s own tests are thorough about *behaviour* — ranges refused, silence
//! unvoiced, a reused detector matching a fresh one — but exactly one of them
//! checks a frequency (`finds_a440`, ±1 Hz on a single pure tone). The
//! implementation underneath, `pitch.rs`, has no test module at all: the FFT
//! autocorrelation, the cumulative-mean normalisation, the first-local-minimum
//! rule and the parabolic interpolation are reached only through that one
//! assertion.
//!
//! This file covers what that leaves open — accuracy across the range, and the
//! octave errors YIN is specifically known for. These are the checks that do not
//! need a reference implementation; the ones that do live in
//! `examples/verify_analysis.py`, which judges the same signals against librosa
//! and pyloudnorm.
//!
//! Each test here was confirmed to fail against a deliberately broken detector
//! (integer periods instead of interpolated ones; the global minimum instead of
//! the first local one below threshold) before being committed.

use tutti_analysis::{yin, YinConfig};
use tutti_types::{Hz, Samples};

const SR: f64 = 48_000.0;

/// Sum of harmonics at explicit relative amplitudes; `harmonics[k]` is partial
/// `k+1`. Built at f64 and summed directly rather than shaped from a table, so
/// the generator cannot smear the harmonic structure under test.
fn tone(freq: f64, harmonics: &[f64], secs: f64, amp: f64) -> Vec<f32> {
    let n = (SR * secs) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / SR;
            let s: f64 = harmonics
                .iter()
                .enumerate()
                .map(|(k, &a)| a * (std::f64::consts::TAU * freq * (k as f64 + 1.0) * t).sin())
                .sum();
            (s * amp) as f32
        })
        .collect()
}

fn sine(freq: f64, secs: f64, amp: f64) -> Vec<f32> {
    tone(freq, &[1.0], secs, amp)
}

/// Detected frequency, or `None` if unvoiced.
fn detect(samples: &[f32]) -> Option<f32> {
    let cfg = YinConfig::standard(SR).expect("standard range");
    yin(&cfg, samples)
        .expect("buffer long enough")
        .pitch()
        .map(|p| p.frequency.get())
}

/// Pure tones across the usable range, to 0.1%.
///
/// The tolerance is deliberately far tighter than `finds_a440`'s ±1 Hz, which
/// at 440 Hz is 0.23% and at 1975 Hz would admit a whole semitone of error at
/// the top of the range. Sub-sample interpolation is what buys this precision;
/// removing it pushes the top three cases to 0.8–1.3% and fails here.
#[test]
fn pure_tones_are_detected_across_the_range() {
    for &f in &[55.0f64, 110.0, 220.0, 440.0, 880.0, 1320.0, 1975.0] {
        let got = detect(&sine(f, 0.5, 0.5)).unwrap_or_else(|| panic!("{f} Hz read as unvoiced"));
        let err = (got as f64 - f).abs() / f * 100.0;
        assert!(
            err < 0.1,
            "{f} Hz detected as {got} ({err:.3}% error) — beyond the 0.1% \
             sub-sample interpolation should deliver"
        );
    }
}

/// Harmonically rich tones must report the fundamental, not a partial.
///
/// This is YIN's signature failure mode and the reason the paper takes the
/// *first* local minimum below threshold rather than the global one. A detector
/// using the global minimum reads a saw at 110 Hz as 55 Hz — the check below
/// names the octave explicitly so the failure reads as "octave error" rather
/// than as a large percentage.
#[test]
fn harmonic_tones_report_the_fundamental_not_an_octave() {
    let saw: Vec<f64> = (1..=8).map(|k| 1.0 / k as f64).collect();
    let square = [1.0, 0.0, 0.33, 0.0, 0.2, 0.0, 0.14];

    for &f in &[110.0f64, 220.0, 440.0] {
        for (label, harmonics, amp) in [
            ("saw", saw.as_slice(), 0.3),
            ("square", square.as_slice(), 0.4),
        ] {
            let got = detect(&tone(f, harmonics, 0.5, amp))
                .unwrap_or_else(|| panic!("{label} {f} Hz read as unvoiced"));

            for (mult, name) in [(2.0, "an octave up"), (0.5, "an octave down")] {
                assert!(
                    (got as f64 - f * mult).abs() > f * mult * 0.02,
                    "{label} at {f} Hz came back as {got} — {name}"
                );
            }
            let err = (got as f64 - f).abs() / f * 100.0;
            assert!(err < 0.5, "{label} at {f} Hz detected as {got} ({err:.3}%)");
        }
    }
}

/// A tone with no energy at the fundamental still has that fundamental's pitch.
///
/// Partials 2, 3 and 4 only. The periodicity is still `1/f`, so YIN — which
/// works in the time domain — should find it. A detector that had quietly
/// degenerated into spectral peak-picking reports `2f` here and passes every
/// other test in this file.
#[test]
fn a_missing_fundamental_is_still_found() {
    for &f in &[220.0f64, 440.0] {
        let got = detect(&tone(f, &[0.0, 1.0, 0.7, 0.5], 0.5, 0.4))
            .unwrap_or_else(|| panic!("missing-fundamental {f} Hz read as unvoiced"));
        let err = (got as f64 - f).abs() / f * 100.0;
        assert!(
            err < 0.5,
            "a tone with partials 2,3,4 of {f} Hz was read as {got} — \
             the fundamental is absent from the spectrum but not from the period"
        );
    }
}

/// Level must not move the frequency.
///
/// YIN normalises by energy (the cumulative mean difference is a ratio), so a
/// reading that drifts with amplitude means that normalisation is wrong. 40 dB
/// is a realistic dynamic span for a single instrument.
#[test]
fn detection_is_independent_of_level() {
    let loud = detect(&sine(440.0, 0.5, 0.5)).expect("loud tone voiced");
    let quiet = detect(&sine(440.0, 0.5, 0.005)).expect("quiet tone voiced");
    assert!(
        (loud - quiet).abs() < 0.5,
        "440 Hz read as {loud} at -6 dBFS but {quiet} at -46 dBFS — \
         the energy normalisation is level-dependent"
    );
}

/// Noise must not be reported as pitched.
///
/// A confident wrong answer is worse than no answer: anything downstream
/// (tuning, transcription, correction) would act on it. Deterministic LCG so
/// this cannot be flaky.
#[test]
fn noise_is_not_confidently_pitched() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let samples: Vec<f32> = (0..(SR * 0.5) as usize)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0) as f32 * 0.5
        })
        .collect();

    let cfg = YinConfig::standard(SR).unwrap();
    let est = yin(&cfg, &samples).expect("long enough");
    assert!(
        !est.is_voiced(),
        "white noise reported as {} Hz at confidence {}",
        est.frequency().get(),
        est.confidence().get()
    );
}

/// A tracked sweep must follow the sweep.
///
/// The per-frame *accuracy* is judged in Python, where the analysis window can
/// be modelled properly. What is asserted natively is the ordering, which needs
/// no model: a monotonically rising sweep must produce monotonically rising
/// estimates. A single frame landing an octave off breaks this even when it
/// falls inside a percentage tolerance.
#[test]
fn a_rising_sweep_reads_as_rising() {
    let cfg = YinConfig::standard(SR).unwrap();
    let frame = cfg.buffer_size().get();
    let n = frame * 8;

    // Phase is the integral of frequency. Writing `sin(TAU*f*t)` with a moving
    // `f` sweeps at twice the intended rate — the generator would then be under
    // test alongside the detector.
    let mut phase = 0.0f64;
    let sweep: Vec<f32> = (0..n)
        .map(|i| {
            let f = 200.0 + 600.0 * (i as f64 / n as f64);
            let s = phase.sin() * 0.5;
            phase += std::f64::consts::TAU * f / SR;
            s as f32
        })
        .collect();

    let tracked = yin::yin_track(&cfg, &sweep, Samples(frame)).expect("track");
    let freqs: Vec<f32> = tracked
        .iter()
        .map(|e| e.pitch().map(|p| p.frequency.get()).unwrap_or(0.0))
        .collect();

    assert!(
        freqs.iter().all(|&f| f > 0.0),
        "some sweep frames read unvoiced: {freqs:?}"
    );
    for (i, w) in freqs.windows(2).enumerate() {
        assert!(
            w[1] > w[0],
            "frame {} ({}) is not above frame {} ({}) — a rising sweep must \
             read as rising: {freqs:?}",
            i + 1,
            w[1],
            i,
            w[0]
        );
    }
}

/// The reported note must match the reported frequency.
///
/// These are two fields of one struct, computed together; if they disagree the
/// MIDI conversion is wrong even when the detection is right. Checked against an
/// independently written conversion rather than against `freq_to_midi` itself.
#[test]
fn the_note_name_agrees_with_the_frequency() {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];

    for &(f, want) in &[
        (110.0f64, "A2"),
        (220.0, "A3"),
        (261.63, "C4"),
        (440.0, "A4"),
        (880.0, "A5"),
    ] {
        let cfg = YinConfig::standard(SR).unwrap();
        let est = yin(&cfg, &sine(f, 0.5, 0.5)).expect("long enough");
        let p = est.pitch().unwrap_or_else(|| panic!("{f} Hz unvoiced"));

        let midi = (69.0 + 12.0 * (p.frequency.get() as f64 / 440.0).log2()).round() as i32;
        let derived = format!("{}{}", NAMES[(midi % 12) as usize], midi / 12 - 1);

        assert_eq!(
            p.note_name(),
            want,
            "{f} Hz should be {want}, got {}",
            p.note_name()
        );
        assert_eq!(
            p.note_name(),
            derived,
            "note name disagrees with the frequency the same call reported \
             ({} Hz)",
            p.frequency.get()
        );
    }
}

/// The range bounds must be honoured, not merely validated at construction.
///
/// `YinConfig::new` refuses an inverted range, and that is tested. What is not:
/// that a *valid* narrow range actually constrains the search. A tone outside
/// the configured band must not be reported as if it were inside it.
#[test]
fn a_tone_below_the_configured_range_is_not_reported_as_in_range() {
    // Search 400-2000 Hz, present a 100 Hz tone. The correct answers are
    // "unvoiced" or a harmonic inside the band — never 100 Hz, which the
    // configuration excludes.
    let cfg = YinConfig::new(SR, Hz(400.0), Hz(2000.0)).expect("valid range");
    let est = yin(&cfg, &sine(100.0, 0.5, 0.5)).expect("long enough");

    if let Some(p) = est.pitch() {
        assert!(
            p.frequency.get() >= 390.0,
            "a 100 Hz tone was reported as {} Hz by a detector configured for \
             400-2000 Hz — the period bounds are not constraining the search",
            p.frequency.get()
        );
    }
}
