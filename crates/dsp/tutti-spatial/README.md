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

A panner alone: stereo in, one output per speaker.

```rust
use tutti_core::dsp::Net;
use tutti_core::AudioUnit;
use tutti_core::{Azimuth, Elevation};
use tutti_spatial::VbapPannerNode;

// 5.1: two inputs, six outputs. Only 2/4/6/8/12 have presets.
let panner = VbapPannerNode::surround_5_1().expect("5.1 is a defined preset");
assert_eq!(panner.num_channels(), 6);

// 45° to the right, level with the listener. `store` normalizes: the bearing
// wraps, the height clamps. It is a lock-free write, so it may happen while
// the node renders.
panner.set_position(Azimuth(45.0), Elevation(0.0));

let mut net = Net::new(2, 6);
let node = net.push(Box::new(panner));
net.pipe_input(node);
net.pipe_output(node);
net.check();

let mut out = [0.0f32; 6];
net.tick(&[1.0, 1.0], &mut out);
```

A whole mix: `build_vbap_mix` places several sources and returns the summed
N-wide node.

```rust
use tutti_core::dsp::Net;
use tutti_core::AudioUnit;
use tutti_nodes::testing::Const;
use tutti_spatial::{build_vbap_mix, VbapSource};
use tutti_types::ChannelLayout;

let mut net = Net::new(0, 4);

// Two sources, each a node whose output ports 0 and 1 feed its panner. A mono
// source presents the same sample on both.
let front = net.push(Box::new(Const::frame(&[1.0, 1.0])));
let rear = net.push(Box::new(Const::frame(&[1.0, 1.0])));

let mix = build_vbap_mix(
    &mut net,
    ChannelLayout::QUAD,
    &[
        VbapSource::at(front, 45.0),   // front-left
        VbapSource::at(rear, 135.0),   // rear-left
    ],
)?;
net.pipe_output(mix);
net.check();

let mut out = [0.0f32; 4];
net.tick(&[], &mut out);
# Ok::<(), tutti_spatial::VbapError>(())
```

## Constraint: angles do not compare or add

`Azimuth` and `Elevation` carry no `Ord` and no `Add`. A circle has no ends, so
"greater" is undefined and a sum has no origin: aiming at -170° from 170° is a
20° move across the seam, not a 340° sweep back through zero. Take differences in
the scalar space via `.get()`, and let `SpatialTarget::store` normalize — the
bearing wraps, the height clamps.

## Known deviation: `VbapPannerNode::reset` clears configuration, not just state

`AudioUnit::reset` means "clear runtime state" everywhere else in the engine.
This node's `reset` additionally rewrites caller-set **configuration**: it
returns spread to `Spread::POINT`, width to `StereoWidth::NATURAL`, and the
position to front/level. So a `reset` intended to drop a tail also silently
discards where the caller had placed the source.

Stated rather than fixed, so nothing is written around it: **do not call `reset`
to clear a tail on this node.** Rebuild the node, or re-`set_position` after.

## Node ids

The `AudioUnit` fingerprints in `node_id.rs` are **persisted values** and must not
be renumbered. `assert_unique` guards them within this crate; cross-crate
uniqueness rests on the mnemonic convention described in `tutti_core::node_id`.

## RT safety

`tests/rt_no_alloc.rs` asserts both panners' `process` paths never allocate.
Mutation-verified: injecting a `vec!` into the VBAP process path aborts the test.

## Features

`default = []`.

- `hrtf` — real HRTF binaural rendering (`HrtfBinauralNode`), by FFT convolution
  against a measured HRIR sphere. Pulls the `hrtf` crate. Off by default, so that
  renderer is absent from a default build.

## License

MIT OR Apache-2.0
