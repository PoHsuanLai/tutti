# tutti

The audio engine behind one dependency, with no Bevy anywhere.

The engine is a set of focused crates. This one re-exports them so a headless
consumer — a CLI, a renderer, a server, a test — names one dependency instead
of a dozen. If you are writing a Bevy app, take
[`bevy-tutti`](../bevy-tutti) instead; it is the same engine plus the ECS
adapter.

```toml
[dependencies]
tutti = { git = "…", features = ["export", "analysis", "wav"] }
```

```rust
use tutti::dsp::Net;
use tutti::nodes::testing::Osc;
use tutti::prelude::*;

// A graph, rendered offline, with no device and no Bevy.
let mut net = Net::new(0, 2);
let tone = net.push(Box::new(Osc::sine(Hz(440.0))));
net.pipe_output(tone);

let engine = tutti::core::Engine::new(
    tutti::core::MotionFsm::new(tutti::core::TransportSettings::new()),
    net.backend(),
);
let mut out = vec![0.0f32; 256 * 2];
engine.process(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));

assert!(out.iter().any(|&s| s != 0.0));
# // Keep the net alive: the backend borrows through it.
# std::mem::forget(net);
```

## What the modules are

| module | crate | needs |
|---|---|---|
| `tutti::core` | `tutti-core` | always |
| `tutti::types` | `tutti-types` | always |
| `tutti::dsp` | FunDSP, via `tutti-core` | always |
| `tutti::graph` | `tutti-graph` | always |
| `tutti::nodes` | `tutti-nodes` | always |
| `tutti::node` | `tutti-node` | always |
| `tutti::device` | `tutti-cpal` | `device` |
| `tutti::io` | `tutti-io` | `io` (implied by `audio-io` and by every codec) |
| `tutti::export` | `tutti-export` | `export` |
| `tutti::sampler` | `tutti-sampler` | `sampler` |
| `tutti::polysynth` | `tutti-polysynth` | `synth` |
| `tutti::soundfont` | `tutti-soundfont` | `soundfont` |
| `tutti::spatial` | `tutti-spatial` | `spatial` |
| `tutti::analysis` | `tutti-analysis` | `analysis` |
| `tutti::modulation` | `tutti-mod` | `modulation` |
| `tutti::midi`, `::midi_runtime`, `::midi_file` | the `midi/` crates | `midi` |
| `tutti::midi_hardware` | `tutti-midi-hardware` | `midi-hardware` |
| `tutti::plugin` | `tutti-plugin` | `plugin` (+ a format) |

`full` turns on everything except the plugin formats, which stay opt-in
because each links an SDK.

## Examples

- `examples/headless_export.rs` — render a graph to a file and measure its
  loudness. The same program as `tutti-export`'s own `examples/export.rs`,
  which names five crates directly; here it names one.
- `examples/headless_engine.rs` — open a device and start the audio thread
  with no ECS. This exists nowhere else in the repo: the only other bootstrap
  is `bevy_tutti::engine::build`, which is 388 lines of Bevy systems.

## One rule

**This crate contains no code.** Every item is a `pub use`. See the module
docs for why that is load-bearing and what happened the last time a package
of this name held logic.
