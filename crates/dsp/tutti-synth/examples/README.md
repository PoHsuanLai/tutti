# Synth correctness harness

Judges `PolySynth`'s rendered audio against first-principles synthesis theory —
harmonic series, filter transfer functions, unison beating.

```bash
just verify-audio                 # renders + judges everything, this included
# or directly:
cargo run --release --manifest-path crates/bevy-tutti/Cargo.toml \
    -p tutti-synth --example render_synth_cases -- /tmp/tutti-synth
cd crates/tutti && uv run python \
    crates/dsp/tutti-synth/examples/verify_synth.py /tmp/tutti-synth
```

## Why this exists

`tutti-synth` is not an untested crate — it carries 3,134 lines of tests, over
half its source. But they test **plumbing**: MIDI events reach voices, allocation
picks the right slot, `isolate` severs a shared inbox, atomics propagate across
clones. Almost nothing asserted what comes out of `process`.

`test_voice_stealing_in_polysynth` is representative: it plays three notes into a
two-voice synth and checks that *a* voice reports note 67. It never listens. A
synth that allocated perfectly and emitted silence — or the wrong pitch, or the
wrong timbre — passes it.

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

## Verified by sabotage

A green harness proves nothing until it has been shown to go red. Each of these
was applied to the engine, confirmed to fail the right checks, and reverted:

| Sabotage | Caught by |
|---|---|
| Ignore velocity (always full) | `velocity_scales_the_output_level` |
| Detune the note-on pitch path by a semitone | 4 native tests, including the stealing and mono-mode pitch assertions |
| Zero the right channel in `tick` | `the_tick_path_matches_the_block_path` — **nothing caught this before the test was added** (see below) |
| Triangle oscillator emits a saw | 12 Python checks; **all 12 native tests still pass** |
| Filter cutoff off by an octave | 8 Python checks, including the −3 dB point |

## What the sabotage pass found

`AudioUnit` requires both `tick` (per-sample) and `process` (per-block), and
`PolySynth` implements the mixing logic **twice**. Every test in this file — and
every pre-existing test in the crate — drove `process`. Zeroing the right channel
inside `tick` broke nothing.

That is a real hole, not a hypothetical: a host may call either. It is now
covered by `the_tick_path_matches_the_block_path`, which asserts the two agree on
pitch, level and channel layout. They are deliberately *not* compared
sample-by-sample — `process` applies MIDI at block boundaries while `tick`
advances the allocator every sample, so their phase relative to a note-on differs
by up to one block.

## One thing the judge gets right that is easy to get wrong

**A filter must be measured against the dry signal, not against itself.** The
first version of the filter check compared the loudest harmonic below the cutoff
against the loudest above it, within a single rendered file. It reported both
highpass cases as broken.

The check was broken, not the filter. A saw falls as 1/k, so the "above cutoff"
band's maximum always sits at its *lowest* harmonic — right at the transition
edge, where a correct filter has barely begun to act. It compared two points that
say nothing about the filter's response.

Measured properly — filtered amplitude over dry amplitude, per harmonic — the
highpass attenuates 110 Hz to 0.003 of source at a 2 kHz cutoff and passes
3.5 kHz at 0.95. Every filter's transfer at its named cutoff lands between 0.657
and 0.740, against the 0.707 that *defines* a cutoff frequency.

## Result

No defect found in `tutti-synth`. Every oscillator produces the correct pitch and
the textbook harmonic series; filters hit their −3 dB point within a few percent;
unison beats and decorrelates as configured; polyphony sums correctly.

The tuning tables were also checked against published temperament values and are
exact: just intonation and Pythagorean match the literature to 0.01 cents, and
the hardcoded quarter-comma meantone table is within 0.5 cents of theory
(whole-cent rounding, well under the perceptual threshold).
