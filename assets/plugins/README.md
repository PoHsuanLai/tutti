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

The `plugin_loading.rs` example will:
1. Scan for plugins in this directory
2. Fall back to system plugin directories
3. Look up a plugin with `Plugins::find` / `Plugins::load_by_name`
4. Insert the loaded `PluginClient` into the graph

## Note

Plugins are not included in the repository. Download them separately from the links above.
All recommended plugins are free and open source (or freeware).
