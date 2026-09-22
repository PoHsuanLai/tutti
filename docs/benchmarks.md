# Benchmark baseline

Recorded 2026-09-22 on:

| | |
|---|---|
| CPU | AMD Ryzen 9 9950X, 16 cores / 32 threads |
| OS | Linux 7.2.5 (Fedora 44) |
| rustc | 1.98.1 (48a229cea 2026-09-01) |
| profile | `bench` (release) |
| criterion | 0.8.2 |

**These numbers do not travel.** A figure from one machine says nothing about
another, which is why the machine is named above and why `just bench-save` /
`just bench-cmp` keep baselines local. Reproduce with:

```bash
just bench                    # full measurement
just bench-save main          # a baseline on YOUR machine
just bench-cmp main           # compare a change against it
```

## The unit that matters

At 48 kHz a **64-frame block has 1.333 ms** to be produced in. Every figure
below is also given as a percentage of that budget, because "12 µs" is not
actionable and "0.9% of the block budget" is. Criterion's `elem/s` divided by
48 000 is the realtime multiple.

## Graph render — `tutti-core/benches/engine_render.rs`

Cost of `Engine::process` against the number of DSP nodes in the graph, at a
64-frame block.

| nodes | per block | % of 1.333 ms |
|---|---|---|
| 1 | 0.79 µs | 0.06% |
| 8 | 2.45 µs | 0.18% |
| 32 | 8.21 µs | 0.62% |
| 128 | 31.6 µs | 2.4% |
| 512 | 124 µs | 9.3% |

Scaling is linear (4× the nodes costs 3.3–3.9× the time), so a rough ceiling
on **this machine, single-threaded, is ~5,500 simple filter nodes** before one
block consumes the whole budget. A superlinear reading here would itself be
the finding.

Other axes:

- **Block size.** 63–66 Melem/s from 64 to 1024 frames. The 64-frame figure is
  only ~4% below the 512-frame one, so the fixed per-callback cost (the
  backend pump, the stack buffer zeroing) is small — there is no large prologue
  to optimise away.
- **Device width**, at 256 frames: 1ch 3.15 µs, 2ch 3.88 µs, 6ch 4.64 µs,
  8ch 4.94 µs. The fold to the device width is cheap.
- **Transport.** `process` (motion drain + declick) and `process_segment`
  (render only) are within noise of each other at 3.71 µs. The transport costs
  effectively nothing per block.

## Output callback — `tutti-cpal/benches/audio_callback.rs`

**The reason this file exists.** `Engine::process` is not what a sound card
calls; the real callback also folds to stereo, meters, and converts format.
At 256 frames, stereo:

| | per block | vs render |
|---|---|---|
| `process_audio` alone | 1.96 µs | — |
| full callback, f32 out | 2.63 µs | **+34%** |
| full callback, i16 out | 2.73 µs | +39% |

So **a graph-only benchmark understates the callback by about a quarter of its
total cost.** Metering and the fold, not the format conversion, are most of
that gap — i16 adds only 4% over f32.

The master tap is opt-in and the cost matches: 2.81 µs closed, 3.43 µs open
(**+22%**) at 256 frames. Worth knowing before leaving one open by default.

## Synth voices — `tutti-polysynth/benches/polysynth.rs`

**`PolySynth` is hard-capped at 16 voices** — `PolySynth::new` rejects a
`max_voices` above `FINISHED_NOTES_CAPACITY` (16, at `polysynth.rs:36`). More
polyphony means more instances, not a bigger config.

| held voices | per 64-frame block | % of budget |
|---|---|---|
| 1 | 2.04 µs | 0.15% |
| 4 | 7.53 µs | 0.56% |
| 8 | 14.8 µs | 1.1% |
| 16 (the ceiling) | 31.0 µs | 2.3% |

- **Oscillator**, 16 voices: sine 9.0 µs, square 9.7 µs, saw 14.8 µs,
  triangle 15.3 µs.
- **Filter**, 16 voices: none 14.7 µs, SVF lowpass 16.7 µs (+13%),
  **Moog ladder 29.2 µs (+99%)** — the ladder roughly doubles the synth.
- **Unison**, 8 notes: 1× 15.3 µs, 3× 39.5 µs, 7× 85.6 µs. Unison multiplies
  the voice count, so this is where a budget actually goes.

## Sampler voices — `tutti-sampler/benches/voice_pool.rs`

Memory sources only; no disk, no butler.

| voices | per 64-frame block | % of budget |
|---|---|---|
| idle pool | 0.09 µs | ~0% |
| 1 | 1.37 µs | 0.10% |
| 8 | 9.85 µs | 0.74% |
| 32 | 38.4 µs | 2.9% |
| 64 | 81.3 µs | 6.1% |

Linear at ~1.27 µs per voice.

**The phase vocoder is the expensive thing in this crate**, and by a long way.
At 8 voices:

| stretch | per block | vs bypass |
|---|---|---|
| 1.0× (bypass) | 10.2 µs | — |
| 2.0× | 161 µs | **15.7×** |
| 0.5× | 165 µs | **16.1×** |

Pitch shift is far cheaper: 0 cents 9.9 µs, +700 cents 21 µs (~2×).

## Plugin hosting, in-process — `tutti-plugin/benches/vst2_in_process.rs`

The **only** plugin path criterion can measure honestly. Every format except
VST2-with-the-`vst2`-feature runs in a subprocess over an shm/IPC bridge,
where the tail decides whether audio drops out and criterion's outlier
*rejection* would discard exactly those samples.

| instances | per 64-frame block | % of budget |
|---|---|---|
| 1 | 0.39 µs | 0.03% |
| 4 | 1.63 µs | 0.12% |
| 16 | 6.60 µs | 0.50% |

Linear, and very cheap — but read it as a **floor, not a capacity**. The
reference plugin is a trivial test fixture, so what this measures is the
in-process hosting overhead, not any real plugin's DSP. It answers "does
hosting itself cost anything" (barely) rather than "how many plugins fit".

## Offline export — `tutti-export/benches/offline_render.rs`

`render_to_buffers`, no file touched:

| case | time | × realtime |
|---|---|---|
| 0.25 s stereo @ 48 k | 121 µs | **~2,070×** |
| 1 s stereo @ 48 k | 503 µs | ~1,990× |

So a three-minute stereo bounce is about **0.09 s** of render on this machine,
before encoding. Width at 0.25 s: mono 62 µs, stereo 73 µs, quad 115 µs,
5.1 151 µs.

**Encoding dominates a real export.** At 0.25 s stereo, Int24:

| format | time | vs render |
|---|---|---|
| wav | 269 µs | ~2× the render |
| ogg | 4.10 ms | ~34× |
| flac | 5.22 ms | ~43× |

A FLAC export is ~98% encoder and ~2% engine. **Never gate the `encode` group**
— its cost belongs to third-party encoders and the page cache.

## What is gated, and what is not

**Gated**, in the ordinary test job:
`crates/core/tutti-core/tests/alloc_budget.rs`. Allocation counts and bytes
are machine-independent, so a budget on them is a real regression gate that
survives a shared vCPU.

**Smoke-tested only**, on `push: main` and `workflow_dispatch`: the
`bench-smoke` CI job runs every benchmark once (`-- --test`) to prove the
harnesses still work against the current API. It compares nothing.

**Never gated:** any timing comparison. GitHub's runners swing 30–50% on this
workload, and `tutti-sampler/examples/profile_stretch_clone.rs` documents an
81× spread on a *quiet* machine. A flapping perf gate earns
`continue-on-error: true` within a month and then tests nothing — the way the
pre-extraction workflow died.

## Where criterion is the wrong tool, and what to use instead

Criterion suits steady-state, allocation-free, fixed-working-set code. It is
the wrong instrument where cost is dominated by allocation churn or by the
scheduler, because its outlier *rejection* discards exactly the samples that
decide whether audio drops out.

Two harnesses already cover those cases, and **neither should be ported to
criterion**:

- **`tutti-sampler/examples/profile_stretch_clone.rs`** — `Net::commit` at
  scale. A counting global allocator, `#[inline(never)]` phase markers for a
  sampling profiler, and a median rather than a mean, because wall-clock
  spread reached 81× on identical work. `just profile-stretch`.

- **`tutti-plugin/tests/real_plugin_pressure.rs`** — out-of-process plugin
  hosting, driven at true callback pacing, reporting **mean / p50 / p99 /
  worst / over-deadline / non-silent** per run. It asserts on **cost** and on
  a **throughput floor** (>90% of blocks carrying audio), and the history of
  that second assertion is the useful part: before `plugin-server`'s
  `raise_to_realtime`, three runs of the same code on the same machine gave
  29, 78 and 492 non-silent blocks out of 500 and the check had to be a
  printed note. With realtime priority on the subprocess it is 495-498/500,
  so the assertion now doubles as the regression guard for that priority
  going missing. The cost figure was always safe to assert either way: a
  starved block copies no audio, so starvation can only push cost down.

  Reference scaling from that harness (12-core, debug build, TAL-Reverb-4):
  1 plugin 23 µs median, 2 → 60 µs, 4 → 135 µs, 8 → 303 µs — **~23-38 µs per
  plugin against the 667 µs per plugin the synchronous design paid.** This is
  the engine's existing answer to expensive DSP, and it is not graph
  threading: the node pipelines, declares one block of latency, and PDC
  compensates.

  The real gap is not the measurement but its reach: every test there is
  `#[ignore]`d because it needs third-party plugins **installed on the
  machine**, so a bare checkout cannot run it even though the reference CLAP
  probe is built as a dev-dependency. Teaching it to fall back to the probe
  would make it runnable everywhere; a second harness would not.

This is also why there is no criterion bench for the out-of-process path.
The bridge is asynchronous — `Batcher::collectable` substitutes silence for
any block the server has not answered — so a late block does not arrive late,
it **never arrives**. "Round-trip latency" is not the quantity; starvation
rate against a deadline is, and that is what the harness above measures.
