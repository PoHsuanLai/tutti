# tutti-types

Typed quantities and shapes shared by every engine crate: unit newtypes,
channel layouts, and the `AudioIn`/`AudioOut` I/O traits.

## What this is

The common definitions every other Tutti crate shares rather than duplicating.
Use it directly when you need only the vocabulary (a crate that implements
`AudioIn`, or one that only needs `Beat`); otherwise reach it through the
`tutti` umbrella, which re-exports it as `tutti::types`. What is in it:

- **`value`** — what a parameter *is* and how it is carried: the `Unit` marker
  trait, the measurement newtypes (`Hz`, `Seconds`, `Db`, `Beat`, `Bpm`,
  `Samples`, …), and the atomic `Param<U>` cell that holds one.
- **Channels** — `ChannelLayout`, a `u16` channel count with named constants
  (`MONO`, `STEREO`, ...): the one answer to "how many channels?" that every
  subsystem shares instead of a bare integer. `ChannelTopology` + `Speaker`
  answer the *different* question of which speaker each channel feeds; distinct
  questions, so distinct types.
- **Buffers** — `Interleaved` / `InterleavedMut`, a flat buffer that carries its
  own frame width, so a frame index and a sample index stop being the same type.
  The `fold_frame` family folds a wider signal into a narrower sink (ITU-R
  BS.775 / Dolby) rather than dropping the extra channels; only the common
  widths fold, and the matrix is not normalised (see `fold_frame`).
- **`io`** — `AudioIn` / `AudioOut` and `pump`: the two traits every audio source
  and sink in the engine speaks (mic, file, disk, plugin boundary).
- **`pcm`** — the canonical float→fixed-point quantization, shared by every codec
  and sink so a recorded and an exported file quantize a sample identically.
- **`wav`** — the WAV container facts two writers share: the speaker mask a
  multichannel file declares, and the size limit of a data chunk.
- **`latency`** / **`tail`** — PDC planning (`LatencyGraph`, `plan`,
  `delays`) and how long a graph rings after its input stops (`TailGraph`,
  `graph_tail`), as pure graph math over any graph representation.
- **`graph`** — the audio graph *as a value*: `Topology` (nodes, edges, outputs),
  `validate` → `Valid`, `topo_order`. `Clone + Eq + Hash`, so two graphs compare
  and one hashes; it implements both traits above, so latency and tail are folds
  over it. Compiling one into a runtime is `tutti-graph`'s compiler (a
  `GraphSpec` over a `Topology`, to a `Plan`) — that half names the units and
  so cannot live here.
- **`meter`** — musical meter: `TimeSignature`, the `MeterMap` timeline of
  changes, and the `Meter` trait that turns a `Beat` into a bar and beat. Pure
  musical math, so it layers *over* a transport rather than living inside one.

**The root is the API.** The common names are re-exported there, so
`tutti_types::Bpm` and `tutti_types::AudioIn` resolve directly — import from the
root, or from `prelude` for the common subset. Eight of the twelve modules are
public (`value`, `graph`, `io`, `latency`, `meter`, `pcm`, `tail`, `wav`),
because each holds items the root does not re-export (`graph::Source`,
`pcm::BitDepth`, everything in `wav`); the other four (`channels`, `downmix`,
`interleaved`, `topology`) are private and fully re-exported.

## What this crate does not own

Anything that renders, decodes, or opens a device. It is the **root leaf** —
Bevy-free, engine-free, depending on no workspace crate (its outside
dependencies are `atomic_float` and `libm`, plus `serde` and `bevy_reflect`
behind their features) and `#![forbid(unsafe_code)]`. The lock-free and `unsafe`
runtime primitives, the graph runtime, the engine, the device and the file
codecs are other crates'. Two consequences make the split load-bearing:

- The `AudioIn`/`AudioOut` traits are homed here so *any* subsystem can implement
  them without an absurd dependency edge. A sampler implementing `AudioIn` must
  not have to depend on the device layer to name the trait.
- Unit newtypes are **mandatory** across the engine, so their home has to be
  reachable from everywhere. A crate that only needs `Beat` gets `Beat` and
  nothing else.

## Example — the measurement vocabulary

This crate depends on no other engine crate, so there is nothing to integrate
with: what a consumer meets first is the units. Cross a family boundary with the
**named converter**, never with arithmetic on the inner float.

```rust
use tutti_types::{Cents, Db, SampleRate, Samples, Seconds, Semitones};

// dB → linear gain. `Db` is logarithmic, so this is a conversion, not a cast.
let trim = Db(-6.0);
assert!((trim.to_amplitude().get() - 0.501_187).abs() < 1e-5);

// Cascaded gain stages ADD in dB — that operator is opted in because it
// means something. Multiplication is NOT: scaling the dB value squares the
// amplitude, so `trim * 2.0` would be a quarter of the signal, not half of
// it. `Db` has no `Mul`; a factor of two lives in the amplitude domain.
assert_eq!(trim + trim, Db(-12.0));
let quartered = Db(-12.0).to_amplitude().get();
let squared = trim.to_amplitude().get() * trim.to_amplitude().get();
assert!((quartered - squared).abs() < 1e-6);

// Seconds → frames. The rounding is IN THE NAME because allocating a delay
// line and counting elapsed frames want different answers from one span.
let rate = SampleRate(48_000.0);
let block = Seconds(0.010_51); // 504.48 frames
assert_eq!(block.to_samples_floor(rate), Samples(504)); // frames elapsed
assert_eq!(block.to_samples_ceil(rate), Samples(505)); // frames to allocate

// Pitch offsets convert too — `cents.get() / 100.0` would compile and return
// `Cents`, a value wrong by 100x whose type claims it is fine.
assert_eq!(Cents(1200.0).to_semitones(), Semitones(12.0));
```

## Units are labels

A unit is a name on a float, not a validated value: `Pan(5.0)` builds. The
range a type documents is what its `new_clamped` or wrap produces, and
`new_clamped` passes NaN through (see the `units` module docs). The types that
do validate are `LoopRange`, `BeatsPerBar`, `NoteValue` and `MeterMap`.

## Operators are opt-in, and every omission is deliberate

A unit gets exactly the algebra it has. `Db * 2.0`, `Beat + Beat` and
`Azimuth: Ord` do not exist, because each would compile and mean the wrong
thing (a squared amplitude, two positions added with no origin, an ordering
on a circle). Each type's docs name what it omits and the named method that
replaces it, so the answer to a missing operator is always a method, never an
escape to raw `f32`.

## Features

Both are off by default (`default = []`) — the engine-wide rule that Bevy is
opt-in is stated here rather than inherited, because this is the crate every
other one names.

- `serde` — `Serialize`/`Deserialize` on the wire- and document-carried types:
  the unit newtypes, `Samples`, `Frame`, `Note`, `Latency`, `Tail`,
  `ChannelLayout`, `ChannelTopology` and the meter vocabulary. Not `Param`,
  `UnitParam` or `LoopRange`.
- `bevy` — `Reflect` on the unit newtypes, `ChannelLayout` and the param
  addresses (`UnitParam`, `UnitAddr`, `ParamAddr`). Reflection only: no
  `Component`/`Resource` derives, because an app stores a `Volume`, never a bare
  `Db`.

## License

MIT OR Apache-2.0
