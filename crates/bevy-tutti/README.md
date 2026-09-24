# bevy-tutti

Bevy plugin for the [Tutti](https://github.com/PoHsuanLai/tutti) audio engine. Exposes Tutti's real-time audio graph, transport, MIDI, plugin hosting, and DSP as Bevy ECS components, resources, and systems.

## What is this?

[Tutti](https://github.com/PoHsuanLai/tutti) is a real-time, lock-free audio engine built in Rust for DAW and interactive audio applications. It handles synthesis, sample playback, MIDI, plugin hosting (VST3/VST2/CLAP), recording, automation, spatial audio, and offline export.

**bevy-tutti** bridges Tutti into the Bevy ECS. Instead of managing audio lifetimes and callbacks manually, you spawn entities with trigger components and let systems handle the rest. The plugin also syncs engine state (transport, metering, device info) into Bevy resources every frame, so your UI and game logic can read audio state without touching the audio thread.

## Quick start

```rust
use bevy::prelude::*;
use bevy_tutti::*;
use tutti_core::Hz;
use tutti_nodes::testing::Osc;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(TuttiPlugin::default())
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands) {
    // A node is spawned unwired; the resource declares what feeds the master.
    let osc = commands.spawn_audio_node(Osc::sine(Hz(440.0))).id();
    commands.insert_resource(MasterSources::mono_from(osc));
}
```

## Plugin configuration

```rust
// Default: stereo output, system default device
TuttiPlugin::default()

// Custom I/O
TuttiPlugin { inputs: 2, outputs: 2, ..Default::default() }
               // select device by index
                         // enable MIDI subsystem

// MPE (requires `mpe` feature, automatically enables MIDI)
TuttiPlugin::default()

// Resource-only mode (no ECS systems, just TuttiEngineResource)
TuttiPlugin::default()
```

## Direct engine access

Every ECS system is optional. Each subsystem of the engine is its own
Bevy resource — take only what you need:

```rust
use bevy::prelude::*;
use bevy_tutti::prelude::*;
use tutti_core::Hz;
use tutti_nodes::testing::Osc;
use tutti_core::transport::MotionEvent;

fn control(transport: Res<TransportRes>, mut graph: ResMut<AudioGraphRes>) {
    transport.settings.set_tempo(128.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    let id = graph.0.add(Osc::sine(Hz(440.0)));
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
use tutti_core::Hz;
use tutti_nodes::testing::Osc;

fn setup(mut commands: Commands) {
    // `spawn_audio_node` adds an *unwired* node — it renders nothing until
    // something declares it as a source.
    let osc = commands.spawn_audio_node(Osc::sine(Hz(440.0))).id();
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
| `PortSources` | always | What feeds this entity's input ports. Index *i* is port *i*. |
| `AudioPump<S, CH>` | always | A running `AudioIn` → `AudioOut` transfer. Registered with `App::add_audio_pump`. |
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
| `AudioTapRes` | always | Post-master frame tap for analysis. Closed at build; `open()` hands back a consumer the caller owns |
| `MasterSources` | always | What feeds each global output channel |
| `AudioDeviceState` | always | Output devices, current device, running status |
| `ChannelCompensation` | always | Per-channel PDC pre-roll for out-of-graph sources |
| `MidiBusRes`, `MidiRoutingRes` | `midi` | The synth fan-out bus and the inbound routing table |
| `DiskStreamerRes` | `sampler` | The disk-streaming engine; owns the butler thread |
| `PluginsRes` | `plugin` | The scanned plugin catalog (inserted lazily) |

## Trigger components

Spawn an entity with a trigger component to perform an action. The corresponding system processes `Added<T>` queries, does the work, removes the trigger, and inserts a result component.

### Audio playback

Clip playback goes through `tutti-sampler`'s `VoicePool`: build a `Voice` (in-RAM
`MemorySource` or disk-streaming `DiskVoice`) and send it with
`VoiceCommand::Add`, then drive it with the other `VoiceCommand` variants. A
voice bound to a transport derives its read position from the playhead, so it
stays sample-aligned with the timeline.

Disk streaming is driven through `DiskStreamerRes`: `commands()` issues
stream/seek/loop operations and `status()` builds a `DiskVoice` to wire into the
graph. Neither is wrapped in ECS vocabulary — both speak in channel indices and
timeline placements, which is clip-scheduling policy a host owns.

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
commands.spawn(PluginRequest::new("path/to/Reverb.vst3"));
```

After processing: `PluginRequest` becomes a private `PendingPlugin`, and on completion `AudioNode` + `PluginEmitter` are inserted. Use the `PluginHandle` for parameter control, editor management, and state save/load.

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

### Recording, and audio I/O generally

Recording is one case of moving frames from an `AudioIn` to an `AudioOut`, which
is what `AudioPump` does. Register the frame type once, then spawn a pump:

```rust
app.add_audio_pump::<f32, 2>();   // stereo — mic, WAV
app.add_audio_pump::<f32, 6>();   // 5.1 render

// Mic -> WAV. `matching_sink` builds the sink from the device's own rate and
// width, which is the one place both halves are in scope — hand-rolling the
// `WavOut` is how you get a file that plays at the wrong speed.
// `MicIn::open_with_monitor` also hands back a monitor node; see below.
let mic = MicIn::open(None)?;
let wav = mic.matching_sink(&path, BitDepth::Float32)
    .ok_or("could not create WAV")?;
let pump = commands.spawn(AudioPump::start(mic, wav, 1024)).id();

// Later:
audio_pumps.get(pump)?.stop();
```

The master output records the same way — `AudioTapRes` is the engine's
lock-free copy of it, and `TapIn` adapts the consumer end into an `AudioIn`:

```rust
// One consumer at a time: `open` returns `Err(TapBusy)` rather than displacing
// an analysis reader that got there first.
let src = TapIn::new(tap.open()?);
let wav = WavOut::create(&path, config.sample_rate, 2, BitDepth::Float32)
    .ok_or("could not create WAV")?;
commands.spawn(AudioPump::start(src, wav, 1024));
```

Nothing can check that a sink's rate matches its source — `AudioIn` carries no
rate — so for a tap it comes from `AudioConfig`, not from the source.

There is no policy argument for what an empty poll means: that is
`AudioIn::ON_EMPTY`, a property of the source type. A `MicIn` is `Starved` (an
empty ring means the callback has not pushed yet, so the pump parks and
retries); a decoded file is `EndOfStream` (the pump finishes and finalizes).
Passing it per call would let a caller state it wrong, and treating a mic as
finite would end a recording milliseconds in with no error.

The sink is finalized **exactly once on every path out** — an explicit `stop()`,
the source ending itself, or the entity being despawned mid-pump. That matters
because `AudioOut::finalize` consumes `self` and can fail; for a WAV, missing it
leaves the header unpatched and the file unreadable. `PumpFinished` carries the
result, so a host can react to a sink that failed to close.

The pump runs on its own thread, not a Bevy task pool: it never completes, and
`AsyncComputeTaskPool` caps at four threads, so live pumps would starve every
other async job.

`AudioPump` is the ECS-shaped surface. The same loop without an ECS is
`tutti_io::Recorder`, which takes the same `(source, sink)` pair and hands back a
handle you `stop()` — reach for it in a non-Bevy host, or in a Bevy one for work
whose lifetime you want to own yourself.

#### Monitoring while recording

`MicIn::open_with_monitor` returns a `MicMonitorNode` alongside the source. The
two drain **independent rings** fed by the same callback — a deep one for
recording, a shallow one for monitoring — so polling one never steals frames
from the other.

The node is a plain `AudioUnit`; add it and declare what it feeds, like any node:

```rust
let (mic, monitor) = MicIn::open_with_monitor(None)?;
let id = graph.0.add(Box::new(monitor));
commands.spawn(AudioNode(id));   // then name it in MasterSources or an PortSources
```

A monitor node that is never wired fills its ~10 ms ring and then silently drops
every frame — there is no error and no counter, so an unwired monitor looks
exactly like a working one.

### Export

Requires the `export` feature. An export is an **entity**: spawn an
[`ExportRequest`] and `ExportPlugin` drives it to completion off the main
thread.

The underlying engine call is also available directly:

```rust,ignore
// `net` by value, and the clock is mandatory — forgetting the transport is a
// compile error rather than a silently silent render.
let written = tutti_export::render_to_file(net, &config, &clock, &path)?;
```

### DSP nodes

Spawn the node marker; its `#[require(...)]` list inserts the param components
with their defaults, and you override only the ones you care about via
struct-update.

```rust
// DSP nodes are plain `AudioUnit`s from `tutti-nodes`, spawned like any
// other node. There are no marker components and no per-node ECS wrappers.
use tutti_nodes::{CompressorNode, LfoNode};

commands.spawn_audio_node(LfoNode::new(Hz(2.0)));
commands.spawn_audio_node(CompressorNode::default());

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
// `tutti-spatial` is re-exported whole; there is no adapter code. The VBAP
// and binaural panners are plain `AudioUnit`s, spawned like any other node,
// and `build_vbap_mix` assembles a subgraph the host spawns the same way.
use bevy_tutti::spatial::vbap::VbapPannerNode;

commands.spawn_audio_node(VbapPannerNode::new(layout, sample_rate)?);
```

## Features

All features are opt-in and aligned with Tutti's feature flags.

| Feature | What it enables |
|---------|----------------|
| `sampler` | Clip playback: the `.wav` asset loader and `DiskStreamerRes` |
| `audio-io` | The live I/O edge (`bevy_tutti::io`): mic capture, `WavOut`, `Recorder` |
| `midi` | MIDI routing, sequencing, clock, MPE — no OS I/O |
| `midi-hardware` | The OS layer on top of `midi`: device connect/poll, CoreMIDI virtual ports |
| `synth` | The software synths |
| `soundfont` | SoundFont (.sf2) asset loading and playback (implies `synth`, `midi`) |
| `plugin` | VST3/VST2/CLAP/AU hosting: editor windows, catalog scan, crash detection |
| `vst2` / `vst3` / `clap` / `au` | Individual plugin format support (each implies `plugin`) |
| `modulation` | Control-rate modulation: LFO sources and mod-matrix edges as entities |
| `spatial` | 3D spatial audio with distance attenuation (implies `dsp`) |
| `dsp` | The VBAP/binaural panner. Dynamics are always compiled |
| `convolution` | FFT convolution reverb (partitioned IR) |
| `export` | The `tutti-export` dependency (no ECS surface — see above) |
| `wav` / `flac` / `mp3` / `ogg` | Individual audio format decoders |
| `full` | Everything above except the opt-in plugin formats |

`AudioPump`, the graph, transport, metering and PDC are always available — no
feature gate. The pump in particular needs neither `sampler` nor `audio-io`: it
speaks `AudioIn` / `AudioOut`, which live in `tutti-core`. `audio-io` is what
gives you a `WavOut` to point it at.

`sampler` and `audio-io` are independent in both directions — recording a take
needs no clip playback, and playing a clip needs no microphone. They were one
flag until the live I/O edge moved to its own crate; a host wanting only mic→WAV
was compiling the butler streaming thread to get it.

## Bevy compatibility

| bevy-tutti | Bevy |
|------------|------|
| 0.1 | 0.19 |

## License

MIT OR Apache-2.0
