# tutti-mod

Modulation for the Tutti engine — pure `phase -> value` sources, a keyed
accumulator target, and a mod-matrix router.

## What this is

Three roles, layered: **rules** (a routing table of `ModEdge`s), **dispatch** (an
id→target router), and **receive** (a keyed accumulator that clamps
`base + Σ offsets`). The pure floor underneath them is the `Modulator` trait —
`phase -> value`, state-threaded like `Iterator::scan` — with `Lfo`, sample &
hold, and the shared `shape` / `fold` / `curve_apply` math.

`ModMatrix` is the fluent front door; the primitives are rarely touched
directly.

## Why it is its own crate

**Audio-free and Bevy-free.** Nothing here touches a sample buffer or an ECS
`World`, which is what lets the same `Modulator` drive a filter cutoff, a UI
colour, or a spring. The Bevy-side reconciliation that binds routes onto engine
params is `bevy_tutti::modulation`'s, one layer up; this vocabulary is its
foundation and stands alone without it.

The second payoff is that the value tiers agree by construction. One `Curve`
(`beat -> Option<f32>`) is read at three rates — per frame (`AtomicTarget`), per
block (a plugin's param producer), per sample (`tutti_nodes::ModulatorNode`) —
and both the scalar and curve paths apply the identical
`shape(raw, depth, polarity, curve)` expression, so switching delivery does not
change what you hear.

## Where it sits

Depends only on `tutti-types` (for `Phase`, `PhaseIncrement`, `Depth`,
`ParamAddr`) and `audio-automation` (for `CurveType`). `tutti-nodes` and
`tutti-polysynth` depend on it — `tutti-nodes` re-exports `ModParams`,
`ModTarget` and `AtomicTarget` so a downstream node reaches them without a
second dependency. `bevy-tutti` layers the ECS reconciliation on top.

## Features

`default = []` — the pure floor (the `Modulator` trait, `Lfo`, the shape math)
needs no features at all.

- `std` — heap/`Arc` support, required by the routing layer.
- `routing` — the mod-matrix subsystem: `ModTarget`/`AtomicTarget`, `ModRouter`,
  `ModRoutingTable`, `ModPreFrame`, and `ModMatrix`. Implies `std`.
- `bevy` — `Component`/`Reflect` on `ModTargetId`, `LayerKey` and the
  `LfoShape`/`Polarity` vocabulary. Implies `routing`.

Note that most of the type names above are behind `routing`, which is off by
default; docs.rs renders with `all-features = true`.

## Example

The crate-level rustdoc carries a verified `ModMatrix` quick start and a
cascading-source example (one LFO modulating another's rate). Both are doctests:

```
cargo test --doc -p tutti-mod --features routing
```

## License

MIT OR Apache-2.0
