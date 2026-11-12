# DSP correctness harness

Judges tutti-dsp's filters and the export resampler against the published
definitions of what they compute, re-derived with scipy.

```bash
cargo xtask verify-audio                 # renders + judges everything, this included
# or directly:
cargo run --locked --release \
    -p tutti-dsp --example render_dsp_cases -- /tmp/tutti-dsp
uv run python \
    crates/tutti-dsp/examples/verify_dsp.py /tmp/tutti-dsp
```

## Why this exists

The crate's tests pin each filter against its own model (`SvfModel`,
`LadderModel`, `CrossoverModel`): the render agrees with the model that
describes it. That cannot tell whether the model is the right filter. This
harness compares the render with transfer functions written from the
definitions by a script that shares nothing with the crate.

## What is checked

The Rust half renders impulse responses (float WAVs, one channel per band for
the crossover) and tones through the resampler, and writes `cases.json` with
the parameters of each. The Python half evaluates each impulse response's DTFT
on a log grid from 20 Hz to 0.45 x rate and compares it with the reference, in
magnitude (dB, where the reference is above -60 dB) and phase (degrees, above
-40 dB). It prints one row per case and exits non-zero on any FAIL.

| Case | Reference | Tolerance |
|---|---|---|
| SVF: low-pass, high-pass, band-pass, notch, all-pass, bell, low and high shelf, three settings each, f64 state (and f32 for a third of them) | the biquads of the RBJ Audio EQ Cookbook, coded in the script | 0.01 dB, 0.1 degree (f64); 0.05 dB, 0.5 degree (f32) |
| Ladder LP24, no resonance | four bilinear one-poles, prewarped at the cutoff | 0.01 dB, 0.1 degree |
| Ladder LP24, resonance | `1 / ((1 + s/wc)^4 + 4 res)` under the prewarped bilinear map (the loop is zero-delay, so exact to rounding) | 0.01 dB, 0.1 degree |
| Linkwitz-Riley, 2 and 3 bands | scipy `butter(2)` squared; band j is the high-pass of every earlier split, the low-pass of its own and the all-pass of every later one; the bands sum to a flat magnitude | 0.01 dB, 0.1 degree |
| Phaser all-pass chain, 1 and 4 stages, three centres | magnitude 1 and phase `-2 n atan(tan(pi f/fs) / tan(pi fc/fs))` | 0.01 dB, 0.2 degree |
| Export resampler, 44.1 to 48 kHz and back | the ideal band-limited converter; `scipy.signal.resample_poly` is run beside it and printed | passband ripple 0.1 dB up to 0.9 x the lower Nyquist; the -3 dB point at or above 0.92 x Nyquist; tones keep their frequency to 0.5 Hz; output length to one frame; tones above the target Nyquist 60 dB down (20 dB at 450 Hz above) |

The SVF is a different topology from a biquad with the same transfer function
(both are the bilinear transform of `1 / (s^2 + s/Q + 1)` prewarped at the
cutoff), so its magnitude and phase agree to rounding, which is why the
tolerances are that tight.

## Known findings

A case whose disagreement is understood is listed in `KNOWN` in the script, with
the reason: it prints as KNOWN and does not fail the run. Every other FAIL does,
and so does a KNOWN case that passes (a stale excuse: remove it from `KNOWN`).

- **SVF band-pass gain.** `SvfType::BandPass` has a peak gain of Q (the cookbook's
  "constant skirt gain" band-pass), not 0 dB; the two agree at Q = 1. The script
  judges it against the constant-skirt form. The variant is not stated in the
  enum's docs.
