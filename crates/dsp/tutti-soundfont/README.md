# tutti-soundfont

SoundFont (`.sf2`) playback for the Tutti audio engine, via RustySynth.

The crate has one type, `SoundFontUnit`: a graph node (`tutti_graph::Node`)
with no audio inputs, stereo out and one MIDI event input. Build it from a
decoded `SoundFont` and a `SynthesizerSettings`, choose a preset with
`program_change`, insert it into a graph, and wire a MIDI source (a clip
node, a keyboard queue, a hardware input) to its event input. Each event is
applied at its own frame offset within the block, to a resolution of 8
frames.

Use it when you want General MIDI or sampled-instrument playback from a
SoundFont file. For a subtractive synthesizer, use `tutti-polysynth`; the two
are separate peers that share only the graph-node shape.

## Quick start

This needs a `.sf2` on disk, so it is `no_run`.

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

## Sample rate

RustySynth builds its voice tables for one rate and cannot re-rate them. As a
graph node, the unit follows its graph: when the graph is prepared at another
rate, the node's `prepare` (on the control thread) replaces its synthesizer
with one built at the new rate over the same shared SoundFont, keeping the
preset. A fork of the graph for an offline export does the same at the
export's rate. A rate RustySynth refuses (outside 16–192 kHz) leaves the unit
at its own rate. `SoundFontUnit::with_sample_rate` does the same by hand.

## MIDI timing resolution: 8 frames

An event affects the sample at its offset and none before it, but two offsets
inside the same 8-frame window sound together. RustySynth renders voices in
internal chunks of `block_size` frames and fills each chunk whole, so an event
cannot take effect part-way into one. This crate always builds the
synthesizer at a `block_size` of 8, the smallest RustySynth accepts
(`SYNTH_BLOCK_FRAMES`), and ignores the `block_size` a caller passes; every
other `SynthesizerSettings` field is used as given.

At 44.1 kHz that is 0.18 ms. An event at offset N (a multiple of 8) produces
exactly the offset-0 render shifted by N frames. The smaller chunk costs about
5 µs per 64-frame block with 8 sustained voices.

## MIDI value resolution: 7 bits

The event input carries MIDI 2.0 (UMP); RustySynth takes MIDI 1.0. Values are
narrowed with the MIDI 2.0 spec's Min-Center-Max converters.

- Applied: note-on, note-off, control change, channel pitch bend, program
  change.
- Dropped: anything MIDI 1.0 cannot express (per-note pitch bend, per-note
  controllers, per-note management) and channel and key pressure, which
  RustySynth has no setter for. 16-bit velocity and 32-bit controller values
  are reduced to 7 bits.

## Real-time behaviour

Rendering a block allocates nothing and takes no lock. Scratch buffers are
sized in `prepare` to the graph's largest block, and up to 256 events per
block are applied (any beyond that are dropped). Building a unit, changing
its rate and forking it allocate, and belong on the control thread.

`note_on`, `note_off` and `program_change` call RustySynth directly. They
take `&mut self` and carry no frame offset, so they are for setting a unit up
before insertion or driving it by hand in tests; once it is in a graph,
send MIDI on its event input.

## Scope

- No asset loading: the crate takes a decoded `SoundFont`. `bevy-tutti`
  provides asset-managed loading for Bevy apps.
- No audio-file playback: that is `tutti-sampler`.
- No MIDI device I/O: that is `tutti-midi-hardware`; the message types are
  `tutti-midi-types`.

`SoundFont`, `SoundFontError` and `SynthesizerSettings` are re-exported from
RustySynth, so decoding a file needs no direct `rustysynth` dependency.
The `tutti` crate re-exports this one as `tutti::soundfont` behind its
`soundfont` feature, and `bevy-tutti` uses it behind the same feature.

## Features

None.

## License

MIT OR Apache-2.0
