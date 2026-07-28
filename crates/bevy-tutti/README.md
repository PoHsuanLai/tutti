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
    // Spawn a synth voice and play a note.
    commands.spawn((
        SynthNode::default(),
        MidiUnit::default(),
    ));
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
use bevy_tutti::prelude::*;
use tutti_core::dsp::sine_hz;
use tutti_core::transport::MotionEvent;

fn control(transport: Res<TransportRes>, mut graph: ResMut<AudioGraphRes>) {
    transport.settings.set_tempo(128.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    let id = graph.0.add(sine_hz::<f32>(440.0));
    graph.0.commit();
}
```

## Node entities

An entity carrying `AudioNode(NodeId)` *is* a node in the graph. The reconcile
pipeline (`GraphReconcileSystems::{Spawn, Params, Despawn, Compensate, Commit}`)
translates component edits into graph operations and coalesces a single
`graph.commit()` per frame.

```rust
use bevy::prelude::*;
use bevy_tutti::prelude::*;
use tutti_core::dsp::sine_hz;

fn setup(mut commands: Commands) {
    // `spawn_audio_node` adds an *unwired* node — it renders nothing until
    // something declares it as a source.
    let osc = commands.spawn_audio_node(sine_hz::<f32>(440.0)).id();
    commands.insert_resource(MasterSources::from(osc));
}
```

Despawn the entity to remove the underlying graph node.

### Components

Every component is a thin wrapper over a tutti capability that already exists.

| Component | Feature | What it binds |
|-----------|---------|---------------|
| `AudioNode(NodeId)` | always | Identity for "this entity owns a graph node." |
| `AudioParam<U, P>` | always | One scalar param: unit `U`, address `P`. Registered with `App::add_audio_param`. |
| `AudioSources` | always | What feeds this entity's input ports. Index *i* is port *i*. |
| `ModParamRange` | `modulation` | Depth/range for a modulated param. |
| `PendingSoundFontUnit` | `synth` | "Build a SoundFont unit off-thread, then bind it." |
| `MidiRouteRule` | `midi` | Which inbound MIDI channel reaches which entities. |
| `PluginEmitter`, `PluginEditorOpen` | `plugin` | A hosted plugin instance and its editor window. |

The DAW parameter components (`Volume`, `Pan`, `Mute`, …) are **not** here: they
are app vocabulary and live app-side. `AudioParam<U, P>` is the generic the
engine adapter keeps.

### Helpers

| Helper | Where | What it does |
|--------|-------|--------------|
| `Commands::spawn_audio_node(unit)` | always | Add `unit` to the graph + spawn an entity with `AudioNode`. The node arrives unwired. |
| `crossfade_audio_node(commands, entity, new_unit)` | always | `Net::crossfade` for entity-as-node; the same `NodeId` survives, so declared wiring keeps resolving. |

## ECS resources

Inserted by `TuttiPlugin` once the engine builds. Each is a newtype over an
engine value — the wrapper adds no behaviour, it just gives a Bevy system a way
to reach it.

| Resource | Feature | Description |
|----------|---------|-------------|
| `AudioGraphRes` | always | fundsp `Net` -- the editable DSP graph. No `Deref`, so the mutate/commit boundary stays visible at call sites. |
| `AudioConfig` | always | Sample rate and channel layout, captured at build |
| `TransportRes` | always | Lock-free transport handle (play/stop/seek/tempo/loop) |
| `MetronomeRes` | always | Shared `ClickState` the click node reads |
| `MeteringRes` | always | Lock-free master peak/RMS meter |
| `MasterSources` | always | What feeds each global output channel |
| `AudioDeviceState` | always | Output devices, current device, running status |
| `ChannelCompensation` | always | Per-channel PDC pre-roll for out-of-graph sources |
| `MidiBusRes`, `MidiRoutingRes` | `midi` | The synth fan-out bus and the inbound routing table |
| `PluginsRes` | `plugin` | The scanned plugin catalog (inserted lazily) |

## Trigger components

Spawn an entity with a trigger component to perform an action. The corresponding system processes `Added<T>` queries, does the work, removes the trigger, and inserts a result component.

### Audio playback

Clip playback goes through `tutti-sampler`'s `TrackClipReaderUnit`: build a
`Voice` (in-RAM `SamplerUnit` or disk-streaming `StreamingClipReader`) and send
it with `ClipCommand::AddVoice`, then drive it with `ClipCommand::Update*`. A
clip bound to a transport derives its read position from the playhead, so it
stays sample-aligned with the timeline.

There is no spawn-a-trigger one-shot API — an earlier `PlayAudio` component
existed but had no consumer and was removed.

### SoundFont instruments

Requires `soundfont` feature.

```rust
let sf2 = asset_server.load("sounds/GeneralMidi.sf2");
commands.spawn(PlaySoundFont { source: sf2, preset: 0, channel: 0 });
```

### Audio plugins (VST3/VST2/CLAP)

Requires `plugin` feature. Format is auto-detected from file extension.

```rust
commands.spawn(LoadPlugin::new("path/to/Reverb.vst3"));
commands.spawn(LoadPlugin::new("path/to/Synth.clap").param("cutoff", 0.7));
```

After processing: `LoadPlugin` is removed, `AudioNode` + `PluginEmitter { handle }` are inserted. Use the `PluginHandle` for parameter control, editor management, and state save/load.

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
fn start(mut export: MessageWriter<StartExport>) {
    export.write(StartExport {
        path: "output.wav".into(),
        duration_seconds: Some(30.0),
        format: Some(AudioFormat::Wav),
        normalization: Some(Normalize::lufs(-14.0)),
        ..default()
    });
}
```

After processing, an entity carrying `ExportInProgress` tracks the in-flight job; on completion it is replaced by `ExportComplete` or `ExportFailed`.

### Audio input

Requires `sampler` feature.

```rust
// EnableAudioInput / DisableAudioInput are Messages (not spawned components).
input.write(EnableAudioInput { device_index: Some(0), monitoring: true, gain: 0.8 });
input.write(DisableAudioInput);
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

commands.spawn(AddAutomationLane { envelope });
```

After processing: `AutomationLaneEmitter { node_id }` is inserted.

### DSP nodes

Spawn the node marker; its `#[require(...)]` list inserts the param components
with their defaults, and you override only the ones you care about via
struct-update.

```rust
// LFO — required: Frequency, ModDepth, LfoShapeKind, BeatSynced
commands.spawn((LfoNodeMarker, Frequency(2.0), ModDepth(0.5), LfoShapeKind::Sine));
commands.spawn((LfoNodeMarker, Frequency(4.0), ModDepth(0.8), LfoShapeKind::Triangle, BeatSynced(true)));

// Compressor — required: ThresholdDb, CompressorRatio, Attack, Release, GainDb
commands.spawn((
    CompressorNode,
    ThresholdDb(-18.0), CompressorRatio(3.0),
    Attack(0.01), Release(0.15), GainDb(3.0),
));

// Gate — required: ThresholdDb, Attack, Release
commands.spawn((GateNode, ThresholdDb(-25.0), Attack(0.002), Release(0.2)));
```

### Spatial audio

Requires `spatial` feature.

```rust
// Mark one entity as the listener
commands.spawn((AudioListener, Transform::default()));

// Spatial emitter: add SpatialAudio + Transform to any entity carrying an
// AudioNode (i.e. a node already in the graph).
commands.spawn((
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
