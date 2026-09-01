# Tutti Spatial

Spatial audio: VBAP speaker panning, binaural HRTF rendering, surround mix assembly.

## What this is

The engine's only **geometry** — azimuth, elevation, speaker layouts, HRIR spheres. Everything that processes a signal per channel lives in [`tutti-nodes`](../tutti-nodes); everything that needs to know *where a sound is* lives here.

Two independent renderers, each named for its algorithm rather than the category: `vbap` (loudspeakers, `layout.count()` outputs) and `hrtf` (headphones, always 2). Each owns its own error type; there is no crate-level `Error`. Shared between them: `SpatialTarget`, the position de-zipper, and the SMPTE/WAV channel-order conventions in `layout`.

`vbap::VbapPannerNode` places a source in a speaker field via [vbap](https://crates.io/crates/vbap) (presets for 2/4/6/8/12 channels — stereo, quad, 5.1, 7.1, 7.1.4). `hrtf::HrtfBinauralNode` renders to headphones by FFT convolution against a measured HRIR sphere, using the [hrtf](https://crates.io/crates/hrtf) crate; the dataset is supplied by the caller as bytes. `vbap::build_vbap_mix` assembles the whole `sources → panners → sum` graph in one call, including LFE bass management. It is named for the algorithm, not the output shape — there is no non-VBAP way to reach it.

Position changes are de-zippered by a one-pole smoother, so a moving source does not click.

## Quick Start

```rust
use tutti_spatial::{build_vbap_mix, VbapSource};
use tutti_types::ChannelLayout;

// Place two sources in a quad field and get back the summed 4-wide mix node.
let mix = build_vbap_mix(
    &mut net,
    ChannelLayout::QUAD,
    &[
        VbapSource::at(front, 45.0),   // front-left
        VbapSource::at(rear, 135.0),   // rear-left
    ],
)?;
net.pipe_output(mix);
```

## Features

- `hrtf` — real HRTF binaural rendering (`HrtfBinauralNode`). Off by default; pulls the `hrtf` crate.

## Why it depends on tutti-nodes

`build_vbap_mix` builds its graph out of general-purpose units: `ChannelSumNode` folds the panners into one N-wide node, and `SvfFilterNode` low-passes the LFE send at ~120 Hz. That is a plain consumer edge — geometry depends on signal processing, never the reverse, and neither of those units has anything spatial in it.

## Node ids

The `AudioUnit` fingerprints in `node_id.rs` are **persisted values** and must not be renumbered. `assert_unique` guards them within this crate; cross-crate uniqueness rests on the mnemonic convention described in `tutti_core::node_id`.

## RT safety

`tests/rt_no_alloc.rs` asserts both panners' `process` paths never allocate. Mutation-verified: injecting a `vec!` into the VBAP process path aborts the test.
