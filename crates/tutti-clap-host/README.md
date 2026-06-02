# clap-host

[![CI](https://github.com/PoHsuanLai/clap-host/actions/workflows/ci.yml/badge.svg)](https://github.com/PoHsuanLai/clap-host/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/clap-host.svg)](https://crates.io/crates/clap-host)
[![docs.rs](https://docs.rs/clap-host/badge.svg)](https://docs.rs/clap-host)
[![License](https://img.shields.io/crates/l/clap-host.svg)](LICENSE)

A safe Rust library for hosting [CLAP](https://cleveraudio.org/) audio plugins.

## Features

- **Cross-platform** — macOS, Linux, Windows
- **f32 and f64** audio processing
- **MIDI** — note on/off, CC, pitch bend, program change, poly pressure, sysex
- **Note expression** — MPE-style per-note volume, pan, tuning, vibrato, brightness
- **Parameters** — enumerate, get/set, sample-accurate automation
- **Transport** — tempo, time signature, play/record state, loop points, bar position
- **State** — save/load plugin state with optional context (preset, project, duplicate)
- **GUI** — open/close plugin editor windows via `WindowHandle` + `EditorSize`
- **30+ extensions** — audio ports, note ports, ambisonic, surround, voice info, undo, triggers, tuning, remote controls, context menus, and more

## Quick Start

```rust
use clap_host::{ClapInstance, MidiEvent, ProcessContext, TransportInfo};

let mut plugin = ClapInstance::load("/path/to/plugin.clap", 44100.0, 512)?;
println!("{} by {}", plugin.info().name, plugin.info().vendor);

let midi = [MidiEvent::note_on(0, 0, 60, 100)];
let transport = TransportInfo::new().with_tempo(128.0).with_playing(true);
let output = plugin.process(&mut buffer, &ProcessContext {
    midi: &midi,
    transport: Some(&transport),
    ..Default::default()
})?;
// output.midi_events       — MIDI events from the plugin
// output.param_changes     — output parameter changes
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
let transport = TransportInfo::new()
    .with_tempo(140.0)
    .with_playing(true)
    .with_time_signature(3, 4)
    .with_position(8.0, 3.43)
    .with_loop(true, 4.0, 16.0);
```

### State

```rust
// Save
let state = plugin.state()?;

// Load
plugin.set_state(&state)?;

// With context
use clap_host::StateContext;
let preset = plugin.state_with_context(StateContext::ForPreset)?;
```

### Note Expression

```rust
use clap_host::{NoteExpressionValue, NoteExpressionType};

let expr = NoteExpressionValue::new(NoteExpressionType::Tuning, /*note_id*/ 0, 0.5)
    .at(128)        // sample offset
    .on_channel(0);
```

### Event Lists

```rust
use clap_host::InputEventList;

let mut events = InputEventList::new();
events
    .add_midi_events(&midi)
    .add_param_changes(&param_changes)
    .add_note_expressions(&expressions)
    .sort_by_time();
```

## Plugin Editor

```rust
use clap_host::WindowHandle;

if plugin.has_editor() {
    // This is the only unsafe boundary in the public API.
    let handle = unsafe { WindowHandle::from_raw(native_view_ptr) };
    let size = plugin.open_editor(handle)?;
    println!("Editor size: {}x{}", size.width, size.height);
}

plugin.close_editor();
```

## Custom MIDI Types

Implement `ClapMidiEvent` to pass your own event types directly to `process()`:

```rust
use clap_host::{ClapMidiEvent, ClapNoteEvent};

struct MyEvent { offset: i32, note: u8, velocity: f64 }

impl ClapMidiEvent for MyEvent {
    fn to_clap_events(&self) -> Vec<ClapNoteEvent> { /* ... */ }
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
