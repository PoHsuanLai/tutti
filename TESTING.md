# Testing Tutti

Quick guide for testing plugins.

## Quick Start

### Test Plugins

1. Download a free plugin:
   - [Dragonfly Room Reverb](https://github.com/michaelwillis/dragonfly-reverb/releases) (recommended, ~5MB)
   - [Surge XT](https://github.com/surge-synthesizer/releases-xt/releases) (~50MB)

2. Place the .vst3 file in `assets/plugins/`

3. Run example:
   ```bash
   cargo run --example plugin_loading --features plugin
   ```

## What Gets Tested

### Plugin System
- VST3 and CLAP plugin loading via `tutti_plugin::Plugins`
- Plugin scanning (assets dir + system directories)
- Loading a named plugin with `Plugins::load_by_name` and inserting into the graph
- Audio routing through plugins

## Examples

### `plugin_loading.rs`
Tests plugin hosting by:
- Scanning for plugins in `assets/plugins/` and system directories
- Creating a sine wave → reverb plugin → output chain
- Falls back to built-in reverb if no plugins found

## Requirements

### Plugins
- None (downloads are manual)
- Recommended: Dragonfly Room Reverb

## File Structure

```
tutti/
├── assets/
│   └── plugins/                  # VST3/CLAP plugins (manual download)
│       └── README.md
└── examples/
    └── plugin_loading.rs
```

## Troubleshooting

**"No plugins found"**
- Download plugins from the links in the example docs
- Place `.vst3` files in `assets/plugins/`
- The example will auto-detect plugins in that directory
