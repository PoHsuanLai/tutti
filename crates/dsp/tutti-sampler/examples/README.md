# Independent verification of sampler output

Renders a matrix of sampler cases to WAV, then judges them with numpy/scipy.

```bash
cargo run --release -p tutti-sampler --example render_cases -- /tmp/wavs

uv venv && uv pip install numpy scipy
.venv/bin/python crates/dsp/tutti-sampler/examples/verify_sampler.py /tmp/wavs
```

## Why this exists

Tests written against the same understanding of the DSP as the code cannot
contradict it, and a test that asserts only `!= 0.0` passes a gain error, a
phase-seeding error, an inert pitch shift or a smeared seek alike, since all of
them produce non-zero output.

`verify_sampler.py` derives its expectations from the case **name** and first
principles — an octave is 2x, a fifth is `2**(7/12)` — never from the Rust. It
re-measures frequency by FFT peak with parabolic interpolation, plus purity,
level, and block-RMS ripple. It is a second opinion, not a restatement.

`render_cases` drives the real `VoicePool` (placement gate, `MemorySource`, the
resident stretch filter, a transport advanced block by block, as a host
advances it), not `stretch::Unit` in isolation: the unit has its own tests, and
what this checks is the assembly. A time-stretched case must keep its pitch
(`stretch_half` measures 440 Hz), which is what separates a time-stretch from
varispeed.

## Reading the output

- **`ok`** — measured within tolerance. Every case is `ok`: 13 tonal cases
  plus the 2 seek cases.
- **`KNOWN-BUG`** — a real defect, reported rather than hidden. Listed in
  `KNOWN_BROKEN` in the script (empty at present), and only for its pitch
  symptom: a level or purity regression on the same case still fails. When the
  judge and the engine disagree and the *judge* is right, the case is marked
  here rather than having its tolerance widened until it passes.
- **0%-overlap exemption** — `pitch_down_two_octaves` and
  `stretch_half_pitch_down` reach an effective factor of 0.25, where the analysis
  hop equals the 2048 window and consecutive frames share no samples. A phase
  vocoder reconstructs from the phase relationship *between* overlapping frames,
  so there is nothing to reconstruct and the level ripples. Pinned in the
  stretch tests by `the_slowest_factor_ripples_because_its_frames_do_not_overlap`.
  Exempted from purity/ripple, still held to pitch and a looser level bound.

## Two things the harness must do

**Drive whole blocks under a moving transport.** A placed voice seats on the
playhead in each block's `Env` and steps through the block. Rendering one
frame per block against a transport that does not move emits the same sample
over and over: a staircase that resamples the source downward. The harness
drives blocks and moves the transport after each, as a host does.

**Keep an unprocessed control.** `dry` is what distinguishes "the engine is
broken" from "the harness is broken".
