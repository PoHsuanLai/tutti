# Tutti

A real-time audio engine for DAW applications in Rust: an audio graph runtime,
MIDI 2.0 processing, sample playback, spatial audio, offline rendering, and
plugin hosting for VST3, CLAP, Audio Units and VST2.

The engine is a set of focused crates rather than one package. Depend on the
ones you need, or take an umbrella:

- [`tutti`](crates/tutti) — the whole engine behind one dependency, **no
  Bevy**. For a CLI, a renderer, a server, a test.
- [`bevy-tutti`](crates/bevy-tutti) — the same engine plus the Bevy adapter
  (ECS, assets, systems).

Both are re-export crates; `tutti` is enforced to contain no code at all.

> **Status: pre-release.** Nothing here is published to crates.io yet, so every
> dependency is a path dependency. Names and signatures still move between
> commits.

## The layering

Every crate sits in one of four tiers, and the arrows only ever point down.

```
         tutti  ·  bevy-tutti          the two umbrellas: Bevy-free, and the
                                           Bevy adapter (ECS, assets, systems)
                            │
   ┌──────────┬─────────────┼──────────────┬─────────────┐
  dsp/       midi/        plugin/         io            export
   │          │             │              │              │
   └──────────┴──────┬──────┴──────────────┴──────────────┘
                 tutti-core                the graph runtime
                     │
                 tutti-types               the shared vocabulary
```

**The floor.** `tutti-types` is the vocabulary every other crate speaks — the
unit newtypes (`Hz`, `Db`, `Beat`, `Samples`, `SampleRate`), the channel
layouts, the `AudioIn`/`AudioOut` edge traits, and `RtPublish`, the one
sanctioned way to hand non-scalar state to the audio thread. `tutti-core` builds
the runtime on it: the graph (FunDSP's `Net`), `Transport`, metering, and
latency compensation.

**The edges.** `tutti-cpal` is the only path to a sound card. `tutti-io` is the
live edge — a microphone monitor, a WAV sink, `Recorder`. `tutti-export` is its
deliberate opposite number: the *offline* edge, rendering a graph faster than
real time.

**The subsystems.**

| Crate | What it owns |
|---|---|
| [`tutti-nodes`](crates/dsp/tutti-nodes) | The DSP node library: filters, delays, dynamics, distortion, chorus, convolution, mix bus, automation |
| [`tutti-sampler`](crates/dsp/tutti-sampler) | Clip playback — in-memory and disk-streamed voices, time-stretch, the prefetch butler |
| [`tutti-polysynth`](crates/dsp/tutti-polysynth) | A polyphonic subtractive/wavetable synth with MPE |
| [`tutti-soundfont`](crates/dsp/tutti-soundfont) | SoundFont (`.sf2`) playback |
| [`tutti-spatial`](crates/dsp/tutti-spatial) | The engine's only geometry: VBAP for speakers, HRTF for headphones |
| [`tutti-analysis`](crates/dsp/tutti-analysis) | Waveform summaries, onset detection, pitch, correlation, loudness |
| [`tutti-mod`](crates/core/tutti-mod) | The modulation matrix — audio-free and Bevy-free |
| [`tutti-midi-types`](crates/midi/tutti-midi-types) | MIDI 2.0 / UMP value types, the vocabulary the MIDI stack shares |
| [`tutti-midi-runtime`](crates/midi/tutti-midi-runtime) | Routing, voice allocation, MPE, clock |
| [`tutti-midi-file`](crates/midi/tutti-midi-file) | SMF and MIDI 2.0 clip codecs — OS-free, so reading a `.mid` links no CoreMIDI |
| [`tutti-midi-hardware`](crates/midi/tutti-midi-hardware) | The OS MIDI edge: CoreMIDI and ALSA seq-UMP |
| [`tutti-plugin`](crates/plugin/tutti-plugin) | Plugin hosting, **out of process** — a crashing plugin does not take the host with it |

The four format hosts ([VST3](crates/plugin/formats/tutti-vst3-host),
[CLAP](crates/plugin/formats/tutti-clap-host),
[AU](crates/plugin/formats/tutti-au-host),
[VST2](crates/plugin/formats/tutti-vst2-host)) sit under `tutti-plugin`, which
is what you use; each speaks one plugin ABI.

## Quick start

The engine is Bevy-free at its core, but the shortest path to audible output is
the adapter. This spawns an oscillator and wires it to the master out:

```rust,ignore
use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_tutti::prelude::*;
use tutti_core::dsp::{sine_hz, Net};

fn build_chain(mut commands: Commands) {
    let osc = commands.spawn_audio_node(sine_hz::<f32>(440.0)).id();
    // Wiring is *declared*, never called: the resource names what feeds each
    // global output channel, so two nodes cannot both claim the master.
    commands.insert_resource(MasterSources::mono_from(osc));
}

App::new()
    .add_plugins(DefaultPlugins)
    .add_plugins(TuttiPlugin::default())
    .add_systems(Startup, build_chain)
    .run();
```

Every crate's own documentation carries a runnable example for that subsystem;
those are doctests, so they are checked on every build.

## Two rules worth knowing before you read further

**Wiring is declared, not called.** A node is spawned unwired; a resource names
what feeds each input port. `Net` holds exactly one source per input, and so
does the declaration — which is what makes accidental fan-in unrepresentable.
Summing is a node's job.

**Quantities carry their units.** `Hz`, `Db`, `Beat`, `Samples` and the rest are
newtypes, not `f32` aliases, and the conversions between them are named methods
(`Db::to_amplitude`, `Seconds::to_samples`). The types stop at C ABI boundaries,
and where they stop, a comment says why.

## Building

```bash
cargo check --workspace
cargo nextest run --workspace     # nextest does NOT run doctests
cargo test --workspace --doc      # so run these separately
```

Cloning needs `--recursive`: `tutti-vst3-host` compiles a reference plugin
against the vendored VST3 SDK, which is a submodule. Without it the build stops
with instructions.

`bevy` is an optional, off-by-default feature on every engine crate that has
one, so the engine compiles without Bevy unless a consumer asks for it.

## License

MIT OR Apache-2.0.
