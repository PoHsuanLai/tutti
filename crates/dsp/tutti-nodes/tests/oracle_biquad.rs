//! Differential test: SVF magnitude response vs the `biquad` crate.
//!
//! # What the oracle is, and why it is independent
//!
//! `biquad` implements the RBJ audio-EQ cookbook as a direct-form-1 biquad.
//! tutti's `SvfFilterNode` is a Cytomic/Zavalishin topology-preserving
//! state-variable filter. **These are different filters**, so this is not a
//! bit-equality test — it is a check that the two agree on the thing they both
//! claim to implement: a 12 dB/octave response at a stated cutoff and Q.
//!
//! What that buys, and what it does not: it would catch a cutoff that lands at
//! the wrong frequency, a Q that does nothing, a response that is upside-down
//! (low-pass passing highs), or a gain that is off by a constant. It would not
//! catch a subtle difference in resonant peak shape, because the topologies
//! genuinely differ there.
//!
//! # ⚠ The oracle has a bug, and this file compensates for it
//!
//! `biquad` 0.5.0's cutoff is **four times lower than the one you ask for**.
//! See [`BIQUAD_OMEGA_BUG`] for the derivation, the line numbers and the
//! compensation. Read that before concluding anything here is a tutti defect:
//! uncompensated, the two filters disagree by up to 24 dB, which reads exactly
//! like a catastrophic engine bug and is nothing of the kind.
//!
//! The compensation is itself checked, in
//! [`the_compensated_oracle_matches_the_rbj_cookbook`], against the cookbook's
//! own closed-form coefficient — so a reader need not take the factor on
//! trust, and a future `biquad` release that fixes the bug fails that test
//! rather than silently shifting every probe.
//!
//! # Tolerance rationale
//!
//! **0.01 dB**, and it is measured rather than guessed. The observed delta at
//! every one of the 15 probes (3 filter kinds × 5 frequencies) is 0.000 dB: a
//! TPT state-variable filter and a direct-form-1 RBJ biquad at the same cutoff
//! and Q are the *same* transfer function, differing only in how they store
//! state. So the honest bound is "indistinguishable", and 0.01 dB is that plus
//! room for f32 accumulation. A looser bound would be unjustified — and would
//! blunt the test, since a 1% cutoff error only moves the response 0.17 dB.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type as BqType};
use tutti_core::AudioUnit;
use tutti_nodes::{SvfFilterNode, SvfType};

const SR: f64 = 48_000.0;
const Q: f32 = 0.707;
const CUTOFF: f32 = 1_000.0;

/// Steady-state gain of `f` at `freq`, measured as an RMS ratio.
///
/// Drives a sine long enough for the transient to settle, discards the first
/// half, and compares output RMS to input RMS. Measuring rather than evaluating
/// the transfer function is deliberate: it tests the filter that actually runs,
/// including its coefficient update path, not a formula restated in the test.
fn measured_gain_db(mut render: impl FnMut(f32) -> f32, freq: f32) -> f64 {
    let n = 24_000usize; // 0.5 s at 48 kHz — plenty for a 12 dB/oct settle.
    let mut input = Vec::with_capacity(n);
    let mut output = Vec::with_capacity(n);
    for i in 0..n {
        let x = (std::f64::consts::TAU * freq as f64 * i as f64 / SR).sin() as f32;
        input.push(x);
        output.push(render(x));
    }
    // Second half only: the first half carries the startup transient.
    let rms = |v: &[f32]| -> f64 {
        let s = &v[v.len() / 2..];
        (s.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / s.len() as f64).sqrt()
    };
    20.0 * (rms(&output) / rms(&input)).log10()
}

/// Drive tutti's SVF one sample at a time through its `AudioUnit` surface.
fn svf_gain_db(kind: SvfType, freq: f32) -> f64 {
    let mut node = SvfFilterNode::<f64>::new(kind, CUTOFF, Q);
    // The node is born at a placeholder rate; without this the corner sits
    // ~8.8% high and every comparison below is wrong for that reason alone.
    node.set_sample_rate(tutti_core::SampleRate(SR));
    measured_gain_db(
        move |x| {
            let mut out = [0.0f32; 1];
            node.tick(&[x], &mut out);
            out[0]
        },
        freq,
    )
}

/// How far `biquad` 0.5.0's stated cutoff is from the one it actually builds.
///
/// **This compensates for a bug in the oracle, not in tutti.** RBJ defines
/// `omega = 2*pi*f0/fs`. `biquad` 0.5.0 computes `normalized_f0 = f0/(2*fs)`
/// (coefficients.rs:302) and then `omega = PI * normalized_f0`
/// (coefficients.rs:104), giving `omega = pi*f0/(4*fs)` — four times too small,
/// so every filter it builds sits two octaves below the frequency asked for.
///
/// Without this factor the two filters disagree by up to 24 dB, which reads as
/// a catastrophic tutti bug and is nothing of the kind. With it, they agree to
/// 0.000 dB at every probe. That is the finding: the SVF is exactly an RBJ
/// biquad, and the oracle was the broken party.
const BIQUAD_OMEGA_BUG: f32 = 4.0;

/// The oracle: the same nominal filter, RBJ cookbook, direct form 1.
fn biquad_gain_db(kind: BqType<f32>, freq: f32) -> f64 {
    let coeffs = Coefficients::<f32>::from_params(
        kind,
        (SR as f32).hz(),
        (CUTOFF * BIQUAD_OMEGA_BUG).hz(),
        Q,
    )
    .expect("coeffs");
    let mut f = DirectForm1::<f32>::new(coeffs);
    measured_gain_db(move |x| f.run(x), freq)
}

/// Five frequencies spanning two decades around the 1 kHz corner. Three filter
/// kinds below use the same five, for the 15 probes the tolerance is measured
/// over.
const PROBES: [f32; 5] = [100.0, 500.0, 1_000.0, 2_000.0, 10_000.0];

/// The compensation is checked against the cookbook, not taken on trust.
///
/// [`BIQUAD_OMEGA_BUG`] is a claim about an external crate, and an unverified
/// claim about an oracle is exactly what produced the 24 dB false alarm this
/// file's header describes. So: ask `biquad` for the compensated cutoff and
/// assert its `a1` is the RBJ closed form evaluated at the frequency we
/// actually wanted.
///
/// RBJ, audio-EQ cookbook, low-pass:
///
/// ```text
/// w0    = 2*pi*f0/fs
/// alpha = sin(w0) / (2*Q)
/// a0    = 1 + alpha
/// a1    = -2*cos(w0)
/// ```
///
/// `biquad` stores its denominator already normalized by `a0`, so the value to
/// compare is `a1/a0`.
///
/// If a future `biquad` release fixes its omega, this test fails and says so —
/// which is the point. The three response tests would otherwise start
/// comparing against a filter two octaves away with no visible cause.
///
/// Mutation: `BIQUAD_OMEGA_BUG` 4.0 → 1.0 (i.e. "the oracle is fine") → fails,
/// biquad reporting -1.953721 against the cookbook's -1.815318.
#[test]
fn the_compensated_oracle_matches_the_rbj_cookbook() {
    let coeffs = Coefficients::<f32>::from_params(
        BqType::LowPass,
        (SR as f32).hz(),
        (CUTOFF * BIQUAD_OMEGA_BUG).hz(),
        Q,
    )
    .expect("coeffs");

    // The cookbook, at the cutoff we asked for — not the one biquad was told.
    let w0 = std::f64::consts::TAU * CUTOFF as f64 / SR;
    let alpha = w0.sin() / (2.0 * Q as f64);
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * w0.cos();
    let expected = a1 / a0;

    let actual = coeffs.a1 as f64;
    assert!(
        (actual - expected).abs() < 1e-5,
        "the omega compensation is wrong: biquad's normalized a1 is {actual:.6}, \
         the RBJ cookbook at {CUTOFF} Hz / {SR} Hz / Q {Q} gives {expected:.6}. \
         Either BIQUAD_OMEGA_BUG no longer describes this version of the crate, \
         or the crate has fixed its omega — check biquad's coefficients.rs \
         before touching anything in tutti."
    );
}

/// Low-pass: the two topologies must agree on the response.
///
/// Mutation: SVF cutoff x1.01 (a 1% error) -> fails, 0.1734 dB against the
/// 0.01 dB bound.
#[test]
fn svf_lowpass_tracks_an_rbj_biquad() {
    let mut worst = 0.0f64;
    for f in PROBES {
        let a = svf_gain_db(SvfType::LowPass, f);
        let b = biquad_gain_db(BqType::LowPass, f);
        let d = (a - b).abs();
        println!("LP {f:>8} Hz: svf {a:>8.3} dB, biquad {b:>8.3} dB, delta {d:.3} dB");
        worst = worst.max(d);
    }
    // Tolerance measured, not guessed. The observed delta at every probe is
    // 0.000 dB: a TPT state-variable filter and a direct-form-1 RBJ biquad at
    // the same cutoff and Q are the *same* transfer function, differing only
    // in how they store state. So the honest bound is "indistinguishable",
    // and 0.01 dB is that bound plus room for f32 accumulation. A looser one
    // would be unjustified — nothing here needs the slack.
    assert!(
        worst < 0.01,
        "LP: worst deviation {worst:.4} dB exceeds 0.01 dB"
    );
}

/// High-pass: same claim, and it catches an inverted response.
///
/// Mutation: SVF cutoff x1.01 -> fails at 0.1734 dB, the same sensitivity as
/// the low-pass.
#[test]
fn svf_highpass_tracks_an_rbj_biquad() {
    let mut worst = 0.0f64;
    for f in PROBES {
        let a = svf_gain_db(SvfType::HighPass, f);
        let b = biquad_gain_db(BqType::HighPass, f);
        let d = (a - b).abs();
        println!("HP {f:>8} Hz: svf {a:>8.3} dB, biquad {b:>8.3} dB, delta {d:.3} dB");
        worst = worst.max(d);
    }
    assert!(
        worst < 0.01,
        "HP: worst deviation {worst:.4} dB exceeds 0.01 dB"
    );
}

/// Band-pass. `biquad`'s `BandPass` is the unity-peak (constant-0-dB) form,
/// which is the same normalization the SVF's `m1 = 1/q` mix produces.
///
/// Mutation: SVF cutoff x1.01 -> fails at 0.0867 dB. Half the low-pass figure:
/// a band-pass is symmetric about the corner, so shifting it moves the two
/// skirts in opposite directions and the RMS sees partial cancellation. Still
/// 8x the bound.
#[test]
fn svf_bandpass_tracks_an_rbj_biquad() {
    let mut worst = 0.0f64;
    for f in PROBES {
        let a = svf_gain_db(SvfType::BandPass, f);
        let b = biquad_gain_db(BqType::BandPass, f);
        let d = (a - b).abs();
        println!("BP {f:>8} Hz: svf {a:>8.3} dB, biquad {b:>8.3} dB, delta {d:.3} dB");
        worst = worst.max(d);
    }
    assert!(
        worst < 0.01,
        "BP: worst deviation {worst:.4} dB exceeds 0.01 dB"
    );
}
