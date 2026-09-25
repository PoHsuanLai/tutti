# Analysis correctness harness

Judges `tutti-analysis`'s **pitch** and **loudness** readings against independent
references — librosa for YIN, pyloudnorm for EBU R128.

```bash
just verify-audio                 # renders + judges everything, this included
# or directly:
cargo run --release \
    -p tutti-analysis --example render_analysis_cases -- /tmp/tutti-analysis
uv run python \
    crates/dsp/tutti-analysis/examples/verify_analysis.py /tmp/tutti-analysis
```

## Why this exists

`pitch.rs` holds the whole YIN implementation — the FFT autocorrelation, the
cumulative-mean normalisation, the first-local-minimum rule, the parabolic
interpolation — and has **no test module at all**. Its wrapper `yin.rs` is well
tested, but for *behaviour*: ranges refused, silence unvoiced, a reused detector
matching a fresh one. Exactly one test checked an actual frequency, at one tone,
to ±1 Hz. Octave errors, YIN's signature failure mode, were unasserted anywhere.

Loudness had the mirror-image gap: the plumbing was tested (streaming folds to
one-shot, ragged chunks handled) but never the numbers.

## Shape

Unlike the sampler and export harnesses, which write *audio* for Python to
analyse, this one writes the analyser's *answers* as CSV and Python recomputes
them. What is under test here is the analysis itself.

The signal generators are deliberately **duplicated** in both halves rather than
shared through a file. If both sides read the same generated WAV, agreement would
prove only that they read the same bytes. Each side synthesises from the case
name — `saw_220` is a sawtooth at 220 Hz because the name says so — so a
disagreement is a real disagreement about the DSP.

## What is checked where

Native Rust (`tests/pitch_accuracy.rs`, `tests/loudness_accuracy.rs`, 15 tests)
carries everything that needs no reference library: accuracy to 0.1% across the
range, octave errors, missing fundamentals, level independence, and for loudness
the values R128 defines on paper. These run under plain `cargo test`.

Python carries the cross-implementation checks — librosa on the identical signal,
pyloudnorm at three sample rates — plus the per-frame sweep accuracy, which needs
a model of the analysis window that is clearer to express in NumPy.

## Two things the judge gets right that are easy to get wrong

**YIN does not analyse the whole frame.** `compute_difference` sets
`window = max_period` and correlates `x[0..window]` against
`x[0..window + max_period]`, so with the standard 50 Hz floor at 48 kHz a
1920-sample frame is judged on its first 960 samples. Taking the frame *centre*
as the expected value puts every sweep expectation 14–17 Hz high and reads as a
systematic downward bias in the detector. It is not one — the same code reads a
static tone to within 0.002 Hz at the same frame length.

**A chirp has no single true frequency.** Even against the correct window the
reading sits slightly high, by an amount that falls monotonically with frequency
(4.4 Hz at 220 Hz, 1.4 Hz at 745 Hz). In the period domain, which is what YIN
estimates, that is −4.4 samples shrinking to −0.12: the lowest frame has the
longest period, so its window spans the fewest cycles and the estimate is pulled
hardest. The sweep tolerance is therefore scaled by how far the sweep travels
inside the analysis window, not set to a flat percentage — a flat 2% fails frame
0 alone and invites widening the number until it passes.

**librosa is a cross-check, not an oracle.** It reads static tones ~0.2% high
itself, so pitch is asserted against the *synthesised* frequency, which is known
exactly, with librosa reported alongside so a disagreement can be attributed.

## Verified by sabotage

A green harness proves nothing until it has been shown to go red. Each of these
was applied to the engine, confirmed to fail the right checks, and reverted:

| Sabotage | Caught by |
|---|---|
| Return the integer period (drop parabolic interpolation) | `pure_tones_are_detected_across_the_range`, +3 Python checks — 0.8–1.3% error at 880 Hz and above |
| Take the global minimum instead of the first local one below threshold | `harmonic_tones_report_the_fundamental_not_an_octave` by name, +12 Python checks including the librosa cross-check |
| Hardcode 48 kHz in the meter (the defect `LoudnessConfig` exists to prevent) | `loudness_is_independent_of_sample_rate`, + pyloudnorm at 96 kHz |

## Result

No defect found in either surface. Pitch is accurate to under 0.02% on every
static tone including missing-fundamental and odd-harmonic cases; loudness agrees
with pyloudnorm to within 0.05 LU at three sample rates.
