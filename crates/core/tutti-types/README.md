# tutti-types

The engine's shared value vocabulary: unit newtypes, RT-callback primitives, and
the `AudioIn`/`AudioOut` I/O traits.

## What this is

The definitions every other Tutti crate shares rather than duplicating. Six
families, one module each:

- **`value`** — the measurement newtypes (`Hz`, `Seconds`, `Db`, `Beat`, `Bpm`,
  `Samples`, …), the `Unit` marker trait, and the atomic `Param<U>` cell that
  carries one.
- **`rt`** — what the audio callback touches: `AudioThreadCell`, the capped
  `RtEventBuf` / `RtVec` / `RtScratch` collections, the `ScopedNoDenormals`
  guard, and `RtPublish`/`RtRef` for handing non-scalar state to the audio
  thread.
- **`channels`** / **`topology`** — `ChannelLayout` (how many) and
  `ChannelTopology` + `Speaker` (which speaker each channel feeds). Distinct
  questions, so distinct types.
- **`io`** — `AudioIn` / `AudioOut` and `pump`: the two traits every audio
  source and sink in the engine speaks.
- **`latency`** / **`tail`** — PDC planning and how long a graph rings, as pure
  graph math over any graph representation.
- **`meter`** — `TimeSignature`, `MeterMap`, and the `Meter` trait that turns a
  `Beat` into a bar and beat.

Everything is re-exported at the crate root, so `tutti_types::Bpm` and
`tutti_types::AudioThreadCell` resolve directly.

## Why it is its own crate

It is the **root leaf** — Bevy-free, engine-free, and depending only on
`smallvec` / `atomic_float` / `arc-swap`. Two consequences make the split
load-bearing:

- The `AudioIn`/`AudioOut` traits are homed here so *any* subsystem can
  implement them without an absurd dependency edge. A sampler implementing
  `AudioIn` must not have to depend on the device layer to name the trait.
- Unit newtypes are **mandatory** across the engine, so their home has to be
  reachable from everywhere. A crate that only needs `Beat` gets `Beat` and
  nothing else.

## Where it sits

Under everything. `tutti-core`, `tutti-mod`, `tutti-units`, `tutti-spatial`,
`tutti-analysis`, `tutti-export`, `tutti-midi-types`, `tutti-plugin-types`, the
format hosts and `fundsp-tutti` all depend on it; it depends on no Tutti crate.
`tutti-core` re-exports most of it, so a consumer already on the engine root
usually needs no direct dependency.

## Features

- `serde` — `Serialize`/`Deserialize` on the wire- and document-carried types:
  `ChannelLayout`, `ChannelTopology`, the meter vocabulary and the units it is
  built from.
- `bevy` — `Reflect` on the units and param vocabulary. Reflection only: no
  `Component`/`Resource` derives, because an app stores a `Volume`, never a bare
  `Db`.

Both are off by default (`default = []`).

## Reading further

The operator **omission ledger** in `value/units.rs`'s tests is the reference
for why `Db * 2.0`, `Beat + Beat` and `Azimuth: Ord` do not exist, and which
named method replaces each. Read it before adding an operator.

## License

MIT OR Apache-2.0
