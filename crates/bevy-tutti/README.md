# bevy-tutti

Bevy plugin for the [Tutti](https://github.com/PoHsuanLai/tutti) audio engine. Exposes Tutti's real-time audio graph, transport, MIDI, plugin hosting, and DSP as Bevy ECS components, resources, and systems.

## What is this?

[Tutti](https://github.com/PoHsuanLai/tutti) is a real-time, lock-free audio engine built in Rust for DAW and interactive audio applications. It handles synthesis, sample playback, MIDI, plugin hosting (VST3/VST2/CLAP), recording, automation, spatial audio, and offline export.

**bevy-tutti** bridges Tutti into the Bevy ECS. Instead of managing audio lifetimes and callbacks manually, you spawn entities with trigger components and let systems handle the rest. The plugin also syncs engine state (transport, metering, device info) into Bevy resources every frame, so your UI and game logic can read audio state without touching the audio thread.

## Quick start

```rust
use bevy::prelude::*;
use bevy_tutti::*;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(TuttiPlugin::default())
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands, assets: Res<AssetServer>) {
    // One-shot sound effect (auto-despawn when done)
    commands.spawn(PlayAudio::once(assets.load("boom.wav")).despawn_on_finish());

    // Looping ambient at 30% volume
    commands.spawn(PlayAudio::looping(assets.load("wind.ogg")).gain(0.3));
}
```

## Plugin configuration

```rust
// Default: stereo output, system default device
TuttiPlugin::default()

// Custom I/O
TuttiPlugin::with_io(2, 2)          // 2 inputs, 2 outputs
    .with_output_device(1)           // select device by index
    .with_midi()                     // enable MIDI subsystem

// MPE (requires `mpe` feature, automatically enables MIDI)
TuttiPlugin::default().with_mpe(MpeMode::Zone1)

// Resource-only mode (no ECS systems, just TuttiEngineResource)
TuttiPlugin::default().without_ecs()
```

## Direct engine access

Every ECS system is optional. Each subsystem of the engine is its own
Bevy resource — take only what you need:

```rust
use bevy::prelude::*;
use bevy_tutti::*;
use tutti::dsp::sine_hz;

fn control(transport: Res<TransportRes>, mut graph: ResMut<TuttiGraphRes>) {
    transport.tempo(128.0).play();
    let id = graph.0.add(sine_hz::<f32>(440.0));
    graph.0.pipe_output(id);
    graph.0.commit();
}
```

## Node entities

With the `bevy_ecs` integration baked into `tutti`, audio graph nodes can
also be spawned as Bevy entities and tuned with normal `Changed<T>` queries.
The reconcile pipeline (`GraphReconcileSystems::{Spawn, Params, Despawn, Commit}`)
translates component edits into graph operations and coalesces a single
`graph.commit()` per frame.

```rust
use bevy::prelude::*;
use bevy_tutti::*;
use tutti::dsp::sine_hz;

fn setup(mut commands: Commands) {
    commands
        .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
        .insert(Volume(0.5));
}

fn fade(mut q: Query<&mut Volume, With<AudioNode>>) {
    for mut v in &mut q {
        v.0 = (v.0 - 0.005).max(0.0);
    }
}
```

Despawn the entity to remove the underlying graph node.

### Components

Every component is a thin wrapper over a tutti capability that already
exists. See [`tutti::core::ecs`] for the parameter components and the
top-level bevy-tutti module docs for the binding modules.

| Component | Feature | What it binds |
|-----------|---------|---------------|
| `AudioNode(NodeId)` | always | Identity for "this entity owns a graph node." |
| `NodeKind` | always | Typed dispatch tag (Sampler, Plugin, Generator, …). |
| `Volume`, `Pan`, `Mute` | always | Per-node parameter components. |
| `PluginParam { id, value }` | `plugin` | RT-safe `PluginHandle::set_parameter` write. |
| `SamplerSpeed`, `SamplerLooping` | `sampler` | `SamplerUnit::set_speed` / `set_looping`. |
| `PendingSamplerLoad` | `sampler` | "Load a wave, then build a `SamplerUnit`." |
| `WaveImportQueue` (resource) | `sampler` | Tracks in-flight `tutti::sampler::file::ImportHandle`s. |
| `AutomationLaneNode`, `AutomationDrivesParam` | `automation` | Drive `Volume` / `PluginParam` from a `LiveAutomationLane<f32>` output. |
| `MidiSynthMarker`, `ScheduledMidi` | `midi` | Time-delayed MIDI dispatch via `MidiBusRes`. |
| `SidechainOf`, `SidechainSources` | always | Wire one entity's audio into another's input port 1. |
| `PendingVst2Build` | `plugin` + `vst2` | Main-thread VST2 loader (avoids JUCE MessageManager mis-binding). |

### Helpers

| Helper | Where | What it does |
|--------|-------|--------------|
| `Commands::spawn_audio_node(unit, kind)` | always | Add `unit` to the graph + spawn an entity with `AudioNode + NodeKind`. |
| `crossfade_audio_node(commands, entity, new_unit)` | always | `TuttiGraph::crossfade_boxed` for entity-as-node, same `NodeId` survives. |

## ECS resources

These resources are synced from the engine every frame via lock-free atomics:

| Resource | Feature | Description |
|----------|---------|-------------|
| `TuttiGraphRes` | always | `TuttiGraph` -- editable DSP graph |
| `TransportRes` | always | Lock-free transport handle (play/stop/seek/tempo/loop) |
| `MeteringRes` | always | Lock-free metering snapshots |
| `TransportState` | always | Beat position, tempo, play/pause/record/loop state |
| `MasterMeterLevels` | always | Peak and RMS levels (L/R) |
| `AudioDeviceState` | always | Output devices, current device, running status |
| `ContentBounds` | `sampler` | Content end beat and duration in seconds |
| `LiveAnalysisData` | `analysis` | Spectrum, loudness, and other analysis data |
| `AudioInputState` | `sampler` | Input device info and capture status |

## Trigger components

Spawn an entity with a trigger component to perform an action. The corresponding system processes `Added<T>` queries, does the work, removes the trigger, and inserts a result component.

### Audio playback

```rust
// One-shot
commands.spawn(PlayAudio::once(handle));

// Looping with parameters
commands.spawn(PlayAudio::looping(handle).gain(0.5).speed(1.2));

// With time stretching (returns tuple, must be last in chain)
commands.spawn(PlayAudio::once(handle).gain(0.8).time_stretch(0.5, 0.0));

// Or as companion component
commands.spawn((
    PlayAudio::once(handle),
    TimeStretch { stretch_factor: 0.5, pitch_cents: -100.0 },
));
```

After processing: `PlayAudio` is removed, `AudioEmitter { node_id }` is inserted. If time-stretched, `TimeStretchControl` is also inserted for lock-free parameter updates.

### SoundFont instruments

Requires `soundfont` feature.

```rust
let sf2 = asset_server.load("sounds/GeneralMidi.sf2");
commands.spawn(PlaySoundFont::new(sf2).preset(0).channel(0));
```

### Audio plugins (VST3/VST2/CLAP)

Requires `plugin` feature. Format is auto-detected from file extension.

```rust
commands.spawn(LoadPlugin::new("path/to/Reverb.vst3"));
commands.spawn(LoadPlugin::new("path/to/Synth.clap").param("cutoff", 0.7));
```

After processing: `LoadPlugin` is removed, `AudioEmitter` + `PluginEmitter { handle }` are inserted. Use the `PluginHandle` for parameter control, editor management, and state save/load.

### MIDI

Requires `midi` feature.

```rust
// Route MIDI to an entity's audio node
commands.entity(synth).insert(MidiReceiver { channel: Channel::all() });

// Send MIDI events
commands.spawn(SendMidi {
    target_node: node_id,
    events: vec![MidiEvent::note_on(60, 100)],
});
```

`MidiInputEvent` is emitted as a Bevy message for incoming hardware MIDI (requires `midi-hardware`).

### Neural audio

Requires `neural` feature.

```rust
// Neural synth (also requires `midi`)
let model = asset_server.load("models/violin.mpk");
commands.spawn(PlayNeuralSynth::new(model));

// Neural effect
commands.spawn(PlayNeuralEffect::new(asset_server.load("models/amp_sim.mpk")));
```

### Recording

Requires `sampler` feature.

```rust
// Start recording audio on channel 0
commands.spawn(StartRecording::new(0, RecordingSource::Audio));

// Overdub mode
commands.spawn(StartRecording::new(0, RecordingSource::Audio).mode(RecordingMode::Overdub));

// Stop recording
commands.spawn(StopRecording { channel_index: 0 });
```

### Export

Requires `export` feature.

```rust
commands.spawn(
    StartExport::new("output.wav")
        .duration_seconds(30.0)
        .format(AudioFormat::Wav)
        .normalization(NormalizationMode::Loudness { target_lufs: -14.0 })
);
```

After processing: `StartExport` is removed, `ExportInProgress` is inserted. When done, replaced by `ExportComplete` or `ExportFailed`.

### Audio input

Requires `sampler` feature.

```rust
commands.spawn(EnableAudioInput::new().device(0).monitoring(true).gain(0.8));
commands.spawn(DisableAudioInput);
```

### Live analysis

Requires `analysis` feature.

```rust
commands.spawn(EnableLiveAnalysis);
// Read from Res<LiveAnalysisData>
commands.spawn(DisableLiveAnalysis);
```

### Automation

Requires `automation` feature.

```rust
use tutti::{AutomationEnvelope, AutomationPoint, CurveType};

let mut envelope = AutomationEnvelope::new("volume");
envelope.add_point(AutomationPoint::new(0.0, 0.0))
        .add_point(AutomationPoint::with_curve(4.0, 1.0, CurveType::SCurve));

commands.spawn(AddAutomationLane::with_envelope(envelope));

// Or empty lane for a target
commands.spawn(AddAutomationLane::new("filter_cutoff"));
```

After processing: `AutomationLaneEmitter { node_id }` is inserted.

### DSP nodes

```rust
// LFO (always available, no feature gate)
commands.spawn(AddLfo::new(LfoShape::Sine, 2.0).depth(0.5));
commands.spawn(AddLfo::beat_synced(LfoShape::Triangle, 4.0).depth(0.8));

// Compressor (requires `dsp` feature)
commands.spawn(AddCompressor::new(-18.0, 3.0).attack(0.01).release(0.15).makeup(3.0));
commands.spawn(AddCompressor::new(-18.0, 3.0).stereo());

// Gate (requires `dsp` feature)
commands.spawn(AddGate::new(-25.0).attack(0.002).hold(0.05).release(0.2));
```

### Spatial audio

Requires `spatial` feature.

```rust
// Mark one entity as the listener
commands.spawn((AudioListener, Transform::default()));

// Spatial emitter
commands.spawn((
    PlayAudio::once(handle),
    SpatialAudio::default(),
    Transform::from_xyz(5.0, 0.0, -3.0),
));
```

## Features

All features are opt-in and aligned with Tutti's feature flags.

| Feature | What it enables |
|---------|----------------|
| `sampler` | Audio playback, time stretch, recording, audio input, content bounds |
| `midi` | MIDI routing, send, input events |
| `midi-hardware` | Physical MIDI device connect/disconnect |
| `mpe` | MPE zone configuration and per-note expression |
| `midi2` | MIDI 2.0 message types |
| `soundfont` | SoundFont (.sf2) asset loading and playback |
| `neural` | Neural model asset loading, neural effects (+`midi` for neural synths) |
| `plugin` | VST3/VST2/CLAP plugin hosting |
| `vst2` / `vst3` / `clap` | Individual plugin format support |
| `spatial` | 3D spatial audio with distance attenuation |
| `dsp` | Compressor and gate DSP nodes |
| `automation` | Automation lanes with envelope playback |
| `export` | Offline audio export (WAV/FLAC/MP3/OGG) |
| `analysis` | Live spectrum and loudness analysis |
| `wav` / `flac` / `mp3` / `ogg` | Individual audio format decoders |
| `files` | All audio format decoders |
| `full` | Everything |

LFO is always available (no feature gate required).

## Bevy compatibility

| bevy-tutti | Bevy |
|------------|------|
| 0.1 | 0.17 |

## License

MIT OR Apache-2.0
