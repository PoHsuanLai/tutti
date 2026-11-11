# tutti-nodes

The Tutti audio engine's built-in DSP nodes: filters, delays, dynamics,
modulation effects, mixing and automation.

Every node here is a graph node (`tutti_graph::Node`), and a node file holds
only the adapter: shape, prepare, process, params and fork. The algorithm each
one runs is a pure function or struct over slices in `tutti-dsp`: the filter
coefficients and steps (`SvfCoeffs`, `LadderCoeffs`, `Crossover`), the dynamics
curves and kernels, `Convolver`, `DelayLine` and the waveshaping curves. Build one, insert it
into a graph (`tutti_graph::{GraphBuilder, Editor}`), wire it, and the graph
prepares it at the device rate and renders it. Inserting a node hands back its
controls, usually a `tutti_graph::ParamSet` addressed by `UnitParam` (a
`UnitAddr` for a node with several params of a kind), which change the running
node without a lock.

- **Filters** — `SvfFilterNode`, `LadderFilterNode` (both any width, one
  coefficient solve shared across the channels) and `EqBandNode`.
- **Crossover** — `CrossoverNode`, which makes `bands × channels` outputs from
  `tutti_dsp::CrossoverDesign`, a 2 to 6 band Linkwitz-Riley 4th-order split (the
  bands sum to flat magnitude; IIR, so not linear-phase and no latency). Each
  crossover is an indexed `Cutoff` param (`UnitAddr::new(UnitParam::Cutoff, i)`).
- **Delay** — `DelayLineNode` (any width, with a cross-feedback routing
  matrix), over `tutti_dsp::DelayLine`.
- **Dynamics** — `CompressorNode`, `GateNode`, `BrickwallLimiterNode`,
  `LimiterNode`, and `MultibandCompressorNode` (2 to 4 bands, a linked
  compressor per band of a `Crossover`). Every setting is an indexed param: a
  threshold, knee, ratio, attack, release and make-up (`GainDb`) per band, a
  `Cutoff` per crossover (27 ports at 4 bands), and a `Bypass` and `Solo`
  toggle per band (`>= 0.5` is on, as every toggle; cells without a port). The
  per-band gain reduction meters are readings on `MultibandControls`.
- **Modulation effects** — `ModDelayNode` (chorus and flanger are its two
  `ModDelayConfig` presets) and `PhaserNode`, both any width.
- **Distortion** — `DistortionNode` and its `ShapeKind` waveshapers.
- **Modulation sources** — `ModulatorNode<M>` (aliased `LfoNode`), the per-sample
  adapter over a pure `Modulator` (an LFO's is `tutti_modulation::Wave`, the wave a
  control lane evaluates): an audio generator, whose output feeds a node's
  audio input.
- **Dry/wet** — the convolver, delay, chorus/flanger, phaser and the two reverbs output their wet signal only; blend with the input through the graph's `set_mix`.
- **Mixing** — `BusStripNode` (volume / balance; a mute or bypass is the graph's `NodeMode`).
  Summing is the graph's: a port or an output channel sums its sources; so is
  folding between layouts (a bus edge between declared buses).
- **Spatial** — `VbapPannerNode` (stereo in, one output per speaker of a 2, 4,
  6, 8 or 12 channel layout) and `HrtfBinauralNode` (two outputs, by FFT
  convolution against a caller-supplied HRIR sphere), both steered by their
  params (bearing, height, spread and width, or the blend), de-zippered. A mix of several panned sources
  is plain fan-in: wire each panner's outputs to the same graph ports.
- **Param ports** — each modulatable node declares its params (its
  `*_PARAMS` list: `DELAY_PARAMS`, …) as param ports, filled per
  frame from one event source each (read through `Io::param`).
- **Modulation** — the `automation` module's `ControlLaneNode`, which
  evaluates a `tutti_modulation::ControlProgram` into a param port (design doc 014),
  the signal sources that feed a lane (`EnvelopeFollowerNode`,
  `NoteEnvelopeNode`), and `Modulation`, the reconciler that builds them from
  `tutti_modulation` declarations.
- **Automation / convolution** — the `automation` module's `AutomationLaneNode`
  (a curve as audio) and recording units, and `ConvolverNode` with its IR
  generators (behind a feature).

A `*Node` is the graph-facing node; the inner object some are built from,
`Modulator`, carries no suffix, because it is not a node and
can be used on its own. The algorithms are `tutti-dsp`'s (`tutti_dsp::DelayLine`,
`tutti_dsp::Crossover`, …); this crate re-exports only the types in a node's
signature: `SvfType`, `LadderType`, `Real`, `CrossoverError`,
`InterpolationMode` and `ShapeKind`.

Nothing in this crate can fail: there is no `Error` type, and a node is ready
to insert the moment it is built.

## Scope

- **No room simulation and no HRIR data.** The panners place a source; the
  HRIR dataset is supplied by the caller as bytes.
- **No Bevy.** ECS bindings are the host's; `bevy-tutti` drives the graph.
- **No sample playback and no synthesis.** Those are `tutti-sampler`,
  `tutti-polysynth` and `tutti-soundfont`.

## Quick start

```rust
use tutti_types::MaxBlock;
use tutti_types::{Hz, Q, UnitParam};
use tutti_graph::{Prepare, Solo};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_types::{SampleRate, Samples};

// A lowpass alone in a graph, fed by the graph's input. The filter is a
// graph node: inserted, it hands back its `ParamSet` — cutoff, Q and
// gain by `UnitParam` — and the graph prepares it at the graph's rate.
let filter = SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(800.0), Q(0.707));
let mut solo = Solo::new(filter, Prepare::new(SampleRate(48_000.0), MaxBlock::of(Samples(64))));
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
than sharing it, so writes to one clone never reach another. `ArcLend` is
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
`tutti-polysynth`, `tutti-export` and `tutti-plugin` build on
it. It re-exports the unit types (`Hz`, `Db`, `Seconds`, …) and `Param` from
`tutti-types`.

## Features

`default = []`.

- `convolution` — FFT convolution reverb over a partitioned IR
  (`ConvolverNode` at any width, with `IrChannelConfig`, over
  `tutti_dsp::Convolver`, whose FFT is `tutti_dsp::RealFft`: scalar, so the
  output is CPU-independent). An IR's spectra are stored once
  (`tutti_dsp::IrSpectra`, behind an `Arc`) and shared by every channel and
  every fork of the graph.
- `testing` — stimulus and plumbing nodes for tests, examples and benches
  (`Const`, `Osc`, `Through`, `Split`, `Sink`). Enable it from
  `[dev-dependencies]` only.

## License

MIT OR Apache-2.0
