# tutti-cpal

The CPAL device layer: the seam between Tutti's DSP graph and a sound card.

## What this is

Device enumeration, stream construction, the RT callback body, and mic capture.
`tutti-core` knows how to render a block; this crate knows how to open a device,
hand it blocks on time, and pull audio back in from a microphone.

```rust,no_run
use tutti_cpal::TuttiDriver;

# fn main() -> tutti_cpal::Result<()> {
for device in TuttiDriver::devices()? {
    println!("{}: {}", device.index, device.name);
}
# Ok(())
# }
```

A driver is built from an opened device plus the state its callback reads
(`TuttiDriver::from_parts`); a host assembles those once at startup:

```rust,no_run
use std::sync::Arc;
use tutti_core::dsp::Net;
use tutti_core::{AudioTap, Engine, Hz, MasterMeter, Transport, TransportClock};
use tutti_nodes::testing::Osc;
use tutti_cpal::{AudioCallbackState, AudioEngine, TuttiDriver};

# fn main() -> tutti_cpal::Result<()> {
// Open a device first: it reports the rate the graph must be built at.
let mut audio_engine = AudioEngine::new(None)?;
let sample_rate = audio_engine.sample_rate();

let transport = Transport::new(sample_rate);
let mut net = Net::new(0, 2);
net.push(Box::new(TransportClock::new(
    transport.clock_links(),
    sample_rate,
)));
let tone = net.push(Box::new(Osc::sine(Hz(440.0))));
net.pipe_output(tone);

// `backend()` is the audio thread's half of the graph; the control thread
// keeps `net` and `commit`s edits across to it.
let engine = Engine::new(transport.motion.clone(), net.backend());
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

Both blocks are `no_run` rather than runnable: every line type-checks, but
`AudioEngine::new` and `TuttiDriver::devices` open or enumerate real sound
cards, which a test runner has no business doing.

## What this crate does not own

- **The graph.** Rendering a block is `tutti-core`'s. This crate only decides
  when a block happens and where it goes.
- **Recording.** The pump that drives a mic into a file is `tutti_io::Recorder`,
  which is device-free and sits one layer **down** — see
  [`tutti-io`](../tutti-io). The producer half of the mic ring lives here, in
  the input callback; the consumer half and the monitor node come from there.
- **A lifecycle.** Everything here is framework-free: a host that wants a
  different lifecycle — a Bevy `App`, a CLI, a test harness — drives these types
  itself. Only `bevy-tutti` depends on this crate, and only to own the driver's
  lifecycle.

**It is the only place CPAL is linked.** Everything else in the engine renders
into buffers without knowing where they go, so a headless render, an offline
export or a test harness never pulls in a platform audio API.

## RT discipline

`process_audio` runs on the audio thread and must not allocate, lock, or block.
Its buffers are sized once at stream build (to `MAX_FRAMES`) and never resized —
an over-sized callback is clamped and its tail silenced rather than triggering a
reallocation. `tests/rt_no_alloc.rs` gates this against a disabled allocator;
that file's own docs explain why the gate has to live in `tests/` rather than
beside the code.

The callback is deliberately a free function taking `AudioCallbackState` rather
than a method on the driver, so a test can call exactly what CPAL calls without
opening a device.

## Features

`default = []`.

- `capture` — mic capture (`MicIn`). Without it the crate is output-only.
- `midi` — pre-block MIDI delivery inside the render callback. Off, the callback
  renders the graph and nothing else.

## License

MIT OR Apache-2.0
