# Tutti

A real-time audio engine for DAW applications in Rust: an audio graph runtime,
MIDI 2.0 processing, sample playback, spatial audio, offline rendering, and
plugin hosting for VST3, CLAP, Audio Units and VST2.

The engine is a set of focused crates rather than one package. Depend on the
ones you need, or take an umbrella:

- [`tutti`](crates/tutti) — the whole engine behind one dependency. For a CLI,
  a renderer, a server, a test. It is a re-export crate, enforced to contain no
  code at all.

A Bevy adapter lives at https://github.com/PoHsuanLai/bevy-tutti.

> **Status: pre-release.** Nothing here is published to crates.io yet, so every
> dependency is a path dependency. Names and signatures still move between
> commits.

## The layering

Every crate sits in one of four tiers, and the arrows only ever point down.

```
         tutti                         the umbrella
                            │
   ┌──────────┬─────────────┼──────────────┬─────────────┐
  dsp/       midi/        plugin/         io            export
   │          │             │              │              │
   └──────────┴──────┬──────┴──────────────┴──────────────┘
                 tutti-engine              the graph render
                     │
                 tutti-types               the shared vocabulary
```

**The floor.** `tutti-types` is the vocabulary every other crate speaks — the
unit newtypes (`Hz`, `Db`, `Beat`, `Samples`, `SampleRate`), the channel
layouts, the `Source`/`Sink` edge traits, and `ArcLend` (re-exported from
the dependency-free `arc-lend` crate), the one sanctioned way to hand
non-scalar state to the audio thread: the audio thread only borrows, so it
never frees. `tutti-graph` is
the graph: the `Node` contract, a compiler from a `Topology` to an immutable
plan (latency compensation included), and the executor. `tutti-engine` builds the
runtime on both: the `Engine` that renders the graph, over the `Transport` of
`tutti-transport` and the load meter of `tutti-metering` (level meters are
ops in the graph's plan).

**The edges.** `tutti-cpal` is the only path to a sound card. The live edge is
spread over the crates that own its parts: a tap (`TapReader`) is a `Source`
and an inlet (`InletNode`) copies outside audio into the graph (`tutti-graph`),
`FileOut` is the file sink (`tutti-codec`), and `Recorder` pumps one into the
other (`tutti-session`). `tutti-export` is its
deliberate opposite number: the *offline* edge, rendering a graph faster than
real time (a project's bounce, `tutti-session`'s `Bounce`, drives it).

**The subsystems.**

| Crate | What it owns |
|---|---|
| [`tutti-nodes`](crates/tutti-nodes) | The DSP node library: filters, delays, dynamics, distortion, chorus, convolution, mix bus, automation |
| [`tutti-sampler`](crates/tutti-sampler) | Clip playback — in-memory and disk-streamed voices, time-stretch, the prefetch streamer |
| [`tutti-polysynth`](crates/tutti-polysynth) | A polyphonic subtractive/wavetable synth with MPE |
| [`tutti-soundfont`](crates/tutti-soundfont) | SoundFont (`.sf2`) playback |
| [`tutti-dsp`](crates/tutti-dsp) | Waveform summaries, onset detection, pitch, correlation, loudness |
| [`tutti-modulation`](crates/tutti-modulation) | The modulation compiler — routes to one control program per param, which a control lane plays; audio-free |
| [`tutti-midi-runtime`](crates/tutti-midi-runtime) | Routing, voice allocation, MPE, clock |
| [`tutti-midi-file`](crates/tutti-midi-file) | SMF and MIDI 2.0 clip codecs — OS-free, so reading a `.mid` links no CoreMIDI |
| [`tutti-midi-hardware`](crates/tutti-midi-hardware) | The OS MIDI edge: CoreMIDI and ALSA seq-UMP |
| [`tutti-plugin`](crates/tutti-plugin) | Plugin hosting, **out of process** — a crashing plugin does not take the host with it |

The four format hosts ([VST3](crates/tutti-vst3-host),
[CLAP](crates/tutti-clap-host),
[AU](crates/tutti-au-host),
[VST2](crates/tutti-vst2-host)) are what `tutti-plugin` hosts; `tutti-plugin`
is what you use, and each host speaks one plugin ABI.

## Quick start

`crates/tutti/README.md` has the dependency line and the feature table. Every crate's own documentation carries a runnable example for that subsystem;
those are doctests, so they are checked on every build.

## Two rules worth knowing before you read further

**Wiring is declared, not called.** A node is added unwired; a connection names
what feeds each input port. The graph holds a set of sources per input and
sums them (doc 013, "Audio fan-in sums").

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

`cargo xtask help` lists the repo's development commands (the gates CI runs,
the feature suites, benches); `cargo xtask ci` runs all of CI's checks.

Cloning needs `--recursive`: `tutti-vst3-host` compiles a reference plugin
against the vendored VST3 SDK, which is a submodule. Without it the build stops
with instructions.

## License

MIT OR Apache-2.0.
