# Synth correctness harness

Judges `PolySynth`'s rendered audio against first-principles synthesis theory —
harmonic series, filter transfer functions, unison beating.

```bash
just verify-audio                 # renders + judges everything, this included
# or directly:
cargo run --release \
    -p tutti-polysynth --example render_synth_cases -- /tmp/tutti-polysynth
uv run python \
    crates/dsp/tutti-polysynth/examples/verify_synth.py /tmp/tutti-polysynth
```

## Why this exists

The crate's unit tests mostly check plumbing: MIDI events reach voices,
allocation picks the right slot, a fork shares nothing with the live synth.
A test that plays three notes into a two-voice synth and checks which note a
voice reports would pass a synth that emitted silence, the wrong pitch or the
wrong timbre. This harness listens to the output.

## What is checked where

**Native Rust** (`tests/synth_audio.rs`, 12 tests) covers what needs no reference
implementation: the pitch each oscillator produces, velocity scaling, note-off
silencing, polyphonic summing, `NoSteal` and stealing behaviour, mono mode,
master volume, exact silence when idle, and both stereo channels. These run under
plain `cargo test`.

**Python** covers **spectral shape**, which the native tests cannot reach
cheaply: a sawtooth's 1/k harmonic series, a square's suppressed even harmonics,
a triangle's 1/k² rolloff, filter transfer functions, and unison beating.

The division is not cosmetic. Swapping the triangle oscillator for a saw — a pure
timbre bug that leaves the pitch exactly right — **passes all 12 native tests**
and fails 12 Python checks. That case is why the Python layer is worth its
dependency.

## Verified by mutation

A green harness proves nothing until it has been shown to go red. Each of these
was applied to the engine, confirmed to fail the right checks, and reverted:

| Mutation | Caught by |
|---|---|
| Ignore velocity (always full) | `velocity_scales_the_output_level` |
| Detune the note-on pitch path by a semitone | 4 native tests, including the stealing and mono-mode pitch assertions |
| Zero the right channel in one-frame blocks | `the_tick_path_matches_the_block_path` (see below) |
| Triangle oscillator emits a saw | 12 Python checks; **all 12 native tests still pass** |
| Filter cutoff off by an octave | 8 Python checks, including the −3 dB point |

## The one-frame path

A graph may hand the node a block of any length down to one frame.
`the_tick_path_matches_the_block_path` pins one-frame blocks against 64-frame
blocks: the same pitch, level and channel layout. They are deliberately *not*
compared sample-by-sample — the voice's control steps are cut at the block
edges, so their phase relative to a note-on differs by up to one block.

## One thing the judge gets right that is easy to get wrong

**A filter must be measured against the dry signal, not against itself.**
Comparing the loudest harmonic below the cutoff against the loudest above it,
within a single rendered file, reports a correct highpass as broken. A saw falls as 1/k, so the "above cutoff"
band's maximum always sits at its *lowest* harmonic — right at the transition
edge, where a correct filter has barely begun to act. It compared two points that
say nothing about the filter's response.

Measured properly — filtered amplitude over dry amplitude, per harmonic — the
highpass attenuates 110 Hz to 0.003 of source at a 2 kHz cutoff and passes
3.5 kHz at 0.95. Every filter's transfer at its named cutoff lands between 0.657
and 0.740, against the 0.707 that *defines* a cutoff frequency.

## What the checks establish

Every oscillator produces the correct pitch and the textbook harmonic series;
filters hit their −3 dB point within a few percent; unison beats and
decorrelates as configured; polyphony sums correctly.

The tuning tables match published temperament values: just intonation and
Pythagorean to 0.01 cents, and the hardcoded quarter-comma meantone table
within 0.5 cents of theory (whole-cent rounding, well under the perceptual
threshold).
