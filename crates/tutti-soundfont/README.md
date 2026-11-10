# tutti-soundfont

SoundFont (`.sf2`) playback for the Tutti audio engine.

The crate has one type, `SoundFontUnit`: a graph node (`tutti_graph::Node`)
with no audio inputs, five outputs and one MIDI event input. Build it from a
decoded `SoundFont` and a `Settings`, choose a preset with
`program_change` (or build both in one call with
`SoundFontUnit::with_preset(font, rate, channel, program)`), insert it into a
graph, and wire a MIDI source (a clip node, a keyboard queue, a hardware
input) to its event input. Each event is
applied at its own frame offset within the block, to a resolution of 8
frames.

The outputs are the dry left and right pair, the chorus send left and right,
and the reverb send (mono). The unit has no effect inside it: the sends are how
much of each voice would go to one, from the font's `chorusEffectsSend` and
`reverbEffectsSend` generators (SF2 2.04 section 8.1.2, generators 15 and 16)
and the channel's CC 93 and CC 91, and a send nothing is sent to is reported
silent (`Status::Masked`). The chorus and reverb are graph nodes
(`tutti_nodes::ModDelayNode` with `ModDelayConfig::GENERAL_MIDI_CHORUS`, and
`tutti_nodes::ReverbNode`), summed with the dry pair by a `ChannelSumNode`;
`tutti-session`'s `Session::add_general_midi` wires the three, and a host that
wants other effects wires the sends itself. `Settings` has no switch for
them: a dry instrument is one whose sends are left unconnected.

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
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};

let mut file = File::open("piano.sf2")?;
let soundfont = Arc::new(SoundFont::new(&mut file)?);

let settings = Settings::new(SampleRate::SR_44K1);
let mut unit = SoundFontUnit::new(soundfont, &settings)?;
unit.program_change(MidiChannel::FIRST, U7::of(0)); // channel 0 → program 0

// Into the graph: no audio input, five outputs, one MIDI event input (wire a
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

The voices are built against one rate and cannot be re-rated. As a
graph node, the unit follows its graph: when the graph is prepared at another
rate, the node's `prepare` (on the control thread) replaces its player
with one built at the new rate over the same shared SoundFont, keeping the
preset. A fork of the graph for an offline export does the same at the
export's rate. A rate outside 16–192 kHz leaves the unit
at its own rate. `SoundFontUnit::with_sample_rate` does the same by hand.

## MIDI timing resolution: 8 frames

An event affects the sample at its offset and none before it, but two offsets
inside the same 8-frame window sound together. A voice computes its
envelopes, LFOs and gains once for a chunk of 8 frames and fills the chunk
whole, so an event cannot take effect part-way into one
(`SYNTH_BLOCK_FRAMES`); `Settings` has no block size.

At 44.1 kHz that is 0.18 ms. An event at offset N (a multiple of 8) produces
exactly the offset-0 render shifted by N frames. The smaller chunk costs about
5 µs per 64-frame block with 8 sustained voices.

## MIDI value resolution: 7 bits

The event input carries UMP, MIDI 1.0 and MIDI 2.0 messages; the player takes
MIDI 1.0 values for its controllers, and keeps a note's velocity at 16 bits only
until it is narrowed for the voice. A MIDI 1.0 message is widened to its MIDI 2.0 form first, and
every MIDI 2.0 value is narrowed with `tutti_midi_protocol::narrow_*`, which
keeps the top bits (a plain right shift, M2-115 section 3.4).

- Applied: note-on, note-off, control change, channel pitch bend, program
  change and its bank (the bank's MSB, as CC 0 sets it), and the registered
  controllers pitch bend sensitivity, fine tuning and coarse tuning (RPN 0, 1
  and 2). A note-on of velocity 0 is a note-off, and a
  nonzero velocity that narrows to 0 sounds at 1.
- Not used: a note-off's release velocity, which a SoundFont does not read.
- Dropped: anything MIDI 1.0 cannot express (per-note pitch bend, per-note
  controllers, per-note management), channel and key pressure, which no generator
  reads, and the assignable controllers (NRPN). An RPN or NRPN arrives
  assembled: the input edge's `tutti_midi_protocol::Translator` turns a MIDI
  1.0 Data Entry run into one Registered or Assignable Controller, so Data
  Entry and the RPN and NRPN selects (CC 6, 38 and 98 to 101) reaching the unit
  as plain control changes change nothing. Every other message type, and a
  packet that does not decode, is not used.

## How a note is decided

The note logic is `tutti-voicing`'s: `Voicing::step` reads each message and
says which voices to start, release or cut, and the unit does it. A note
starts one voice for each pair of a preset zone and an instrument zone whose
key and velocity ranges hold it (SF2 2.04 sections 8.1.2 and 9.4). The rules are
the spec's where it speaks and tutti's where it is silent:

| Rule | Value | From |
|---|---|---|
| polyphony | polyphonic; at least one voice, 64 by default | tutti |
| a key struck again | the earlier strike is released, whatever holds it | tutti (spec silent) |
| a full pool | a released voice gives way first (the quietest of them), then the oldest | tutti (spec silent) |
| exclusive class | ends the other notes of the class in the preset, quickly (10 ms at the longest) | spec, 8.1.2 generator 57 |
| sustain (CC 64) | key-offs are stored and run when it goes inactive | spec, 9.2 |
| sostenuto (CC 66) | holds the notes in key-on when it goes active | spec, 9.2, with MIDI's number |
| all sound off (CC 120) | shuts off at once, whatever the pedals | spec, 9.2 |
| all notes off (CC 123) | releases, pending the pedals | spec, 9.2 |

The spec's section 9.2 lists CC 66 as "soft" and CC 67 as "sostenuto", the
reverse of MIDI 1.0; this player follows MIDI 1.0, which a keyboard sends, and
ignores CC 67. The owner decided this on 2026-11-08.

A release starts at once, from the envelope's value (SF2 9.1.7), so a note
let go in its delay stage, where that value is 0, never sounds: a key struck
again within a millisecond of its first strike leaves the first voice silent.
There is no minimum sounding time.

## Ring-out and reset

The node declares its tail: the longest volume-envelope release of any voice
the font can start, and no more; each effect after it declares its own. An
export that stops at the last note-off extends by the sum along the path.
Once its voices have ended the node returns `Status::Idle`. `reset` cuts every
voice, forgets the pedals and the held keys and returns modulation, expression
and pitch bend to their defaults; the bank, program, volume and pan stay.

## Real-time behaviour

Rendering a block allocates nothing and takes no lock. Scratch buffers are
sized in `prepare` to the graph's largest block, and up to 256 events per
block are applied (any beyond that are dropped). Building a unit, changing
its rate and forking it allocate, and belong on the control thread.

`note_on`, `note_off` and `program_change` act on the player directly. They
take `&mut self` and carry no frame offset, so they are for setting a unit up
before insertion or driving it by hand in tests; once it is in a graph,
send MIDI on its event input.

## Scope

- No asset loading: the crate takes a decoded `SoundFont`. `bevy-tutti`
  provides asset-managed loading for Bevy apps.
- No audio-file playback: that is `tutti-sampler`.
- No MIDI device I/O: that is `tutti-midi-hardware`; the message types are
  `tutti-midi-types`.

A file is decoded by `tutti-sf2` (`tutti_sf2::SoundFont`), which this crate does not re-export: one path for a type. The `tutti` crate re-exports it as `tutti::sf2`.
The `tutti` crate re-exports this one as `tutti::soundfont` behind its
`soundfont` feature, and `bevy-tutti` uses it behind the same feature.

## Features

None.

## License

MIT OR Apache-2.0. The voice render (the oscillator, filters, envelopes, LFOs,
the effects and the generator arithmetic) derives from Nobuaki Tanaka's
[rustysynth](https://github.com/sinshu/rustysynth) (after
[MeltySynth](https://github.com/sinshu/meltysynth)), which is MIT licensed:
each derived file says so in its header, and [the notice](LICENSE-RUSTYSYNTH.txt)
travels with the crate.
