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

That paid for itself immediately: it found that `VoicePool` never applied
`stretch::Unit::input_rate`, so every stretch behaved as varispeed — the Rust
suite could not see it, having no test that rendered a stretched *placed* voice
end to end. That defect is **fixed**: `input_rate` is now read on both tiers and
in both drive paths (`voice/slot.rs`), and all six `stretch_*` cases pass.

## Reading the output

- **`ok`** — measured within tolerance. Every case is `ok` today: 13 tonal
  cases plus the 2 seek cases.
- **`KNOWN-BUG`** — a real defect, reported rather than hidden. **No case is
  in this state right now.** The verdict is kept because reporting a defect
  beats suppressing one: when the judge and the engine disagree and the *judge*
  is right, the case is marked here rather than having its tolerance widened
  until it passes.

  It last held the six `stretch_*` cases, when `VoicePool` stepped the source by
  `window_rate()` alone (varispeed) and never called `input_rate` — the vocoder
  got one source sample per output sample, so the stretch factor moved pitch
  instead of duration. `input_rate` is now read in all four places that drive a
  stretched voice (`voice/slot.rs`), memory and disk tiers, `tick` and `process`
  alike, and the judge measures `stretch_half` at 439.9 Hz — unshifted, which is
  what separates a real time-stretch from the varispeed it used to be.
- **0%-overlap exemption** — `pitch_down_two_octaves` and
  `stretch_half_pitch_down` reach an effective factor of 0.25, where the analysis
  hop equals the 2048 window and consecutive frames share no samples. A phase
  vocoder reconstructs from the phase relationship *between* overlapping frames,
  so there is nothing to reconstruct and the level ripples. Pinned in `stretch.rs`
  by `the_slowest_factor_ripples_because_its_frames_do_not_overlap`. Exempted
  from purity/ripple, still held to pitch and a looser level bound.

## Two traps, both paid for

**`tick` once read one sample per block.** A placed voice derives its position
from the playhead, which advances once per block. `process` walked the block
with an offset; `tick` had none, so 64 calls against one transport reading
emitted the same sample 64 times — a staircase that resamples the source
downward. The first draft drove `tick` and every case failed, *including*
`dry`. A placed read now seats on the clock and steps through a block through
either entry point (`MemorySource::seated_position`); the harness still drives
`process`, as a host does.

**Keep an unprocessed control.** `dry` is what distinguishes "the engine is
broken" from "the harness is broken". Both times this harness was wrong, `dry`
is what said so.
