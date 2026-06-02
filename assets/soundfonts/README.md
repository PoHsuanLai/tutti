# SoundFont Assets

RustySynth is a SoundFont **player** - you need to provide a .sf2 file.

## Quick Start

```bash
cd assets/soundfonts
./download-timgm6mb.sh
cd ../..
cargo run --example soundfont --features soundfont,midi
```

## What Gets Downloaded

- **TimGM6mb.sf2** (5.7 MB)
- Source: Debian package archive
- License: GNU GPL
- General MIDI compatible soundfont

The soundfont example will play a melody and chord using the piano preset.
