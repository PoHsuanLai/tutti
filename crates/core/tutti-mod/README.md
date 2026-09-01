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
directly. (Most type names below sit behind the `routing` feature, which is off
under `default = []`, so they are written as code spans rather than links;
docs.rs renders with `all-features = true`.)

## What this crate does not own

- **Audio.** Nothing here touches a sample buffer. A `Modulator` is as usable
  driving a UI colour or a spring as a filter cutoff. The per-sample node
  adapters (`ModulatorNode`, `AutomationLaneNode`) are `tutti-nodes`'.
- **The ECS.** Nothing here touches a `World`. The Bevy-side reconciliation that
  binds routes onto engine params is `bevy_tutti::modulation`'s, one layer up;
  this vocabulary is its foundation and stands alone without it.
- **Failure.** This crate exports no `Error` type on purpose — modulation is
  infallible by design. Every operation is arithmetic over values validated at
  construction; absence (an unresolved target, an empty curve) is an `Option`,
  never a failure.

It depends only on `tutti-types` (for `Phase`, `PhaseIncrement`, `Depth`,
`ParamAddr`) and `audio-automation` (for `CurveType`). `tutti-nodes` and
`tutti-polysynth` depend on it — `tutti-nodes` re-exports `ModParams`,
`ModTarget` and `AtomicTarget` so a downstream node reaches them without a
second dependency.

## Example — source, shape, target, ungated

The three roles on the **pure floor**, which is what `default = []` builds: a
`Modulator` produces a raw `-1..1`, `shape` turns that into a signed offset in
the target's own units, and `fold` accumulates offsets onto a base. No feature,
no `Arc`, no audio buffer — the same three lines drive a filter cutoff, a UI
colour, or a spring.

```rust
use tutti_mod::{fold, shape, CurveType, Lfo, LfoShape, Modulator, Polarity};
use tutti_types::{Depth, Hz, Phase, PhaseIncrement};

// A 2 Hz sine, stepped at the frame rate. The increment is a *type*: a
// hand-rolled `%` wrap keeps its input's sign and walks off the front of a
// shape table when the rate goes negative.
let lfo = Lfo::new(LfoShape::Sine);
let step = PhaseIncrement::per_sample(Hz(2.0), 60.0);

// State is threaded in and out, `Iterator::scan`-style — the modulator is
// `&self` and stores nothing, which is what lets one be a graph node.
let mut state = <Lfo as Modulator>::State::default();
let mut phase = Phase::wrapped(0.25);        // a quarter turn: sine peak
let (next, raw) = lfo.value(state, phase);
state = next;
phase = phase.advance(step);
assert!((raw - 1.0).abs() < 1e-6);

// Depth + polarity + curve turn the raw value into an offset. Bipolar keeps
// the sign, so a cutoff modulated by half depth swings both ways.
let offset = shape(raw, Depth(0.5), Polarity::Bipolar, CurveType::Linear);
assert_eq!(offset, 0.5);

// The accumulator every tier shares: `(base + Σ offsets).clamp(min, max)`.
// Additive and order-independent, so two sources onto one param commute.
let cutoff = fold(1_000.0, [offset * 2_000.0, -250.0].into_iter(), 20.0, 20_000.0);
assert_eq!(cutoff, 1_750.0);
# let _ = (state, phase);
```

## Quick start — `ModMatrix`

The front door is a fluent builder. Feature-gated, so this block is inert under
`default = []`.

```rust
# #[cfg(feature = "routing")] {
use tutti_mod::{ModMatrix, Lfo, LfoShape, SourceRate};
use tutti_types::{Beat, BeatDuration, Hz, Seconds};

let mut m = ModMatrix::new();
let cutoff = m.target(1000.0, 0.0, 2000.0);   // a modulatable param
let gain   = m.target(0.5,    0.0, 1.0);

// Each source runs at its own rate: 2 Hz free, 1 cycle/beat synced.
m.route(Lfo::new(LfoShape::Sine), SourceRate::free_running(Hz(2.0), 0.0))
    .to(&cutoff).depth(1.0);
m.route(Lfo::new(LfoShape::Triangle), SourceRate::beat_synced(BeatDuration(1.0), 0.0))
    .to(&gain).depth(0.5);

let mut driver = m.build();
// Once per frame: pass the transport beat + seconds since the last frame.
for i in 0..4 { driver.run(Beat(i as f64 * 0.25), Seconds(1.0 / 60.0)); }
let hz = cutoff.value();                           // read the modulated value
# assert!((0.0..=2000.0).contains(&hz));
# }
```

## One value, three sampling rates

The rates below are **not three designs**. They are one function read at three
speeds, and knowing that is the difference between picking a rate and thinking
you must pick a mechanism.

`Curve` is that function: `beat -> Option<f32>`, holding no clock and consulting
no loop range, so the *reader* supplies the `Beat`. An automation envelope, an
LFO, a constant, and the summing `LayeredCurve` are all `Curve`s. Because
`LayeredCurve` is itself one, the accumulator
(`clamp(base + Σ layer(beat), [min, max])`) is shared across every rate — one
summation rule, the rate chosen by whoever reads it.

| Rate | Sink | How it gets the beat |
|---|---|---|
| per **frame** | `AtomicTarget` | the driver is handed the beat; it collapses to a scalar and mirrors it into an `AtomicF32` |
| per **block** | a plugin's param producer | holds the `LayeredCurve` and samples it at each block's real beats |
| per **sample** | `AutomationLaneNode`, `ModulatorNode` (`tutti-nodes`) | the beat arrives as a *signal* on the node's `BEAT_PORTS` inputs |

**Which rate to reach for.** The frame rate is the default and is always
correct — ask for more only when the sink reads faster than the frame rate,
where a frame scalar shows up as a staircase and a finer rate traces the ramp.
The per-sample tier costs a graph edge (the node must be wired to the transport
clock); the per-block tier costs nothing extra but requires a sink that accepts
a curve, which `AtomicTarget` deliberately does not — it collapses at a fixed
beat, so a curve stored there would never move.

The tiers agree on *values* by construction, not by coincidence: both the scalar
path (`ModPreFrame::run`) and the curve path (`ShapedCurve`) apply the identical
`shape(raw, depth, polarity, curve) * (max - min)` expression, so a route that
switches delivery does not change what the listener hears.

A `Modulator` is itself rate-agnostic — the *adapter* around it picks the tier.
`tutti_mod::Lfo` sampled by `ModPreFrame` is frame-rate; the same `Lfo` inside
`tutti_nodes::ModulatorNode` (aliased `LfoNode`) is per-sample. One modulator,
two adapters — not two LFOs.

One gap is known and deliberate: the routing subsystem cannot deliver a curve to
a **per-sample** sink for a native param, because `AtomicTarget` is the only sink
native nodes use. Its module doc tracks the audio-rate sink as later work.

## The target + routing (the `routing`/`bevy` features)

- **Receive** — `ModTarget`: a keyed accumulator (`base + Σ keyed offsets`,
  clamped). Concrete: `AtomicTarget` (mirrors its value into a shared
  `AtomicF32` the consumer reads lock-free), a frame-rate cap over a
  `LayeredCurve`.
- **Dispatch** — `ModRouter` / `ModBus`: an id→target map keyed by `ModTargetId`.
- **Rules** — `ModRoutingSnapshot` / `ModRoutingTable`: an `RtPublish`-hot-swapped
  mod-matrix of `ModEdge`s.
- **Driver** — `ModPreFrame`: the once-per-frame producer that samples each
  source and dispatches its shaped offset by id. It owns the sources and threads
  their state across frames.

## Cascading — a source that modulates another source

A `SourceRate`'s frequency is a `Rate`: either a constant `Hz`, or a
`Param<Hz>` read fresh each frame. Point an `AtomicTarget` at that same cell and
one LFO drives another's rate, using the ordinary target/edge machinery — no
special case in the driver:

```rust
# #[cfg(feature = "routing")] {
use tutti_mod::{AtomicTarget, Lfo, LfoShape, SourceRate, Sourced};
use tutti_types::{Hz, Param};

let rate: Param<Hz> = Param::new(Hz(2.0));
// The target writes the cell the source reads — one cell, two views.
let target = AtomicTarget::with_mirror(2.0, 2.0, 10.0, rate.as_atomic());
let wobbling = Sourced::new(Lfo::new(LfoShape::Sine),
    SourceRate::free_running(rate.clone(), 0.0));
# let _ = (target, wobbling);
# }
```

Build one cell and clone the handle: a separately-minted `Param<Hz>` compiles
and modulates nothing. A cascade lags by at most one frame, which is what lets a
cycle (A drives B's rate, B drives A's) settle instead of recursing.

## Module map

- `modulator`, `lfo`, `shape` — the pure source + math.
- `id` — `ModTargetId` (target address) + `LayerKey` (contributor key).
- `target` — `ModTarget`, the keyed sink.
- `curve`, `curve_source`, `layered` — `Curve`, its shaped/beat-driven sources,
  and the summing `LayeredCurve` (feature-gated).
- `param`, `router`, `routing`, `driver` — the routing subsystem (feature-gated).
- `mod_params` — `ModParams`, the trait a foreign unit implements to expose its
  params as mod targets (feature-gated).
- `matrix` — `ModMatrix`, the fluent builder over all of the above.

## Features

`default = []` — the pure floor (the `Modulator` trait, `Lfo`, the shape math)
needs no features at all.

- `std` — heap/`Arc` support, required by the routing layer.
- `routing` — the mod-matrix subsystem: `ModTarget`/`AtomicTarget`, `ModRouter`,
  `ModRoutingTable`, `ModPreFrame`, `Curve`/`LayeredCurve`, `ModParams`, and
  `ModMatrix`. Implies `std`.
- `bevy` — `Component`/`Reflect` on `ModTargetId`, `LayerKey` and the
  `LfoShape`/`Polarity` vocabulary. Implies `routing`.

## License

MIT OR Apache-2.0
