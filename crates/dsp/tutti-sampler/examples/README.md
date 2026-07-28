# Independent verification of sampler output

Renders a matrix of sampler cases to WAV, then judges them with numpy/scipy.

```bash
cargo run --release -p tutti-sampler --example render_cases -- /tmp/wavs

uv venv && uv pip install numpy scipy
.venv/bin/python crates/dsp/tutti-sampler/examples/verify_sampler.py /tmp/wavs
```

## Why this exists

Four defects in this crate — a 60 dB gain error, a 17.5 dB phase-seeding error,
an inert pitch shift, and a seek smear — were each invisible to the whole test
suite. The tests missed them because they were written against the same
understanding of the DSP as the code, so they could not contradict it. Most
asserted only `!= 0.0`, and every one of those defects produced non-zero output.

`verify_sampler.py` derives its expectations from the case **name** and first
principles — an octave is 2x, a fifth is `2**(7/12)` — never from the Rust. It
re-measures frequency by FFT peak with parabolic interpolation, plus purity,
level, and block-RMS ripple. It is a second opinion, not a restatement.

That paid for itself immediately: it found that `VoicePool` never applies
`stretch::Unit::input_rate` (see below), which the Rust suite could not see
because it had no test rendering a stretched *placed* voice end to end.

## Reading the output

- **`ok`** — measured within tolerance.
- **`KNOWN-BUG`** — a real defect, reported rather than hidden. Today: the six
  `stretch_*` cases. `VoicePool` steps the source by `window_rate()` (varispeed
  only) and never calls `input_rate`, which has **zero call sites** in the crate
  on any branch. So the vocoder is fed one source sample per output sample and
  the stretch factor behaves as varispeed — pitch moves by the factor, duration
  does not change. The `stretch::Unit` half is correct and unit-tested; the
  assembly never wires it.
- **0%-overlap exemption** — `pitch_down_two_octaves` and
  `stretch_half_pitch_down` reach an effective factor of 0.25, where the analysis
  hop equals the 2048 window and consecutive frames share no samples. A phase
  vocoder reconstructs from the phase relationship *between* overlapping frames,
  so there is nothing to reconstruct and the level ripples. Pinned in `stretch.rs`
  by `the_slowest_factor_ripples_because_its_frames_do_not_overlap`. Exempted
  from purity/ripple, still held to pitch and a looser level bound.

## Two traps, both paid for

**Drive with `process`, not `tick`.** A placed voice derives its position from
the playhead, which advances once per block. `process` walks the block with
`offset_in_block`; `tick` has no offset, so 64 calls against one transport
reading emit the same sample 64 times — a staircase that resamples the source
downward. The first draft did this and every case failed, *including* `dry`.

**Keep an unprocessed control.** `dry` is what distinguishes "the engine is
broken" from "the harness is broken". Both times this harness was wrong, `dry`
is what said so.
