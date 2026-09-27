# tutti-nodes

The engine's built-in DSP nodes: filters, delays, dynamics, modulation effects,
mixing and automation.

## What this is

`AudioUnit` implementations, plus the `set(UnitParam)` surface a host drives them
through. Every node here goes into a `Net`, gets wired, and renders.

- **Filters** — `SvfFilterNode`, `LadderFilterNode` (both any width, one
  coefficient solve shared across the channels) and `EqBandNode`.
- **Delay** — `DelayLineNode` (any width, with a cross-feedback routing
  matrix), over the inner `DelayLine`.
- **Dynamics** — `CompressorNode`, `GateNode`, `BrickwallLimiterNode`,
  `LimiterNode`.
- **Modulation effects** — `ModDelayNode` (chorus and flanger are its two
  `ModDelayConfig` presets) and `PhaserNode`, both any width.
- **Distortion** — `DistortionNode` and its `ShapeKind` waveshapers.
- **Modulation sources** — `ModulatorNode<M>` (aliased `LfoNode`), the per-sample
  adapter over a pure `tutti_mod::Modulator`.
- **Mixing** — `ChannelSumNode` (K sources × N channels → one N-wide output),
  `DownmixNode`, `BusStripNode` (volume / balance / mute).
- **Param modulation** — each modulatable node declares its params (its
  `*_PARAMS` list: `SVF_PARAMS`, `DELAY_PARAMS`, …) as param ports the
  graph's compiler-owned modulation fills per frame (read through
  `Io::param`), and `ParamModShaping` turns a route's depth / polarity /
  curve into the shaping the graph applies.
- **Automation / convolution** — the `automation` module's `AutomationLaneNode`
  and recording units, and `ConvolverNode` with its IR generators (behind a
  feature).

Note the suffix convention: a `*Node` is the graph-facing `AudioUnit`, while the
inner DSP objects it is built from — `DelayLine`, `Lfo`, `Modulator`, `BandState`
— deliberately carry no suffix, because they are not nodes.

## What it does not own

It is the **infallible** DSP tier: no fallible operation anywhere, and therefore
no `Error` type at all. Speaker-layout construction was the last thing that could
fail here, and it left with the panners.

- **No geometry.** The VBAP and binaural panners are
  [`tutti-spatial`](../tutti-spatial)'s, which depends on *this* crate — its mix
  builder is assembled from `ChannelSumNode` + `SvfFilterNode`. The arrow runs
  geometry → DSP and never back.
- **No Bevy, and no feature to add it.** The entire ECS binding layer — param and
  marker components, reconcile/spawn systems, deferred convolver load — is
  app-side. Engine Bevy is the `Net` pump only.
- **No sample playback and no synthesis.** Those are `tutti-sampler`,
  `tutti-polysynth` and `tutti-soundfont`.

## Quick start

```rust
use tutti_core::{Hz, Q, UnitParam};
use tutti_graph::{Prepare, Solo};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_types::{SampleRate, Samples};

// A lowpass alone in a graph, fed by the graph's input. The filter is a
// native graph node: inserted, it hands back its `ParamSet` — cutoff, Q and
// gain by `UnitParam` — and the graph prepares it at the graph's rate.
let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
let mut solo = Solo::new(filter, Prepare::new(SampleRate(48_000.0), Samples(64)));
let out = solo.render_input(&[&[1.0; 64]]);

// Sweeping the cutoff on the *running* node: the set writes the `Param`
// cell the node reads, at the start of its next block.
assert!(solo.controls().set(UnitParam::Cutoff, 2_000.0));
let out = solo.render_input(&[&[1.0; 64]]);
```

## Live control values must live in shared storage (MANDATORY)

> A value a user can change **while the node is rendering** lives behind an `Arc`
> — a `Param<U>`, an `Arc<AtomicBool>`, an `Arc<AtomicU8>` — and its setter takes
> **`&self`**. A `&mut self` setter means exactly one thing: *restructure me,
> and expect a respawn.*

This is not style. A node in the graph belongs to the executor: nothing on the
control side can reach it but what it shares — its cells, through the controls
it was inserted with. A control stored **by value** cannot be changed on a live
node at all; the change is a replacement (a crossfade to a new node).
`tests/live_controls_reach_the_node.rs` is that rule as assertions. (Under
`Net` it was worse: its frontend held **clones** of its vertices, so a by-value
write compiled, landed on a clone, and the next commit discarded it, with no
error and no diagnostic.)

**`&self` is necessary, not sufficient.** A plain `AtomicBool` field also permits
`&self` and is *still* lost, because `Clone` copies the atomic rather than
sharing it (`tutti_sampler`'s `MemorySource` is the cautionary example:
`trigger` / `play` / `stop` all take `&self` and all evaporate). The property
that matters is **shared across clones**; `&self` is how you get there, not proof
that you did.

### Which mechanism, by what the value is

| the value | mechanism |
|---|---|
| one `f32` with a unit newtype | `Param<U>` + `&self` setter + a `UnitParam` arm in `AudioUnit::set` |
| one `bool`, or a small `Copy` enum | `Arc<AtomicBool>` / `Arc<AtomicU8>` + `&self` setter. A bool rides `UnitParam`'s documented `>= 0.5` encoding — `Setting` carries an `f32`, so it has to. |
| a multi-field struct, or anything heap-backed | a command queue: flatten the struct into scalar fields, allocate sender-side, drain in the callback |

`Param<U>` stops where `Setting` stops: its payload is one `f32`, so anything
wider leaves the `set()` path entirely. That is a property of the transport, not
a limitation of `Param`.

`RtPublish` is **not** on this ladder. It exists to move a *deallocation* off the
audio thread — routing tables, PDC vectors, meter maps. Reaching for it to share
a 16-byte `Copy` struct pays a slot CAS, a `SeqCst` fence and one of the cell's
reader slots for none of its benefit.

### The failure is silent at three layers, and counted at the fourth

Worth knowing before assuming a setting arrived. `AudioUnit::set` has an **empty
default body**, so a unit that does not implement it swallows every setting; a
unit that does implement it ignores params it does not own (which is deliberate —
it is what lets a host push without dispatching on node type); and `from_setting`
answers `None` for an unknown id. Those three are silent.

The fourth is counted. `Net::take_unaddressed_settings` reports settings aimed at
a node id the graph does not hold, kept deliberately separate from
`Net::take_dropped_settings`: a *dropped* setting is backpressure that clears
itself, an *unaddressed* one is a wiring bug that will lose every future write to
the same target.

Neither counter reaches the first three layers, and no counter can — a unit that
ignores a param it does not own is indistinguishable, from `Net`, from one that
owns it and does nothing. That is what a host's audible end-to-end tests are
for, and they necessarily live wherever the DAW param vocabulary does.

## Where it sits

Depends on `tutti-core`, `tutti-types`, `tutti-mod` (with `routing`) and
`audio-automation`. `tutti-spatial`, `tutti-export`, `tutti-plugin` and
`bevy-tutti` depend on it. It re-exports `ModParams`, `ModTarget`,
`AtomicTarget`, `LayeredCurve` and friends from `tutti-mod`, so a downstream
crate implementing `ModParams` needs no separate `tutti-mod` dependency.

## Features

`default = []`.

- `convolution` — FFT convolution reverb over a partitioned IR (`Convolver`,
  `ConvolverNode` at any width, with `IrChannelConfig`). Pulls `realfft`; the
  partitioned convolution is this crate's, so an IR's spectra are stored once
  (`IrSpectra`, behind an `Arc`) and shared by every channel and every fork.

There is no `bevy` feature, and no `spatial` / `hrtf` — see above.

## License

MIT OR Apache-2.0
