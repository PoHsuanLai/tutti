# tutti

The Tutti real-time audio engine behind one dependency, with no Bevy.

Tutti is a set of focused crates: a graph compiler and executor, a transport,
a device layer, DSP nodes, samplers and synths, MIDI, plugin hosting, offline
export. This crate re-exports each of them as a module, so a headless consumer
— a CLI, an offline renderer, a server, a test — names one dependency and
turns subsystems on with features. Writing a Bevy app? Take `bevy-tutti`
instead: the same engine plus the ECS adapter.

```toml
[dependencies]
tutti = { version = "0.0.1", features = ["device", "wav"] }
```

## Quick start

Build a graph, hand its executor to an [`Engine`](crate::core::Engine), and
render a block. No device is needed: this is exactly what the audio callback
does once per buffer.

```rust
use tutti::graph::{GraphBuilder, Prepare};
use tutti::nodes::{LfoNode, LfoShape};
use tutti::prelude::*;

// A graph with no inputs and a stereo output: a 440 Hz sine on both channels.
let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
// A node with parameters also hands back its controls (a `ParamSet`), which
// set those parameters live from the control thread.
let sine = LfoNode::new(LfoShape::Sine).with_frequency(Hz(440.0));
let (tone, _controls) = g.add_with_controls(sine);
g.pipe_output(tone);

// Building prepares every node at the render's rate and largest block. The
// editor stays on the control thread; the executor goes to the engine.
let (mut editor, executor) = g
    .build(Prepare::new(SampleRate(48_000.0), Samples(256)))
    .expect("the graph compiles");

let transport = Transport::new(48_000.0);
let engine = Engine::new(&transport, &mut editor, executor).expect("within the limits");

// One interleaved stereo block, rendered on "the audio thread".
let mut out = vec![0.0f32; 256 * 2];
engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
assert!(out.iter().any(|&s| s != 0.0));
```

Keep the editor: later edits go through it (`commit`) and reach the engine
on its next block.

To play through a sound card, turn on `device` and wrap the same engine in
`tutti::device`'s `AudioCallbackState` and `TuttiDriver`:
`examples/headless_engine.rs` does it in about thirty lines. To write a file
instead, turn on `export` and a codec: see `examples/headless_export.rs`.

## How the pieces fit

- **The graph** ([`graph`](crate::graph)): build one with `GraphBuilder`, or
  edit a live one through its `Editor`. Every change is compiled on the
  control thread and handed to the `Executor` with `commit`, so the audio
  thread never allocates or locks.
- **The engine** ([`core`](crate::core)): [`Engine`](crate::core::Engine)
  renders the executor once per device buffer, folds its output to the
  device width, and applies transport commands on their frame.
  [`Transport`](crate::core::Transport) holds tempo, loop and play state;
  nodes read the playhead per frame from their block's `Env`.
- **The vocabulary** ([`types`](crate::types)): unit newtypes (`Hz`, `Db`,
  `Beat`, `Samples`, …), channel layouts, interleaved buffer views, and the
  real-time primitives the other crates share.
- **The nodes** ([`nodes`](crate::nodes)) and the instrument crates plug into
  the graph as `tutti::graph::Node`s; the edges (`device`, `io`, `export`)
  move samples between the graph and the outside world.

Most of what a host names is in [`prelude`](crate::prelude).

## The crates, by module

| module | crate | what it is | feature |
|---|---|---|---|
| `tutti::core` | `tutti-core` | the engine: `Engine`, `Transport`, metering, delay compensation | always |
| `tutti::types` | `tutti-types` | units, channel layouts, buffer views, RT primitives | always |
| `tutti::graph` | `tutti-graph` | the audio graph: builder, compiler, `Editor` / `Executor` | always |
| `tutti::nodes` | `tutti-nodes` | built-in nodes: filters, delays, dynamics, LFOs, mixing, automation | always |
| `tutti::device` | `tutti-cpal` | the sound card: CPAL output stream, mic capture, driver lifecycle | `device` |
| `tutti::io` | `tutti-io` | the I/O edge: file decode, WAV out, mic monitoring, recording | `io` |
| `tutti::export` | `tutti-export` | offline render of a graph to a buffer or a file | `export` |
| `tutti::sampler` | `tutti-sampler` | sample playback: in-memory and streamed voices, time stretch | `sampler` |
| `tutti::polysynth` | `tutti-polysynth` | polyphonic subtractive synth node | `synth` |
| `tutti::soundfont` | `tutti-soundfont` | SoundFont (.sf2) playback node | `soundfont` |
| `tutti::spatial` | `tutti-spatial` | VBAP speaker panning, binaural HRTF | `spatial` |
| `tutti::analysis` | `tutti-analysis` | waveform, onsets, pitch, loudness, correlation, STFT | `analysis` |
| `tutti::modulation` | `tutti-mod` | modulation sources, targets and the mod matrix | `modulation` |
| `tutti::midi` | `tutti-midi-types` | MIDI value types, MIDI 2.0 / UMP native | `midi` |
| `tutti::midi_runtime` | `tutti-midi-runtime` | MIDI graph nodes, MPE, MIDI-CI, SysEx | `midi` |
| `tutti::midi_file` | `tutti-midi-file` | Standard MIDI File and MIDI 2.0 Clip File codecs | `midi` |
| `tutti::midi_hardware` | `tutti-midi-hardware` | OS MIDI I/O: endpoints, send and receive | `midi-hardware` |
| `tutti::plugin` | `tutti-plugin` | VST2, VST3, CLAP and AU plugins as graph nodes | `plugin` + a format |

Each module is the whole crate, so its own docs are the reference for it.

## Features

None are on by default.

- `full` — everything below except `jack` and the plugin features.
- `device` — `tutti::device`, the sound card. Off by default so an offline
  render, a CI job or a server builds without ALSA or CoreAudio headers.
- `jack` — JACK output through `tutti-cpal` (links libjack; implies `device`).
- `wav`, `flac`, `mp3`, `ogg` — audio file codecs (each implies `io`, and
  enables the same codec in the sampler when `sampler` is on).
- `io` — `tutti::io`. Implied by `audio-io` and every codec.
- `audio-io` — mic capture and monitoring (implies `io`; the capture stream
  itself needs `device`).
- `export` — `tutti::export`.
- `midi` — `tutti::midi`, `tutti::midi_runtime`, `tutti::midi_file`.
- `midi-hardware` — `tutti::midi_hardware` (implies `midi`).
- `synth`, `soundfont`, `sampler`, `spatial`, `analysis`, `modulation` — the
  matching module. `soundfont` implies `midi`; `sampler` implies `wav`.
- `convolution` — the convolution reverb nodes in `tutti::nodes`.
- `plugin` — `tutti::plugin` (implies `midi`), with one or more formats:
  `vst2`, `vst3`, `clap`, `au`. Each format links its SDK, which is why none
  is in `full`. The out-of-process plugin server is a separate binary,
  `tutti-plugin-server`, built on its own.

## This crate holds no code

Every item is a `pub use` of another crate, and the crate's own test enforces
it. A convenience constructor or wrapper belongs in the crate that owns the
thing it wires — device bootstrap in `tutti-cpal`, graph logic in
`tutti-graph` or `tutti-core` — where both `tutti` and `bevy-tutti` get it.

## License

MIT OR Apache-2.0
