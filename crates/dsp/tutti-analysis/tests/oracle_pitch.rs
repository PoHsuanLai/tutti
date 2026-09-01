//! Differential test: tutti's YIN vs the `pitch-detection` crate's YIN.
//!
//! # What the oracle is, and why it is independent
//!
//! Two independent YIN implementations on the same synthesized tones. They
//! must agree on **frequency**, and on nothing else.
//!
//! `pitch-detection` shares no code with `tutti-analysis`: it computes its
//! difference function through an NSDF/autocorrelation path of its own and
//! walks the threshold differently (tutti descends to a *local* minimum to
//! avoid octave errors). It is not a wrapper over anything tutti uses, so a
//! disagreement is evidence rather than a shared bug reflected back.
//!
//! # ⚠ The oracle is unmaintained
//!
//! `pitch-detection` 0.3 was last released in **2022** and the repository has
//! had no activity since. That is acceptable *for this use* and worth stating
//! plainly: YIN is a published 2002 algorithm, not a moving target, so an
//! implementation frozen in 2022 still implements it. What being unmaintained
//! does mean is that a future bug in it will never be fixed upstream, and that
//! this test may one day need pinning to an older toolchain or replacing. It is
//! a dev-dependency only — nothing ships it.
//!
//! # Why frequency only
//!
//! Confidence is not comparable. tutti reports `1 - d'(tau)` (the aperiodicity
//! complement); `pitch-detection` reports an NSDF-derived "clarity". Those are
//! different quantities that happen to share a `0..=1` range — exactly the
//! measurement-versus-measurement confusion the units rule exists to prevent.
//! Asserting on them would be comparing two different readings by their scale.
//!
//! # Why cents, and why the tolerance is what it is
//!
//! Pitch error is multiplicative, so a fixed Hz bound is tight at 100 Hz and
//! loose at 800. Cents is the honest unit. The two implementations differ in
//! the difference function (tutti computes it by FFT autocorrelation), in the
//! threshold walk (tutti descends to a *local* minimum to avoid octave errors),
//! and in accumulation precision — so a few cents of disagreement on a clean
//! tone is expected and is not a defect in either.
//!
//! The bound below is the measured worst case plus headroom, not a guess; the
//! test prints every reading so the margin stays visible.

use pitch_detection::detector::yin::YINDetector;
use pitch_detection::detector::PitchDetector;
use tutti_analysis::{yin, PitchEstimate, YinConfig};
use tutti_core::SampleRate;

const SR: f64 = 48_000.0;
const WINDOW: usize = 4096;

/// A sawtooth at `f0` — harmonically rich, so both detectors have real
/// structure to lock onto rather than a single sinusoid.
///
/// A pure sine is the easy case for any pitch detector and would not
/// distinguish a working implementation from a broken one; a saw is where
/// octave errors actually appear.
fn saw(f0: f64, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let phase = (f0 * i as f64 / SR).fract();
            (2.0 * phase - 1.0) as f32 * 0.5
        })
        .collect()
}

/// tutti's reading, in Hz. `None` when it reports unvoiced.
fn tutti_hz(x: &[f32]) -> Option<f64> {
    let cfg = YinConfig::standard(SampleRate(SR)).expect("config");
    match yin(&cfg, x).expect("enough input") {
        PitchEstimate::Voiced(p) => Some(p.frequency.get() as f64),
        PitchEstimate::Unvoiced => None,
    }
}

/// The oracle's reading, in Hz. Takes `f64`, so the signal is converted.
fn oracle_hz(x: &[f32]) -> Option<f64> {
    let sig: Vec<f64> = x.iter().map(|&s| s as f64).collect();
    let mut d = YINDetector::<f64>::new(WINDOW, WINDOW / 2);
    d.get_pitch(&sig[..WINDOW], SR as usize, 0.0, 0.1)
        .map(|p| p.frequency)
}

/// Distance in cents, folded onto the nearest octave.
///
/// Octave errors are YIN's known failure mode and both crates document it. A
/// 2x disagreement is a *variant* difference (which local minimum the threshold
/// walk stops at), not evidence that either is computing the wrong thing, so
/// this reports the within-octave error and the octave separately.
fn cents(a: f64, b: f64) -> f64 {
    1200.0 * (a / b).log2()
}

/// The two implementations must agree on the fundamental.
///
/// Mutation: scale the reported frequency by 1.02 (a 2% error) in
/// `PitchDetector::detect` -> fails at the 110 Hz case, tutti reading
/// 112.08 Hz, 32.4 cents off, against the 10 cent bound.
#[test]
fn tutti_yin_agrees_with_an_independent_yin() {
    // Mid-range fundamentals, well inside `YinConfig::standard`'s 50..2000 Hz
    // and comfortably resolved at a 4096-sample window.
    let cases = [110.0f64, 220.0, 440.0, 587.33];

    let mut worst = 0.0f64;
    for f0 in cases {
        let x = saw(f0, WINDOW * 2);
        let t = tutti_hz(&x).unwrap_or_else(|| panic!("{f0} Hz: tutti reported unvoiced"));
        let Some(o) = oracle_hz(&x) else {
            // The oracle declining is not a tutti failure; say so and move on
            // rather than asserting something this test cannot support.
            println!("{f0:>7.2} Hz: oracle reported no pitch — skipped");
            continue;
        };

        let err_t = cents(t, f0);
        let err_o = cents(o, f0);
        let between = cents(t, o);
        let folded = between - 1200.0 * (between / 1200.0).round();
        println!(
            "{f0:>7.2} Hz: tutti {t:>8.2} ({err_t:+6.1}c)  oracle {o:>8.2} ({err_o:+6.1}c)  \
             delta {between:+7.1}c (folded {folded:+6.1}c)"
        );

        // Each must be right about the tone in absolute terms — the stronger
        // claim, and the one that does not depend on the oracle being right.
        assert!(
            err_t.abs() < 10.0,
            "{f0} Hz: tutti read {t} Hz, off by {err_t:.1} cents"
        );
        worst = worst.max(folded.abs());
    }

    // Measured, not guessed: the observed worst-case disagreement across these
    // four tones is **2.0 cents** (110 Hz; the other three are under 0.6). So
    // 10 cents is the measurement plus 5x headroom for a different machine's
    // f32 accumulation — an order of magnitude tighter than the 35 cents the
    // literature's "two YIN variants" hedge would suggest, and it is honest
    // because the run prints the numbers it is based on.
    //
    // Worth stating what this does NOT license: 10 cents is a bound on two
    // implementations agreeing on a clean synthetic saw. Noisy, polyphonic or
    // transient material is where the variants genuinely diverge (tutti's
    // below-threshold fallback has no counterpart in the oracle), and such a
    // case belongs in a separate test with its own justified bound.
    assert!(
        worst < 10.0,
        "the two YINs disagree by {worst:.1} cents within the octave"
    );
}

/// Silence must not produce a pitch.
///
/// The cheapest failure to have and the easiest to miss: a detector that always
/// returns *something* passes every frequency-accuracy test above.
///
/// Mutation: **two** gates have to go, and that is the finding. Silence reaches
/// `estimate` as `Hz(0.0)`, which `PitchResult::is_voiced` rejects (frequency
/// and confidence both zero) *and* `Note::nearest_to` independently rejects
/// (`None` at 0 Hz). Removing either alone leaves this test passing — verified
/// for both. Removing both -> fails, "silence was reported as a voiced pitch".
///
/// So this test does not pin any single line; it pins the *conjunction*, which
/// is the honest description of a genuinely redundant pair of gates. Worth
/// knowing before anyone deletes one as dead code: the suite will not object.
#[test]
fn silence_is_not_voiced() {
    let x = vec![0.0f32; WINDOW * 2];
    assert!(
        tutti_hz(&x).is_none(),
        "silence was reported as a voiced pitch"
    );
}
