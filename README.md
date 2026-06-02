<div align="center">
  <img src="logo/logo.png" alt="Tutti Logo" width="200"/>
</div>

# Tutti

[![Crates.io](https://img.shields.io/crates/v/tutti.svg)](https://crates.io/crates/tutti)
[![Documentation](https://docs.rs/tutti/badge.svg)](https://docs.rs/tutti)
[![License](https://img.shields.io/crates/l/tutti.svg)](https://github.com/PoHsuanLai/Tutti#license)
[![CI](https://github.com/PoHsuanLai/Tutti/workflows/CI/badge.svg)](https://github.com/PoHsuanLai/Tutti/actions)

A real-time audio engine for DAW applications in Rust. Tutti provides an audio graph runtime, MIDI processing, sample playback, and plugin hosting.

For audio UI components, see [Armas](https://github.com/PoHsuanLai/Armas).

## Design

- **Audio graph**: Uses FunDSP's `Net` with macros (`chain!`, `mix!`, `stack!`)
- **Lock-free audio**: No allocations or mutexes in audio callback
- **Feature gates**: Only compile what you need (core ~500KB)
- **Flat handle bundle**: Engine returns owned fields you destructure — no god-object

## Overview

Umbrella crate that coordinates multiple audio subsystems:

- **[tutti-core]** - Audio graph runtime (Net, Transport, Metering, PDC, MIDI routing)
- **[tutti-midi-io]** - MIDI I/O subsystem (Hardware I/O, ports, MPE, MIDI 2.0, CC mapping)
- **[tutti-sampler]** - Sample playback (Butler, streaming, recording, time-stretch)
- **[tutti-units]** - Built-in AudioUnits (LFO, filters, delays, dynamics, modulation, spatial)
- **[tutti-plugin]** - Plugin hosting (VST2, VST3, CLAP)
- **[tutti-analysis]** - Audio analysis (waveform, transient, pitch, correlation)
- **[tutti-export]** - Offline rendering and export

## Quick Start

`TuttiEngine` is a flat bundle of owned subsystems returned from the
builder. Destructure it (or address fields directly) and edit `&mut TuttiGraph`
explicitly — no god-object surface.

```rust
use tutti::prelude::*;

// Sample rate is dictated by the audio device.
let mut engine = TuttiEngine::builder().build()?;

// Stage graph edits, then `commit()` once to publish to the audio thread.
let osc    = engine.graph.add(sine_hz::<f32>(440.0));
let filter = engine.graph.add(lowpass_hz::<f32>(2000.0, 1.0));
engine.graph.pipe_all(osc, filter);
engine.graph.pipe_output(filter);
engine.graph.commit();

// Handles are cheap to clone and lock-free.
engine.transport.play();
```

For full builder examples (sf2, wav, vst3), see the crate-level
documentation on [docs.rs/tutti](https://docs.rs/tutti).

## Features

- `default` - Core audio engine only
- `full` - Everything enabled
- `midi` - MIDI subsystem
- `sampler` - Sample playback and recording
- `soundfont` - SoundFont support (requires `sampler`)
- `plugin` - Plugin hosting (VST2/VST3/CLAP)
- `analysis` - Audio analysis tools
- `export` - Offline rendering
- `spatial-audio` - VBAP and binaural panning

## Architecture

Each subsystem is an independent crate. TuttiEngine provides fluent handles to coordinate them.


## Examples

### Loading and Instantiating Nodes

```rust
use tutti::prelude::*;

// Build engine - subsystems enabled via Cargo features
let engine = TuttiEngine::builder()
    .sample_rate(44100.0)
    .build()?;

// Load nodes once (explicit format methods = compile-time type safety)
engine.load_vst3("reverb", "plugin.vst3")?;     // VST3 plugin
engine.load_wav("kick", "kick.wav")?;           // WAV sample

// Add custom DSP nodes programmatically
engine.add_node("my_filter", |params| {
    let cutoff: f32 = get_param_or(params, "cutoff", 1000.0);
    Ok(Box::new(lowpass_hz(cutoff)))
});

// Instantiate nodes (create instances and add to graph)
let synth = engine.create("my_synth", &params! {})?;
let reverb = engine.create("reverb", &params! { "room_size" => 0.9 })?;
let filter = engine.create("my_filter", &params! { "cutoff" => 2000.0 })?;

// Build graph with node IDs
engine.graph_mut(|net| {
    chain!(net, synth, filter, reverb => output);
});
```

### Transport Control (Fluent API)

```rust
use tutti::prelude::*;

let engine = TuttiEngine::builder().build()?;

// Fluent transport API - chainable methods
engine.transport()
    .tempo(128.0)
    .loop_range(0.0, 16.0)
    .enable_loop()
    .play();

// Metronome control
engine.transport()
    .metronome()
    .volume(0.7)
    .accent_every(4)
    .always();

// State queries
let transport = engine.transport();
if transport.is_playing() {
    let beat = transport.current_beat();
    println!("Currently at beat: {}", beat);
}

// Seek and play
transport.seek_and_play(8.0);

// Transport modes
transport.fast_forward();
transport.rewind();
transport.stop();
```

### Streaming and Recording (Butler Thread)

```rust
use tutti::prelude::*;

// Sampler subsystem automatically enabled when 'sampler' feature is compiled
let engine = TuttiEngine::builder().build()?;

let sampler = engine.sampler();

// Stream large files from disk (no memory loading)
sampler.stream("huge_audio_file.wav")
    .channel(0)
    .gain(0.8)
    .speed(1.5)
    .start_sample(44100)  // Start at 1 second
    .start();

// Record audio with ring buffer
let session = sampler.record("recording.wav")
    .channels(2)
    .buffer_seconds(5.0)
    .start();

// Audio callback writes to session.producer

// Stop and flush to disk
sampler.stop_capture(session.id);
sampler.flush_capture(session.id, "final.wav");
```

### Loading and Exporting

```rust
use tutti::prelude::*;

let engine = TuttiEngine::builder().build()?;

// Load audio files
engine.load_wav("kick", "kick.wav")?;
engine.load_flac("snare", "snare.flac")?;

// Instantiate and use in graph
let kick = engine.create("kick", &params! {})?;
let snare = engine.create("snare", &params! {})?;

engine.graph_mut(|net| {
    let mix = mix!(net, kick, snare);
    net.pipe_output(mix);
});

// Export to file
engine.export()
    .duration_seconds(10.0)
    .format(AudioFormat::Flac)
    .normalize(NormalizationMode::lufs(-14.0))
    .to_file("output.flac")?;
```

### MIDI I/O (Fluent API)

```rust
use tutti::prelude::*;

// MIDI subsystem automatically enabled when 'midi' feature is compiled
// Enable it in Cargo.toml: tutti = { version = "...", features = ["midi"] }
let engine = TuttiEngine::builder()
    .midi()  // Opt-in to connect MIDI hardware
    .build()?;

let midi = engine.midi();

// Connect to hardware
midi.connect_device_by_name("Keyboard")?;

// Fluent MIDI output (chainable)
midi.send()
    .note_on(0, 60, 100)
    .cc(0, 74, 64)
    .pitch_bend(0, 0);

// Or single messages
midi.send().note_on(0, 60, 100);
```

### With Multiple Subsystems

```rust
use tutti::prelude::*;

// Enable features in Cargo.toml:
// tutti = { version = "...", features = ["midi", "sampler"] }

let engine = TuttiEngine::builder()
    .midi()  // Opt-in to connect MIDI hardware
    .build()?;

engine.midi().send().note_on(0, 60, 100);

let sampler = engine.sampler();
sampler.stream("file.wav").start();
```

### Using Individual Crates

You can use vocabulary types from the sub-crates directly when building a
custom audio stack:

```rust
// Build a DSP graph with the core vocabulary — bring your own callback.
use tutti_core::{TuttiNet, TransportManager};
use std::sync::Arc;

let mut net = TuttiNet::new(0, 2);
let transport = Arc::new(TransportManager::new(48_000.0));
// ... push nodes, wire outputs, then `net.backend()` for a live backend ...
```

```rust
// Just MIDI
use tutti_midi_types::MidiSystem;

let midi = MidiSystem::new().build()?;
```

## Testing

See [TESTING.md](TESTING.md) for setup instructions.

Quick examples:

```bash
# Plugin loading (see example docs for setup)
cargo run --example plugin_loading --features plugin

# MIDI synthesizer
cargo run --example midi_synth --features "midi,synth"
```

## License

MIT OR Apache-2.0

[tutti-core]: https://crates.io/crates/tutti-core
[tutti-midi-io]: https://crates.io/crates/tutti-midi-io
[tutti-sampler]: https://crates.io/crates/tutti-sampler
[tutti-units]: https://crates.io/crates/tutti-units
[tutti-plugin]: https://crates.io/crates/tutti-plugin
[tutti-analysis]: https://crates.io/crates/tutti-analysis
[tutti-export]: https://crates.io/crates/tutti-export
