# tutti-spatial

Spatial audio: VBAP speaker panning, binaural HRTF rendering, and surround mix
assembly.

## What this is

The engine's only **geometry** — azimuth, elevation, speaker layouts, HRIR
spheres. Two independent renderers, each named for its algorithm rather than the
category it falls in:

| | `vbap` | `hrtf` |
|---|---|---|
| renders to | loudspeakers | headphones |
| outputs | `layout.count()` | always 2 |
| needs | a speaker layout | an HRIR dataset |
| fails with | `VbapError` | `HrtfBinauralError` |

Each owns its own error type; there is deliberately no crate-level `Error` and
nothing called `SpatialPanner`. Shared between them: `SpatialTarget` (bearing and
height as lock-free params), the position de-zipper, and the SMPTE/WAV
channel-order conventions in `layout`.

`VbapPannerNode` places a source in a speaker field via
[vbap](https://crates.io/crates/vbap), with presets for 2/4/6/8/12 channels
(stereo, quad, 5.1, 7.1, 7.1.4). It takes **two inputs**, not one: a mono source
presents the same sample on both ports. `HrtfBinauralNode` renders to headphones
by FFT convolution against a measured HRIR sphere, using the
[hrtf](https://crates.io/crates/hrtf) crate; the dataset is supplied by the caller
as bytes. `build_vbap_mix` assembles the whole `sources → panners → sum` graph in
one call, including LFE bass management — named for the algorithm, not the output
shape, because there is no non-VBAP way to reach it.

Position changes are de-zippered by a one-pole smoother, so a moving source does
not click.

## What it does not own

- **No per-channel signal processing.** Filters, delays, dynamics and the mix
  primitives are [`tutti-nodes`](../tutti-nodes)'s. This crate *depends* on that
  one — `build_vbap_mix` folds its panners with `ChannelSumNode` and low-passes
  the LFE send with `SvfFilterNode` at 120 Hz, the conservative end of the
  Dolby/DTS 80–120 Hz bass-management crossover. The arrow runs geometry → DSP
  and never back; neither of those units has anything spatial in it.
- **No room simulation, and no reverb.** Convolution reverb is `tutti-nodes`'
  `convolution` feature.
- **No HRIR data.** The dataset is the caller's bytes.

## Quick start

A panner alone: stereo in, one output per speaker. Both panners are
graph nodes (`tutti_graph::Node`); inserting one hands back its controls.

```rust
use tutti_core::{Azimuth, Elevation, SampleRate, Samples};
use tutti_graph::{Prepare, Solo};
use tutti_spatial::VbapPannerNode;

// 5.1: two inputs, six outputs. Only 2/4/6/8/12 have presets.
let panner = VbapPannerNode::surround_5_1().expect("5.1 is a defined preset");
assert_eq!(panner.num_channels(), 6);

// 45° to the left, level with the listener. `store` normalizes: the bearing
// wraps, the height clamps.
panner.set_position(Azimuth(45.0), Elevation(0.0));

// Alone in a graph (`Solo`, for tests and examples; a host inserts it with
// `GraphBuilder::add_with_controls` or `Editor::insert`, and keeps the
// controls the same way).
let mut solo = Solo::new(panner, Prepare::new(SampleRate(48_000.0), Samples(64)));

// The controls move the running node: a lock-free write, landing on its next
// block, de-zippered over 50 ms.
solo.controls().set_position(Azimuth(-30.0), Elevation(0.0));

let out = solo.render_input(&[&[1.0; 64], &[1.0; 64]]); // one `Vec` per speaker
assert_eq!(out.len(), 6);
```

A whole mix: `build_vbap_mix` places several sources and returns the summed
N-wide node.

```rust
use tutti_graph::{GraphBuilder, Prepare};
use tutti_nodes::testing::Const;
use tutti_spatial::{build_vbap_mix, VbapSource};
use tutti_types::{ChannelLayout, SampleRate, Samples};

let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);

// Two sources, each a node whose output ports 0 and 1 feed its panner. A mono
// source presents the same sample on both.
let front = g.add(Const::frame(&[1.0, 1.0]));
let rear = g.add(Const::frame(&[1.0, 1.0]));

let mix = build_vbap_mix(
    &mut g,
    ChannelLayout::QUAD,
    &[
        VbapSource::at(front, 45.0),   // front-left
        VbapSource::at(rear, 135.0),   // rear-left
    ],
)?;
g.pipe_output(mix);

let mut r = g
    .renderer(Prepare::new(SampleRate(48_000.0), Samples(64)))
    .expect("builds");
let out = r.render(64); // one `Vec` per speaker
assert_eq!(out.len(), 4);
# Ok::<(), tutti_spatial::VbapError>(())
```

## Constraint: angles do not compare or add

`Azimuth` and `Elevation` carry no `Ord` and no `Add`. A circle has no ends, so
"greater" is undefined and a sum has no origin: aiming at -170° from 170° is a
20° move across the seam, not a 340° sweep back through zero. Take differences in
the scalar space via `.get()`, and let `SpatialTarget::store` normalize — the
bearing wraps, the height clamps.

## `reset` clears time, not placement

Both panners' `Node::reset` drops the de-zipper ramp (and, for the binaural
one, the frame bridge and convolution tails) and **keeps** position, spread,
width and blend: those are caller-set configuration. It also seats the ramp on
the commanded position, so the first block after a reset already renders there
rather than gliding in from front-centre. A fork (an offline export) is reset
before it renders, and its placement is the one set when it was taken.

## Node ids

The `AudioUnit` fingerprints in `node_id.rs` are **persisted values** and must not
be renumbered. Both panners are graph nodes now and report none, but the ids
stay reserved. `assert_unique` guards them within this crate; cross-crate
uniqueness rests on the mnemonic convention described in `tutti_core::node_id`.

## RT safety

`tests/rt_no_alloc.rs` asserts both panners' `process` paths never allocate,
each alone in a graph (`tutti_graph::contract::BlockRig`). Mutation-verified:
injecting a `vec!` into the VBAP process path aborts the test.

## Features

`default = []`.

- `hrtf` — real HRTF binaural rendering (`HrtfBinauralNode`), by FFT convolution
  against a measured HRIR sphere. Pulls the `hrtf` crate. Off by default, so that
  renderer is absent from a default build.

## License

MIT OR Apache-2.0
