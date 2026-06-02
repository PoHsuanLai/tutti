# Tutti Plugin

VST2, VST3, and CLAP plugin hosting.

## What this is

Loads audio plugins in separate server processes. Each plugin runs in its own process, so crashes don't affect the main application. Audio buffers are passed via shared memory (mmap).

Uses [vst](https://crates.io/crates/vst) for VST2, raw C pointers for VST3, and [clap-sys](https://crates.io/crates/clap-sys) for CLAP.

## Quick Start

```rust
use tutti_plugin::{PluginClient, BridgeConfig};

// Start plugin server process
let mut client = PluginClient::new(BridgeConfig::default())?;
client.init().await?;

// Load plugin
client.load_plugin("/path/to/plugin.vst3", 44100.0).await?;

// Process audio
let buffer = AudioBuffer { /* ... */ };
client.process(&mut buffer);
```

## How it works

Client-server architecture with IPC. Audio buffers transferred via shared memory. Supports both f32 and f64 sample formats. MIDI events include frame offsets for sample-accurate timing. Transport context (tempo, time signature, position) is passed to plugins.

## License

MIT OR Apache-2.0
