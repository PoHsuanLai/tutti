# tutti-units

The engine's built-in DSP nodes: filters, delays, dynamics, modulation effects,
mixing and automation.

## What this is

`AudioUnit` implementations, plus the `set(UnitParam)` surface a host drives
them through. Roughly:

- **Filters** — `SvfFilterNode`, `LadderFilterNode`, `EqBandNode`, and their
  stereo pairs.
- **Delay** — `DelayLine`, `DelayLineNode`, `StereoDelayLineNode`.
- **Dynamics** — `Compressor`, `Gate`, `BrickwallLimiter`, `LimiterNode`.
- **Modulation effects** — `ChorusNode`, `FlangerNode`, `PhaserNode`.
- **Distortion** — `DistortionNode` and its `ShapeKind` waveshapers.
- **Modulation sources** — `ModulatorNode<M>` (aliased `LfoNode`), the
  per-sample adapter over a pure `tutti_mod::Modulator`.
- **Mixing** — `ChannelSumUnit` (K sources × N channels → one N-wide output),
  `DownmixUnit`, `BusStripUnit` (volume / balance / mute).
- **Param modulation** — `ParamPorts` plus the `param_mod` chain builders, for a
  node that declares its own audio-rate control inputs.
- **Automation / convolution** — the `automation` lane and recording units, and
  `ConvolverNode` with its IR generators (behind a feature).

## Why it is its own crate

It is the **infallible** DSP tier: no fallible operation anywhere, and therefore
no `Error` type at all. Speaker-layout construction was the last thing that
could fail here, and it left with the panners.

Two neighbours it is deliberately not:

- **No geometry.** The VBAP and binaural panners moved to
  [`tutti-spatial`](../tutti-spatial), which depends on *this* crate (its mix
  builder is assembled from `ChannelSumUnit` + `SvfFilterNode`). The arrow runs
  geometry → DSP and never back.
- **No Bevy, and no feature to add it.** The entire ECS binding layer — param
  and marker components, reconcile/spawn systems, deferred convolver load —
  moved app-side. Engine Bevy is the `Net` pump only.

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

There is no `bevy` feature, and no `spatial`/`hrtf` — see above.

## The rule this crate exists to enforce

**A value a user can change while the node is rendering lives behind an `Arc`,
and its setter takes `&self`.** `Net`'s frontend holds *clones* of its vertices,
and `Net::migrate` swaps the backend's unit back over any vertex it considers
unchanged — so a control stored by value cannot be changed on a live node. The
write lands on a clone the next commit discards, with no error and no
diagnostic: the fader moves on screen and not in the sound.

`&self` is necessary but not sufficient — a plain `AtomicBool` field also permits
`&self` and is still lost, because `Clone` copies the atomic rather than sharing
it. The property that matters is *shared across clones*.

The crate-level rustdoc has the full table of which mechanism to use for which
kind of value, and `tests/live_value_survives_commit.rs` is that mechanism as
three assertions.

## License

MIT OR Apache-2.0
