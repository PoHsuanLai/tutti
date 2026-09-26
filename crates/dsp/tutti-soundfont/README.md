# tutti-soundfont

SoundFont (`.sf2`) synthesis for the Tutti audio engine, via RustySynth.

## What this is

One type: `SoundFontUnit`, a stereo graph node with zero audio inputs and two
outputs — the unit *is* the source. Build it with `SoundFontUnit::new` from a
decoded `SoundFont` and a `SynthesizerSettings`, then `program_change` to pick
the preset and channel.

**Notes arrive on its MIDI event input.** It is a `tutti_graph::Node` with one
MIDI event input (a clip node, a keyboard's queue, the hardware input wire to
it). Events are applied **at their own offset** within a block, to a
resolution of **8 frames** — see the timing-resolution section below for what
that floor is and where it comes from. A test or bench hands it events the
way a graph does (`tutti_graph::contract::{drive_in, Direct}`).

The unit also exposes `note_on(channel, key, velocity)` / `note_off(channel, key)`
as bare MIDI-1 integers, calling RustySynth directly. They are **not** the
intended path: no `frame_offset`, so every note lands at the block start, and
`&mut self`, so they are unreachable once the unit is in a graph. Its peer
`tutti-polysynth` exposes no such pair.

## What it does not own

- **Not a subtractive synth, and not a feature of one.** A `.sf2` player is a
  **peer** of [`tutti-polysynth`](../tutti-polysynth), not a flag on it: this
  unit reaches for none of that crate's voice allocation, tuning, portamento or
  unison — RustySynth owns all of it. What the two share is the *shape*, both
  being graph nodes with a MIDI event input, and that comes from `tutti-graph`,
  not from each other. It was split out of the old
  `tutti-synth` for exactly this reason: the `soundfont` feature there was a
  dependency edge wearing a feature's clothes.
- **No asset loading.** This crate takes a *decoded* `SoundFont`. A host that
  wants asset-managed loading wires it in its own adapter layer; `bevy-tutti` is
  that adapter for a Bevy host.
- **No sample playback from files.** Clip and timeline playback is
  [`tutti-sampler`](../tutti-sampler)'s.
- **No MIDI I/O.** Ports are `tutti-midi-hardware`'s, the wire vocabulary
  `tutti-midi-types`'.

## Quick start

Every path here needs a real `.sf2` on disk and the crate ships no fixture, so
this is `no_run` — it is still type-checked, and a wrong method name fails the
build.

```rust,no_run
use std::fs::File;
use tutti_core::graph::{OutPort, Source};
use tutti_core::{Arc, NodeKey, SampleRate, Samples};
use tutti_graph::{Editor, Prepare};
use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

let mut file = File::open("piano.sf2")?;
let soundfont = Arc::new(SoundFont::new(&mut file)?);

let settings = SynthesizerSettings::new(44_100);
let mut unit = SoundFontUnit::new(soundfont, &settings)?;
unit.program_change(0, 0); // channel 0 → preset 0

// Into the graph: no audio input, stereo out, one MIDI event input (wire a
// clip or a keyboard's queue node to it with `GraphSpec::connect_events`).
// The graph runs at 48 kHz, so `prepare` rebuilds the synthesizer at 48 kHz,
// keeping the preset.
let (mut editor, _executor) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
let key = NodeKey(1);
editor.insert(key, "piano", unit);
editor.spec_mut().topology.outputs = (0..2)
    .map(|port| Source::Node(OutPort { node: key, port }))
    .collect();
editor.commit()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Constraint: a synthesizer's rate is fixed; the node follows its graph

RustySynth builds its voice tables against a rate at construction and offers no
way to re-rate them, so a `SoundFontUnit` renders at the rate its synthesizer
was built at. As a graph node it **follows its graph's rate**: `prepare` (on
the control thread) swaps in `with_sample_rate`, a new synthesizer over the
same SoundFont (shared) and preset, when the graph runs at another rate. So
does a fork for an export, rendered at the export's rate. A rate RustySynth
refuses (outside 16–192 kHz) leaves the unit at its own.

## Constraint: MIDI timing resolution stops at 8 frames

A block is split at every event's offset — render the frames before the
offset, apply the event, carry on — so an event affects the
sample at its offset and no sample before it. What it cannot do is resolve two
offsets that fall inside the same 8-frame window.

The floor is RustySynth's. `Synthesizer::render` accepts a buffer of any length,
but serves those frames out of an internal `block_size` chunk that `render_block`
fills *whole*: voices render a chunk at a time and mix gains ramp across it, so
a note applied part-way into an already-rendered chunk cannot affect it.
`block_size` is therefore the resolution floor, and 8 is the smallest RustySynth
accepts — `SynthesizerSettings::check_block_size` rejects anything outside
`8..=1024`. This crate builds every unit at 8 and **overrides whatever
`block_size` a caller passes**; every other field of `SynthesizerSettings` is
honoured.

At 44.1 kHz that is 0.18 ms. Offsets 0, 8, 16, … resolve distinctly, and an
event at offset N produces exactly the offset-0 render shifted by N frames.
Offsets 16 and 20 do not resolve apart. Finer than that needs a change inside
the vendored synthesizer.

It costs about 5 µs per 64-frame block (measured, release, 8 sustained voices:
5.8 µs at `block_size` 64 against 10.7 µs at 8 — 0.40% to 0.74% of the real-time
budget), and changes the rendered audio by at most ~5% of signal RMS, from finer
gain-ramp granularity rather than any algorithm change.

## Constraint: MIDI resolution stops at 7 bits

The event input speaks MIDI 2.0 (UMP), RustySynth speaks MIDI 1.0 wire format, so every
value downscales through the spec's Min-Center-Max converters. Anything MIDI 2.0
expresses that MIDI 1.0 cannot — per-note pitch bend, per-note controllers,
16-bit velocity, 32-bit CC precision — is **dropped, not approximated**. Channel
and key pressure arrive as well-formed UMP but RustySynth exposes no setter for
them, so they are dropped too.

Translated: note-on / note-off, control change, channel pitch bend, program
change.

## Where it sits

Depends on `tutti-core` (with `midi`), `tutti-midi-types`, `tutti-midi-runtime`,
and the vendored `rustysynth-tutti`. Only `bevy-tutti` depends on it. `SoundFont`,
`SoundFontError` and `SynthesizerSettings` are re-exported from the crate root,
so a consumer needs no direct `rustysynth` dependency to decode a file.

## Features

None. The crate **is** the SoundFont unit — RustySynth and its MIDI input are
both load-bearing, and gating either leaves a `SoundFontUnit` that cannot be
built or cannot receive notes.

## License

MIT OR Apache-2.0
