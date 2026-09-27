# tutti-cpal

The CPAL device layer: the seam between Tutti's audio graph and a sound card.

Device enumeration, output stream lifecycle, the real-time callback body, and
microphone capture. `tutti-core` knows how to render a block; this crate knows
how to open a device, hand it blocks on time, and pull audio back in from a
microphone. It is the only Tutti crate that links a platform audio API, so a
headless render, an offline export or a test never pulls one in.

Use it directly when you assemble the engine yourself; the `tutti` facade
re-exports it as `tutti::device` (feature `device`), and `bevy-tutti` drives
the same types from a Bevy plugin.

## Quick start

List the output devices:

```rust,no_run
use tutti_cpal::TuttiDriver;

# fn main() -> tutti_cpal::Result<()> {
for device in TuttiDriver::devices()? {
    println!("{}: {}", device.index, device.name);
}
# Ok(())
# }
```

Play a graph: open a device, build the graph at the device's rate, and hand
both to a [`TuttiDriver`] with [`TuttiDriver::from_parts`]:

```rust,no_run
use std::sync::Arc;
use tutti_core::graph::{OutPort, Source};
use tutti_core::{AudioTap, Engine, Hz, MasterMeter, NodeKey, Samples, Transport};
use tutti_graph::{Editor, Prepare};
use tutti_nodes::testing::Osc;
use tutti_cpal::{AudioCallbackState, AudioEngine, TuttiDriver};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// Open a device first: it reports the rate the graph must be built at.
let mut audio_engine = AudioEngine::new(None)?;
let sample_rate = audio_engine.sample_rate();

// The graph: an editor on the control thread, its executor for the audio
// thread. Edits are `commit`ted across.
let transport = Transport::new(sample_rate);
let (mut editor, executor) = Editor::new(Prepare::new(sample_rate, Samples(512)));
let tone = NodeKey(1);
editor.insert(tone, "tone", Osc::sine(Hz(440.0)));
editor.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: tone, port: 0 }); 2];
editor.commit()?;

// The engine owns the executor and the transport's clock; the editor stays
// with the host.
let engine = Engine::new(&transport, &mut editor, executor)?;
let state = Arc::new(AudioCallbackState::new(
    engine,
    MasterMeter::new(),
    AudioTap::new(),
));

audio_engine.start(Arc::clone(&state))?;
let driver = TuttiDriver::from_parts(audio_engine, state);
assert!(driver.is_running());
# Ok(())
# }
```

## Main types

- [`TuttiDriver`] — what a host holds: start, restart on another device or
  rate ([`TuttiDriver::restart_with`] re-rates the graph in a hook), stop.
- [`AudioEngine`] — one output device, its [`OutputSpec`], and the stream
  while it runs.
- [`AudioCallbackState`] and [`process_audio`] — what the output callback reads
  and runs each block.
- [`DeviceHost`], [`AudioHost`], [`DeviceSelector`] — choose a platform host
  (default or JACK) and a device on it, by index or by name.
- [`StreamFaults`] — backend errors (such as an unplugged device), which CPAL's
  error callback cannot return anywhere else.
- [`ManualStreamDriver`] — runs the same lifecycle with no device, the caller
  rendering each callback; for tests.
- `MicIn` (feature `capture`) — a microphone as a `tutti_core::io::AudioIn`,
  optionally with a live-monitor graph node.

## Real-time behaviour

The output callback runs on CPAL's audio thread and does not allocate, lock or
block. Its buffers are sized once at stream start to [`MAX_FRAMES`] frames; a
larger callback is clamped and its tail silenced rather than reallocating. Every
other method runs on the control thread.

This crate does not own the graph (rendering a block is `tutti-core`'s) or
recording (the pump that drives a mic into a file is `tutti_io::Recorder`,
which is device-free).

## Features

No features are enabled by default.

- `capture` — microphone capture (`MicIn`), using `tutti-io`'s ring and
  monitor node. Without it the crate is output-only.
- `jack` — the JACK host ([`AudioHost::Jack`]). Linux and the BSDs only, and
  needs libjack at build time; elsewhere, or without the feature, opening JACK
  returns [`Error::HostUnavailable`].

## License

MIT OR Apache-2.0
