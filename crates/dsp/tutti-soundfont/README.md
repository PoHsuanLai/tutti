# tutti-soundfont

SoundFont (`.sf2`) synthesis for the Tutti audio engine, via RustySynth.

## What this is

One type: `SoundFontUnit`, a stereo `AudioUnit` with zero inputs and two outputs
— the unit *is* the source, so it enters a `Net` with only its output piped.
Build it with `SoundFontUnit::new` from a decoded `SoundFont` and a
`SynthesizerSettings`, then `program_change` to pick the preset and channel.

**Notes arrive one way: the MIDI inbox.** `midi_sender()` hands back a producer a
control thread pushes to; `midi_port()` is that endpoint as a whole borrow
(routing address, mailbox, and the source-install slot), so a host resolves all
three through one downcast rather than caching one of them — which is how an id
goes stale across a `crossfade` that keeps the graph node but mints a new port.
Events polled from the inbox are applied **sample-accurately** within a block,
each at its own `frame_offset`.

The unit also exposes `note_on(channel, key, velocity)` / `note_off(channel, key)`
as bare MIDI-1 integers, bypassing the inbox and calling RustySynth directly.
They are **not** the intended path and have none of its properties: no
`frame_offset`, so every note lands at the block start; no routing, so a
`MidiBus` cannot reach them; `&mut self`, so they are unreachable once the unit
is in a `Net`. Its peer `tutti-polysynth` exposes no such pair. Use the inbox.

## What it does not own

- **Not a subtractive synth, and not a feature of one.** A `.sf2` player is a
  **peer** of [`tutti-polysynth`](../tutti-polysynth), not a flag on it: this
  unit reaches for none of that crate's voice allocation, tuning, portamento or
  unison — RustySynth owns all of it. What the two share is the *shape*, both
  being `AudioUnit`s with a MIDI inbox, and that comes from `tutti-core` and
  `tutti-midi-runtime`, not from each other. It was split out of the old
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
use tutti_core::dsp::{AudioUnit, Net};
use tutti_core::Arc;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

let mut file = File::open("piano.sf2")?;
let soundfont = Arc::new(SoundFont::new(&mut file)?);

// The rate is fixed here: `set_sample_rate` is a no-op on this unit, so a
// graph at another rate needs a new one rather than a reconfigured one.
let settings = SynthesizerSettings::new(44_100);
let mut unit = SoundFontUnit::new(soundfont, &settings)?;
unit.program_change(0, 0); // channel 0 → preset 0

// The one note path: queue through the inbox, before the unit moves into the
// graph. A control thread may keep pushing through this sender afterwards.
let sender = unit.midi_sender();
sender.queue(&[MidiEvent::note_on(
    MidiGroup::FIRST,
    MidiChannel::FIRST,
    60,
    0x8000,
)]);

let mut net = Net::new(0, 2);
let node = net.push(Box::new(unit));
net.pipe_output(node);
net.check();

let mut out = [0.0f32; 2];
net.tick(&[], &mut out);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Constraint: the sample rate is fixed for the unit's lifetime

The rate is set once from `SynthesizerSettings::sample_rate` and cannot change:
RustySynth builds its voice tables against a rate at construction and offers no
way to re-rate them, so `AudioUnit::set_sample_rate` is a deliberate no-op here
rather than a missing implementation.

**The graph will not complain.** A unit built at 44.1 kHz and run in a 48 kHz
graph keeps rendering — every note simply plays at the wrong pitch and tempo,
with no error at any layer. A rate change means constructing a new unit and
swapping it into the graph.

## Constraint: MIDI resolution stops at 7 bits

The inbox speaks MIDI 2.0 (UMP), RustySynth speaks MIDI 1.0 wire format, so every
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

None. The crate **is** the SoundFont unit — RustySynth and the MIDI inbox are
both load-bearing, and gating either leaves a `SoundFontUnit` that cannot be
built or cannot receive notes.

## License

MIT OR Apache-2.0
