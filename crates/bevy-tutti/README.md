# bevy-tutti

Bevy plugin for the [Tutti](https://github.com/PoHsuanLai/tutti) audio engine.

Tutti is a real-time, lock-free audio engine for DAWs and interactive audio: a
DSP graph, a transport, MIDI 2.0, sample playback, plugin hosting
(VST2/VST3/CLAP/AU), recording and offline export. `bevy-tutti` runs it inside a
Bevy `App`: `TuttiPlugin` opens the output device and starts the audio callback,
each engine subsystem becomes a Bevy resource, and graph nodes are entities.
Wiring, parameters, MIDI routes and modulation are *declared* as components and
resources; reconcile systems write what changed into the graph and publish it to
the audio thread once per frame.

Use this crate for a Bevy app. For a host without Bevy (a CLI, a server, an
offline renderer), the `tutti` crate exposes the same engine with no ECS.

## Quick start

```rust
use bevy::prelude::*;
use bevy_tutti::prelude::*;
use tutti_core::Hz;
use tutti_nodes::testing::Osc; // a test tone; needs tutti-nodes' `testing` feature

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(TuttiPlugin::default())
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands) {
    // A node arrives unwired; the resource declares what feeds the output.
    let osc = commands.spawn_audio_node(ForkByClone(Osc::sine(Hz(440.0)))).id();
    commands.insert_resource(MasterSources::mono_from(osc));
}
```

`TuttiPlugin { disabled: true, ..Default::default() }` opens no device, for
tests and headless tools; insert `AudioGraphRes::headless(inputs, outputs)`
yourself in that case.

## How it works

- **Nodes are entities.** `Commands::spawn_audio_node(node)` adds a node to the
  graph and spawns an entity carrying `AudioNode`. Despawning the entity removes
  the node. `crossfade_audio_node` swaps the node behind an entity without
  breaking its wiring.
- **Wiring is declared.** `PortSources` on an entity names what feeds each of
  its input ports; the `MasterSources` resource names what feeds each output
  channel; `EventSources` names what feeds a node's event (MIDI) input. Nothing
  calls `connect`.
- **Parameters are components.** `AudioParam<U, P>` holds one parameter value
  (unit `U`, parameter address `P`); register each type once with
  `App::add_audio_param`.
- **One commit per frame.** The reconcile systems run in `Update` in the
  `GraphReconcileSystems` sets (`Spawn`, `Params`, `Despawn`, `Compensate`,
  `Commit`). Direct edits through `AudioGraphRes` set `GraphDirty`, and
  `commit_graph` publishes the frame's edits together.
- **Latency is compensated by the graph.** `GraphLatency` and
  `ChannelCompensation` report the plan the audio thread runs.

## Direct engine access

Every subsystem is its own resource, so a system takes only what it needs:

```rust
use bevy::prelude::*;
use bevy_tutti::prelude::*;

fn control(transport: Res<TransportRes>, meter: Res<MeteringRes>) {
    transport.settings.set_tempo(128.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    let _ = meter;
}
```

| Resource | Feature | What it is |
|----------|---------|------------|
| `AudioGraphRes` | always | The editable DSP graph |
| `AudioConfig` | always | Sample rate and channel layout |
| `TransportRes` | always | Play, stop, seek, tempo and loop |
| `MetronomeRes` | always | The metronome click state |
| `MeteringRes` | always | Master peak and RMS meters |
| `AudioTapRes` | always | A copy of the master output for analysis or recording |
| `MasterSources` | always | What feeds each output channel |
| `AudioDeviceState`, `AudioEngineState` | always | The output device and whether the engine is running |
| `GraphLatency`, `ChannelCompensation` | always | Latency compensation figures |
| `MidiEngineNodes`, `MpeModeRes` | `midi` | The engine's MIDI nodes and the input's MPE mode |
| `DiskStreamerRes` | `sampler` | The disk-streaming engine |
| `PluginsRes` | `plugin` | The scanned plugin catalog |

## Subsystems

### MIDI (`midi`)

```rust
// Hardware channel 1 plays the synth entity.
commands.spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(synth));
// A clip plays it too.
commands.spawn(MidiSourceInstall::new(synth, events));
// And a keyboard: once wired, the entity's `LiveMidi` component sends notes.
commands.entity(synth).insert(LiveMidiInput);
```

`midi-hardware` adds OS MIDI ports and device hot-plug.

### Recording and audio I/O (`audio-io`)

`AudioPump` moves frames from any `AudioIn` to any `AudioOut` on its own thread,
and finalizes the sink exactly once however the pump ends; `PumpFinished`
reports the result.

```rust
app.add_audio_pump::<f32>();

let mic = MicIn::open(None, config.sample_rate)?;
let wav = mic.matching_sink(&path, BitDepth::Float32)?;
let pump = commands.spawn(AudioPump::start(mic, wav, Samples(1024))).id();
```

`TapIn::new(tap.open()?)` records the master output the same way.
`MicIn::open_with_monitor` also returns a `MicMonitorNode` to hear the input
through the graph. `Recorder` is the same loop without ECS.

### Sample playback (`sampler`), SoundFonts (`soundfont`), synths (`synth`)

`sampler` registers a `.wav` asset loader and adds `DiskStreamerRes` for disk
streaming; `memory_voice` and `InsertVoice` put a voice on an entity.
`soundfont` loads `.sf2` assets and plays them with `PlaySoundFont`. `synth`
re-exports the polyphonic synth.

### Plugin hosting (`plugin`)

Plugins run out of process. Spawn a `PluginRequest` with a plugin id and sample
rate; when the plugin loads, the entity gets `AudioNode` and `PluginEmitter`.
`PluginsRes` scans and lists installed plugins; `SetEditorVisible` opens and
closes a plugin's editor window. Enable a format with `vst2`, `vst3`, `clap` or
`au`.

### Export (`export`)

Spawn an `ExportRequest`; `ExportPlugin` renders a copy of the live graph off
the main thread while the live graph keeps playing, and reports `ExportDone` or
an `ExportError`.

### Modulation (`modulation`) and spatial audio (`spatial`)

`modulation` adds LFO sources (`ModSource`) and modulation routes (`ModRoute`)
as entities; add `TuttiModulationPlugin`. `spatial` re-exports `tutti-spatial`'s
VBAP panner, and `hrtf` its binaural panner, spawned like any other node.

## Feature flags

No feature is on by default; the default build is the graph, transport,
metering, device handling and `AudioPump`.

| Feature | What it enables |
|---------|-----------------|
| `full` | Everything below except the plugin formats and `convolution` |
| `midi` | MIDI routing, clip sequencing, MIDI files, clock output and MPE, with no OS MIDI I/O |
| `midi-hardware` | OS MIDI ports, device hot-plug and MIDI 2.0 endpoints (implies `midi`) |
| `synth` | The polyphonic synth |
| `soundfont` | SoundFont (`.sf2`) assets and playback (implies `midi`) |
| `sampler` | Clip playback, disk streaming and the `.wav` asset loader (implies `wav`) |
| `audio-io` | Microphone capture, WAV writing and recording |
| `wav` / `flac` / `mp3` / `ogg` | Audio file decoders |
| `modulation` | LFOs and a modulation matrix as ECS entities |
| `spatial` | VBAP panning and surround mixing |
| `hrtf` | The HRTF binaural panner (implies `spatial`) |
| `convolution` | The FFT convolution reverb node |
| `export` | Offline rendering to files or buffers |
| `plugin` | Out-of-process plugin hosting, editor windows and catalog scans (implies `midi`) |
| `vst2` / `vst3` / `clap` / `au` | Each plugin format (implies `plugin`) |

## Bevy compatibility

| bevy-tutti | Bevy |
|------------|------|
| 0.1 | 0.19 |

## License

MIT OR Apache-2.0
