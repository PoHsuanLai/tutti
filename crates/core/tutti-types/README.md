# tutti-types

The engine's shared value vocabulary: unit newtypes, RT-callback primitives, and
the `AudioIn`/`AudioOut` I/O traits.

## What this is

The common definitions every other Tutti crate shares rather than duplicating —
`tutti-core`, the plugin hosts, export, analysis. Eight families:

- **`value`** — what a parameter *is* and how it is carried: the `Unit` marker
  trait, the measurement newtypes (`Hz`, `Seconds`, `Db`, `Beat`, `Bpm`,
  `Samples`, …), and the atomic `Param<U>` cell that holds one.
- **RT primitives** — what the audio callback touches: `AudioThreadCell`
  (one-borrow-at-a-time interior mutability), the capped `RtEventBuf` (reached
  through `&self`) and `RtVec` (through `&mut self`), the fixed-capacity
  `RtScratch` (own-and-slice, no push), the `ScopedNoDenormals` guard, and
  `RtPublish`/`RtRef` for handing non-scalar state to the audio thread.
- **Channels** — `ChannelLayout` (`Mono`/`Stereo`/`Multi(n)`): the one answer to
  "mono, stereo, or how many?" that every subsystem shares instead of a private
  enum or a bare channel-count integer. `ChannelTopology` + `Speaker` answer the
  *different* question of which speaker each channel feeds; distinct questions,
  so distinct types.
- **Buffers** — `Interleaved` / `InterleavedMut`, a flat buffer that carries its
  own frame width, so a frame index and a sample index stop being the same type.
  `downmix`'s `fold_frame` family folds a wider signal into a narrower sink
  (ITU-R BS.775 / Dolby) rather than dropping the extra channels.
- **`io`** — `AudioIn` / `AudioOut` and `pump`: the two traits every audio source
  and sink in the engine speaks (mic, file, disk, plugin boundary).
- **`pcm`** — the canonical float→fixed-point quantization, shared by every codec
  and sink so a recorded and an exported file quantize a sample identically.
- **`latency`** / **`tail`** — PDC planning (`LatencyGraph`, `plan`,
  `compensate`) and how long a graph rings after its input stops (`TailGraph`,
  `graph_tail`), as pure graph math over any graph representation.
- **`meter`** — musical meter: `TimeSignature`, the `MeterMap` timeline of
  changes, and the `Meter` trait that turns a `Beat` into a bar and beat. Pure
  musical math, so it layers *over* a transport rather than living inside one.

**The root is the API.** Everything is re-exported there, so `tutti_types::Bpm`
and `tutti_types::AudioThreadCell` resolve directly — import from the root, or
from `prelude` for the common subset, not through a module path. Most modules
are private for that reason; the ones that stay public do so because
`tutti-core` re-exports them as modules (`tutti_core::io::AudioIn`).

## What this crate does not own

Anything that renders, decodes, or opens a device. It is the **root leaf** —
Bevy-free, engine-free, depending only on `smallvec` / `atomic_float` /
`arc-swap`. The graph is `tutti-core`'s, the device `tutti-cpal`'s, the file
codecs `tutti-io`'s and `tutti-export`'s. Two consequences make the split
load-bearing:

- The `AudioIn`/`AudioOut` traits are homed here so *any* subsystem can implement
  them without an absurd dependency edge. A sampler implementing `AudioIn` must
  not have to depend on the device layer to name the trait.
- Unit newtypes are **mandatory** across the engine, so their home has to be
  reachable from everywhere. A crate that only needs `Beat` gets `Beat` and
  nothing else.

`tutti-core` re-exports most of this crate, so a consumer already on the engine
root usually needs no direct dependency.

## Example — the measurement vocabulary

Nothing above this crate in the stack, so there is nothing to integrate with:
what a consumer meets first is the units. Cross a family boundary with the
**named converter**, never with arithmetic on the inner float.

```rust
use tutti_types::{Cents, Db, SampleRate, Samples, Seconds, Semitones};

// dB → linear gain. `Db` is logarithmic, so this is a conversion, not a cast.
let trim = Db(-6.0);
assert!((trim.to_amplitude().get() - 0.501_187).abs() < 1e-5);

// Cascaded gain stages ADD in dB — that operator is opted in because it
// means something. Multiplication is NOT, and this is why: scaling the dB
// value squares the amplitude, so `trim * 2.0` would be a quarter of the
// signal, not half of it. The omission ledger withholds `Mul` and points at
// the amplitude domain, which is where a factor of two actually lives.
assert_eq!(trim + trim, Db(-12.0));
let quartered = Db(-12.0).to_amplitude().get();
let squared = trim.to_amplitude().get() * trim.to_amplitude().get();
assert!((quartered - squared).abs() < 1e-6);

// Seconds → frames. The rounding is IN THE NAME because allocating a delay
// line and counting elapsed frames want different answers from one span.
let rate = SampleRate(48_000.0);
let block = Seconds(0.010_5);
assert_eq!(block.to_samples(rate), Samples(504));
assert_eq!(block.to_samples_ceil(rate), Samples(504));

// Pitch offsets convert too — `cents.get() / 100.0` would compile and return
// `Cents`, a value wrong by 100x whose type claims it is fine.
assert_eq!(Cents(1200.0).to_semitones(), Semitones(12.0));
```

## Operators are opt-in, and every omission is deliberate

The **omission ledger** in `value/units.rs`'s tests is the reference for why
`Db * 2.0`, `Beat + Beat` and `Azimuth: Ord` do not exist, and which named
method replaces each. Read it before adding an operator: if the operator is
listed there, the answer is a named method, not an `impl`. An omission ships
with its replacement in the same change — the alternative is call sites escaping
to raw `f32`.

## Features

Both are off by default (`default = []`) — the engine-wide rule that Bevy is
opt-in is stated here rather than inherited, because this is the crate every
other one names.

- `serde` — `Serialize`/`Deserialize` on the wire- and document-carried types:
  `ChannelLayout`, `ChannelTopology`, the meter vocabulary and the units it is
  built from.
- `bevy` — `Reflect` on the units and param vocabulary. Reflection only: no
  `Component`/`Resource` derives, because an app stores a `Volume`, never a bare
  `Db`.

## License

MIT OR Apache-2.0
