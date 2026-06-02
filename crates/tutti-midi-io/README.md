# Tutti MIDI

MIDI I/O, MPE, MIDI 2.0, and CC mapping.

## What this is

MIDI processing for DAW applications. Virtual port routing, hardware device I/O, MIDI Polyphonic Expression (MPE), MIDI 2.0 high-resolution messages, CC-to-parameter mapping with MIDI learn, and MIDI output collection from audio nodes.

Uses [midir](https://crates.io/crates/midir) for hardware I/O and [midi-msg](https://crates.io/crates/midi-msg) for message parsing.

## Quick Start

```rust
use tutti_midi_types::MidiSystem;

// Basic system
let midi = MidiSystem::builder().build()?;

// Send MIDI event
midi.send_note_on(0, 60, 100)?;
```

## Features

- `midi-io` - Hardware device I/O (midir)
- `mpe` - MIDI Polyphonic Expression
- `midi2` - MIDI 2.0 support

## License

MIT OR Apache-2.0
