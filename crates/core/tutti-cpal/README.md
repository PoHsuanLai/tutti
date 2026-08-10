# tutti-cpal

The CPAL device layer: the seam between Tutti's DSP graph and a sound card.

## What this is

Device enumeration, stream construction, the RT callback body, and mic capture.
`tutti-core` knows how to render a block; this crate knows how to open a device,
hand it blocks on time, and pull audio back in from a microphone.

```rust,no_run
use tutti_cpal::TuttiDriver;

for device in TuttiDriver::devices()? {
    println!("{}: {}", device.index, device.name);
}
# Ok::<(), tutti_cpal::Error>(())
```

A driver is built from an opened device plus the state its callback reads
(`TuttiDriver::from_parts`); a host assembles those once at startup.

## Why it is its own crate

**It is the only place CPAL is linked.** Everything else in the engine renders
into buffers without knowing where they go, so a headless render, an offline
export or a test harness never pulls in a platform audio API. The crate is also
framework-free: a host that wants a different lifecycle — a Bevy `App`, a CLI, a
test harness — drives these types itself.

The pump that drives a mic into a file is *not* here. That is
`tutti_io::Recorder`, which is device-free and sits one layer **down** — see
[`tutti-io`](../tutti-io).

## Where it sits

Depends on `tutti-core` (the graph it renders), optionally on `tutti-io` (the
capture ring and monitor node) and `tutti-midi-runtime` (pre-block MIDI). Only
`bevy-tutti` depends on it, and only to own the driver's lifecycle.

## Features

`default = []`.

- `capture` — mic capture (`MicIn`). Without it the crate is output-only. The
  producer half of the mic ring lives in this crate's input callback; the
  consumer half and the monitor node come from `tutti-io`.
- `midi` — pre-block MIDI delivery inside the render callback. Off, the callback
  renders the graph and nothing else.

## RT discipline

`process_audio` runs on the audio thread and must not allocate, lock, or block.
Its buffers are sized once at stream build (to `MAX_FRAMES`) and never resized —
an over-sized callback is clamped and its tail silenced rather than triggering a
reallocation. `tests/rt_no_alloc.rs` gates this against a disabled allocator.

The callback is deliberately a free function taking `AudioCallbackState` rather
than a method on the driver, so a test can call exactly what CPAL calls without
opening a device.

## License

MIT OR Apache-2.0
