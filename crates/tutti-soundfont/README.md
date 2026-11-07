# tutti-soundfont

SoundFont (`.sf2`) playback for the Tutti audio engine, via RustySynth.

The crate has one type, `SoundFontUnit`: a graph node (`tutti_graph::Node`)
with no audio inputs, stereo out and one MIDI event input. Build it from a
decoded `SoundFont` and a `Settings`, choose a preset with
`program_change` (or build both in one call with
`SoundFontUnit::with_preset(font, rate, channel, program)`), insert it into a
graph, and wire a MIDI source (a clip node, a keyboard queue, a hardware
input) to its event input. Each event is
applied at its own frame offset within the block, to a resolution of 8
frames.

Use it when you want General MIDI or sampled-instrument playback from a
SoundFont file. For a subtractive synthesizer, use `tutti-polysynth`; the two
are separate peers that share only the graph-node shape.

## Quick start

This needs a `.sf2` on disk, so it is `no_run`.

```rust,no_run
use std::fs::File;
use tutti_types::graph::{OutPort, Source};
use std::sync::Arc;
use tutti_types::{NodeKey, SampleRate, Samples};
use tutti_graph::{Editor, Prepare};
use tutti_ump::{MidiChannel, U7};
use tutti_soundfont::{Settings, SoundFont, SoundFontUnit};

let mut file = File::open("piano.sf2")?;
let soundfont = Arc::new(SoundFont::new(&mut file)?);

let settings = Settings::new(SampleRate::SR_44K1);
let mut unit = SoundFontUnit::new(soundfont, &settings)?;
unit.program_change(MidiChannel::FIRST, U7::new(0).unwrap()); // channel 0 → program 0

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
(`SYNTH_BLOCK_FRAMES`), and `Settings` has no `block_size`.

At 44.1 kHz that is 0.18 ms. An event at offset N (a multiple of 8) produces
exactly the offset-0 render shifted by N frames. The smaller chunk costs about
5 µs per 64-frame block with 8 sustained voices.

## MIDI value resolution: 7 bits

The event input carries UMP, MIDI 1.0 and MIDI 2.0 messages; RustySynth takes
MIDI 1.0 values. A MIDI 1.0 message is widened to its MIDI 2.0 form first, and
every MIDI 2.0 value is narrowed with `tutti_midi_protocol::narrow_*`, which
keeps the top bits (a plain right shift, M2-115 section 3.4).

- Applied: note-on, note-off, control change, channel pitch bend, program
  change (the program only). A note-on of velocity 0 is a note-off, and a
  nonzero velocity that narrows to 0 sounds at 1.
- Not used: a note-off's release velocity. RustySynth's `note_off` takes only
  a channel and a key.
- Dropped: anything MIDI 1.0 cannot express (per-note pitch bend, per-note
  controllers, per-note management), channel and key pressure, which RustySynth
  has no setter for, native RPN and NRPN messages, and a program change's bank.
  A MIDI 1.0 control change run for an RPN or NRPN still reaches RustySynth as
  plain control changes. Every other message type, and a packet that does not
  decode, is not used.

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

`SoundFont` and `SoundFontError` are re-exported from `tutti-sf2`, so decoding a file needs no direct dependency on it.
The `tutti` crate re-exports this one as `tutti::soundfont` behind its
`soundfont` feature, and `bevy-tutti` uses it behind the same feature.

## Features

None.

## License

MIT OR Apache-2.0
