# tutti-nodes

The Tutti audio engine's built-in DSP nodes: filters, delays, dynamics,
modulation effects, mixing and automation.

Every node here is a graph node (`tutti_graph::Node`). Build one, insert it
into a graph (`tutti_graph::{GraphBuilder, Editor}`), wire it, and the graph
prepares it at the device rate and renders it. Inserting a node hands back its
controls, usually a `tutti_graph::ParamSet` addressed by `UnitParam`, which
change the running node without a lock.

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

A `*Node` is the graph-facing node; the inner DSP objects some are built
from — `DelayLine`, `Lfo`, `Modulator`, `BandState` — carry no suffix, because
they are not nodes and can be used on their own.

Nothing in this crate can fail: there is no `Error` type, and a node is ready
to insert the moment it is built.

## Scope

- **No spatial panning.** The VBAP and binaural panners are `tutti-spatial`,
  which builds its mixes from this crate's `ChannelSumNode` and
  `SvfFilterNode`.
- **No Bevy.** ECS bindings are the host's; `bevy-tutti` drives the graph.
- **No sample playback and no synthesis.** Those are `tutti-sampler`,
  `tutti-polysynth` and `tutti-soundfont`.

## Quick start

```rust
use tutti_core::{Hz, Q, UnitParam};
use tutti_graph::{Prepare, Solo};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_types::{SampleRate, Samples};

// A lowpass alone in a graph, fed by the graph's input. The filter is a
// graph node: inserted, it hands back its `ParamSet` — cutoff, Q and
// gain by `UnitParam` — and the graph prepares it at the graph's rate.
let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
let mut solo = Solo::new(filter, Prepare::new(SampleRate(48_000.0), Samples(64)));
let out = solo.render_input(&[&[1.0; 64]]);

// Sweeping the cutoff on the *running* node: the set writes the `Param`
// cell the node reads, at the start of its next block.
assert!(solo.controls().set(UnitParam::Cutoff, 2_000.0));
let out = solo.render_input(&[&[1.0; 64]]);
```

## Changing a running node

Once a node is in a graph it belongs to the executor, so the control thread
reaches it only through what it shares: its `Param<U>` cells and atomics,
reached through the controls it was inserted with. A setter that takes
`&self` writes such a cell; it is lock-free and lands on the node's next
block. A setter that takes `&mut self` restructures the node and is meant for
before insertion (or for a replacement node). Clones of a node share its
cells, so a control handle stays valid for the node it came from.

`ParamSet::set` returns `false` for a param the node was not inserted with,
so a write to the wrong address is visible at the call site. A host pushing
one value to many nodes can ignore the `false` from the ones that do not own
it.

### Writing your own node's controls

The same rule applies to a node you write: a value that changes while the
node renders lives behind an `Arc`, shared across clones.

| the value | mechanism |
|---|---|
| one `f32` with a unit newtype | `Param<U>` + `&self` setter + its `UnitParam` address in the node's `ParamNode::param_set` |
| one `bool`, or a small `Copy` enum | `Arc<AtomicBool>` / `Arc<AtomicU8>` + `&self` setter. A bool rides `UnitParam`'s `>= 0.5` encoding, since `ParamSet::set` carries an `f32`. |
| a multi-field struct, or anything heap-backed | a command queue: flatten the struct into scalar fields, allocate sender-side, drain in the callback |

A plain `AtomicBool` field is not enough: `Clone` copies the atomic rather
than sharing it, so writes to one clone never reach another. `RtPublish` is
for moving a *deallocation* off the audio thread (routing tables, delay
vectors), not for sharing a small `Copy` value.

## Real-time behaviour

The nodes' `process` paths allocate nothing and take no lock, at any block
length up to the graph's prepared maximum; buffers are sized at construction
and in `prepare`, on the control thread. The recursive nodes (filters, delays,
modulation effects, dynamics) read a non-finite control value (NaN, ±∞) as
unchanged, so it never reaches their state.

## Where it sits

The `tutti` crate re-exports this one as `tutti::nodes`; `bevy-tutti`,
`tutti-spatial`, `tutti-polysynth`, `tutti-export` and `tutti-plugin` build on
it. It re-exports `ModParams`, `ModTarget`, `AtomicTarget`, `LayeredCurve` and
related items from `tutti-mod`, so a crate implementing `ModParams` needs no
separate `tutti-mod` dependency, and the unit types (`Hz`, `Db`, `Seconds`, …)
and `Param` from `tutti-core`.

## Features

`default = []`.

- `convolution` — FFT convolution reverb over a partitioned IR (`Convolver`,
  `ConvolverNode` at any width, with `IrChannelConfig`). Pulls `realfft`. An
  IR's spectra are stored once (`IrSpectra`, behind an `Arc`) and shared by
  every channel and every fork of the graph.
- `testing` — stimulus and plumbing nodes for tests, examples and benches
  (`Const`, `Osc`, `Through`, `Split`, `Sink`). Enable it from
  `[dev-dependencies]` only.

## License

MIT OR Apache-2.0
