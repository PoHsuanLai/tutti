# vst3-host

[![CI](https://github.com/PoHsuanLai/vst3-host/actions/workflows/ci.yml/badge.svg)](https://github.com/PoHsuanLai/vst3-host/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/vst3-host.svg)](https://crates.io/crates/vst3-host)
[![docs.rs](https://img.shields.io/docsrs/vst3-host)](https://docs.rs/vst3-host)
[![License](https://img.shields.io/crates/l/vst3-host.svg)](LICENSE-MIT)

A pure-Rust library for hosting VST3 audio plugins. No C++ SDK required.

## Features

- **Cross-platform** — macOS, Linux, Windows
- **f32 and f64** audio processing
- **MIDI** — Note On/Off, CC, Pitch Bend, Aftertouch, Program Change
- **Note expression** — Volume, Pan, Tuning, Vibrato, Brightness
- **Parameters** — enumerate, get/set, sample-accurate automation
- **Transport** — tempo, time signature, play/record state, loop points, bar position
- **State** — save/load plugin state
- **GUI** — open/close plugin editor windows via `WindowHandle` + `EditorSize`
- **Host events** — parameter edits, progress reports, unit/program changes via channels

### Host Interfaces

All discoverable by the plugin through `IComponentHandler::queryInterface`:

| Interface | Description |
|-----------|-------------|
| IComponentHandler | Parameter edit notifications from plugin GUI |
| IComponentHandler2 | Grouped edits, dirty state, editor requests |
| IComponentHandler3 | Context menu support |
| IComponentHandlerBusActivation | Bus activation requests |
| IProgress | Progress reporting for long operations (preset loading, scanning) |
| IUnitHandler / IUnitHandler2 | Unit selection and program list change notifications |
| IHostApplication | Host name identification |
| IConnectionPoint | Processor ↔ controller messaging |
| IBStream | State serialization |

## Quick Start

```rust
use std::path::Path;
use vst3_host::{Vst3Instance, AudioBuffer, MidiEvent, TransportState};

let mut plugin = Vst3Instance::load(
    Path::new("/Library/Audio/Plug-Ins/VST3/MyPlugin.vst3"),
    44100.0,  // sample rate
    512,      // block size
)?;

println!("{} by {}", plugin.info().name, plugin.info().vendor);

let inputs: [&[f32]; 2] = [&[0.0; 512], &[0.0; 512]];
let mut out_l = vec![0.0f32; 512];
let mut out_r = vec![0.0f32; 512];
let mut outputs: [&mut [f32]; 2] = [&mut out_l, &mut out_r];
let mut buffer = AudioBuffer::new(&inputs, &mut outputs, 44100.0);

let midi = [MidiEvent::note_on(0, 0, 60, 0.8)];
let transport = TransportState::new().tempo(120.0).playing(true);
let output = plugin.process(&mut buffer, &midi, None, &[], &transport);
// output.midi_events       — MIDI events from the plugin
// output.parameter_changes — output parameter changes
```

## Usage

### Parameters

```rust
// Query
let params = plugin.parameters();
for p in &params {
    println!("{}: {} [{}, {}]", p.id, p.name, p.min_value, p.max_value);
}

// Set (chainable)
plugin
    .set_parameter(0, 0.75)
    .set_parameter(1, 0.5);
```

### Transport

```rust
let transport = TransportState::new()
    .tempo(140.0)
    .playing(true)
    .time_signature(3, 4);
```

### State

```rust
// Save
let state = plugin.state()?;

// Load
plugin.set_state(&state)?;

// Chainable configuration
plugin
    .set_sample_rate(48000.0)
    .set_block_size(256);
plugin.set_use_f64(true)?;
```

### Host Events

```rust
use vst3_host::{ParameterEditEvent, ProgressEvent, UnitEvent};

// Parameter edits from plugin GUI
for event in plugin.poll_param_events() {
    match event {
        ParameterEditEvent::PerformEdit { param_id, value } => { /* ... */ }
        _ => {}
    }
}

// Progress reports (e.g. preset loading)
for event in plugin.poll_progress_events() {
    match event {
        ProgressEvent::Updated { id, progress } => { /* 0.0..1.0 */ }
        _ => {}
    }
}

// Unit/program changes
for event in plugin.poll_unit_events() {
    match event {
        UnitEvent::ProgramListChanged { list_id, program_index } => { /* ... */ }
        _ => {}
    }
}
```

For async integration, use `param_event_receiver()`, `progress_event_receiver()`, or `unit_event_receiver()` to get a `crossbeam_channel::Receiver` directly.

## Plugin Editor

```rust
use vst3_host::WindowHandle;

if plugin.has_editor() {
    // This is the only unsafe boundary in the public API.
    let handle = unsafe { WindowHandle::from_raw(native_view_ptr) };
    let size = plugin.open_editor(handle)?;
    println!("Editor size: {}x{}", size.width, size.height);
}

plugin.close_editor();
```

## Custom MIDI Types

Implement `Vst3MidiEvent` to pass your own event types directly to `process()`:

```rust
use vst3_host::{Vst3MidiEvent, events::*};

struct MyEvent { offset: i32, note: i16, velocity: f32 }

impl Vst3MidiEvent for MyEvent {
    fn sample_offset(&self) -> i32 { self.offset }

    fn to_vst3_event(&self) -> Option<Vst3Event> {
        Some(Vst3Event::NoteOn(NoteOnEvent {
            header: EventHeader {
                bus_index: 0, sample_offset: self.offset,
                ppq_position: 0.0, flags: 0, event_type: K_NOTE_ON_EVENT,
            },
            channel: 0, pitch: self.note, tuning: 0.0,
            velocity: self.velocity, length: 0, note_id: -1,
        }))
    }
}
```

## Platform Support

| Platform | Status |
|----------|--------|
| macOS (aarch64, x86_64) | Tested |
| Linux (x86_64) | Supported |
| Windows (x86_64) | Supported |

## License

MIT OR Apache-2.0
