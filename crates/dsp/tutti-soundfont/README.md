# tutti-soundfont

SoundFont (`.sf2`) synthesis for Tutti, via RustySynth.

## What this is

One type: `SoundFontUnit`, a stereo `AudioUnit` with zero inputs and two
outputs. Build it with `SoundFontUnit::new` from a decoded `SoundFont` and a
`SynthesizerSettings`, then `program_change` to pick the preset and channel.
Notes arrive through the unit's `MidiInPort` and are applied sample-accurately
within a block; `note_on` / `note_off` are also available directly, bypassing
the inbox.

A host that wants asset-managed loading wires it in its own adapter layer — this
crate only needs the decoded `SoundFont`. `bevy-tutti` is that adapter for a
Bevy host.

## Why it is its own crate

**A `.sf2` player is a peer of [`tutti-polysynth`](../tutti-polysynth), not a
feature of it.** The two share no code: this unit reaches for none of that
crate's voice allocation, tuning, portamento or unison — RustySynth owns all of
it. What they share is the *shape*, both being `AudioUnit`s with a `MidiInPort`,
and that comes from `tutti-core` and `tutti-midi-runtime`, not from each other.
It was split out of the old `tutti-synth` for exactly this reason: the
`soundfont` feature there was a dependency edge wearing a feature's clothes.

## Where it sits

Depends on `tutti-core` (with `midi`), `tutti-midi-types`,
`tutti-midi-runtime`, and the vendored `rustysynth-tutti`. Only `bevy-tutti`
depends on it. `SoundFont`, `SoundFontError` and `SynthesizerSettings` are
re-exported from the crate root, so a consumer does not need a direct
`rustysynth` dependency to decode a file.

## Features

None. The crate **is** the SoundFont unit — RustySynth and the MIDI inbox are
both load-bearing, and gating either leaves a `SoundFontUnit` that cannot be
built or cannot receive notes.

## Two constraints worth knowing

- **The sample rate is fixed at construction**, taken from
  `settings.sample_rate`. RustySynth cannot be re-rated afterwards, so
  `AudioUnit::set_sample_rate` is a no-op on this unit — a graph running at a
  different rate needs a new unit, not a reconfigured one.
- **MIDI resolution stops at 7 bits.** The inbox speaks MIDI 2.0 (UMP),
  RustySynth speaks MIDI 1.0 wire format, so every value downscales through the
  spec's Min-Center-Max converters. Anything MIDI 2.0 expresses that MIDI 1.0
  cannot — per-note pitch bend, per-note controllers, 16-bit velocity — is
  **dropped, not approximated**.

## License

MIT OR Apache-2.0
