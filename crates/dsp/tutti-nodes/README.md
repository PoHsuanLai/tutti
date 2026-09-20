# tutti-nodes

The engine's built-in DSP nodes: filters, delays, dynamics, modulation effects,
mixing and automation.

## What this is

`AudioUnit` implementations, plus the `set(UnitParam)` surface a host drives them
through. Every node here goes into a `Net`, gets wired, and renders.

- **Filters** — `SvfFilterNode`, `LadderFilterNode`, `EqBandNode`, and their
  stereo pairs.
- **Delay** — `DelayLineNode`, `StereoDelayLineNode`, over the inner `DelayLine`.
- **Dynamics** — `CompressorNode`, `GateNode`, `BrickwallLimiterNode`,
  `LimiterNode`.
- **Modulation effects** — `ChorusNode`, `FlangerNode`, `PhaserNode`,
  `StereoPhaserNode`.
- **Distortion** — `DistortionNode` and its `ShapeKind` waveshapers.
- **Modulation sources** — `ModulatorNode<M>` (aliased `LfoNode`), the per-sample
  adapter over a pure `tutti_mod::Modulator`.
- **Mixing** — `ChannelSumNode` (K sources × N channels → one N-wide output),
  `DownmixNode`, `BusStripNode` (volume / balance / mute).
- **Param modulation** — `ParamPorts` plus the `param_mod` chain builders
  (`ParamSumNode`, `ParamShaperNode`, `AtomicSourceNode`), for a node that
  declares its own audio-rate control inputs.
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
use tutti_core::dsp::Net;
use tutti_core::AudioUnit;
use tutti_core::{Hz, Q};
use tutti_nodes::{SvfFilterNode, SvfType};

// A stereo net whose single node is a lowpass, fed by the net's input.
let mut net = Net::new(2, 2);
let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
let cutoff = filter.frequency(); // the shared cell, before the node is moved
let node = net.push(Box::new(filter));
net.pipe_input(node);
net.pipe_output(node);
net.check();

// The backend is what actually renders; `commit` only hands it a new net.
let mut backend = Box::new(net.backend()) as Box<dyn AudioUnit>;
let mut out = [0.0f32; 2];
backend.tick(&[1.0, 1.0], &mut out);

// Sweeping the cutoff on a *live* node: the write goes through the shared
// `Param` cell, so it reaches the copy the backend is rendering.
cutoff.store(Hz(2_000.0).get(), std::sync::atomic::Ordering::Release);
backend.tick(&[1.0, 1.0], &mut out);
```

## Live control values must live in shared storage (MANDATORY)

> A value a user can change **while the node is rendering** lives behind an `Arc`
> — a `Param<U>`, an `Arc<AtomicBool>`, an `Arc<AtomicU8>` — and its setter takes
> **`&self`**. A `&mut self` setter on an `AudioUnit` means exactly one thing:
> *restructure me, and expect a respawn.*

This is not style. `Net`'s frontend holds **clones** of its vertices, and
`Net::migrate` swaps the backend's unit back over any vertex it considers
unchanged. A control stored **by value** therefore cannot be changed on a live
node: the write lands on a clone the next commit discards. There is no error and
no diagnostic — the fader moves on screen and not in the sound.
`tests/live_value_survives_commit.rs` is that mechanism as three assertions.

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
a 16-byte `Copy` struct pays two `SeqCst` loads, a thread-local lookup and a
scarce guard slot for none of its benefit.

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
  `ConvolverNode`, `StereoConvolverNode`). Pulls `fft-convolver`.

There is no `bevy` feature, and no `spatial` / `hrtf` — see above.

## License

MIT OR Apache-2.0
