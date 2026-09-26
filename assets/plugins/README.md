# Plugin Assets Directory

This directory is for VST3/CLAP plugins used in examples and tests.

## Recommended Free Plugins for Testing

### Reverb
- **Dragonfly Room Reverb** (VST3, CLAP)
  - Download: https://github.com/michaelwillis/dragonfly-reverb/releases
  - License: GPL-3.0
  - Cross-platform (macOS, Windows, Linux)

### Delay
- **CloudReverb** (VST3)
  - Download: https://github.com/xunil-cloud/CloudReverb
  - License: GPL-3.0
  - Includes delay effects

### Synthesizer
- **Surge XT** (VST3, CLAP)
  - Download: https://surge-synthesizer.github.io/
  - License: GPL-3.0
  - Professional-quality open source synth

### Compressor/Dynamics
- **OTT** by Xfer Records (VST3)
  - Download: https://xferrecords.com/freeware
  - License: Freeware
  - Popular multiband compressor

## Directory Structure

Place plugins in this directory for examples:

```
assets/plugins/
├── DragonflyRoomReverb.vst3/
├── Surge XT.vst3/
└── OTT.vst3/
```

## Usage

`tutti-plugin`'s `wire_all_inputs` example takes a path directly:

```sh
cargo run -p tutti-plugin --example wire_all_inputs -- <path-to-plugin>
```

It opens the plugin with `Plugin::open`, offers it every per-block input
(MIDI, transport, chord/scale), reports which the plugin accepted, and hands
the audio node over as a graph node (`Plugin` is a `tutti_graph::IntoNode`).

To go through a catalog instead — scanning directories, and honouring the
blacklist a crashed scan recorded — use `Plugins::find` to get a path and
`Plugins::open` to load it.

## Note

Plugins are not included in the repository. Download them separately from the links above.
All recommended plugins are free and open source (or freeware).
