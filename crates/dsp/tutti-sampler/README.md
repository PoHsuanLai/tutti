# Tutti Sampler

Sample playback, disk streaming, audio input, and recording.

## What this is

Handles file playback and audio recording for DAW applications. Butler thread loads audio from disk asynchronously using ring buffers. Supports audio input from hardware, MIDI/audio/automation recording, time-stretching via phase vocoder, and SoundFont synthesis.

Uses [cpal](https://crates.io/crates/cpal) for audio I/O, [hound](https://crates.io/crates/hound) for WAV files, [symphonia](https://crates.io/crates/symphonia) for other formats, and [rustysynth](https://github.com/PoHsuanLai/rustysynth) for SoundFont.

## Quick Start

```rust
use tutti_sampler::Sampler;

let sampler = Sampler::builder(44100.0).build()?;

// Stream an audio clip to channel 0
sampler.channel(0).play("audio.wav").speed(1.0).start();

// Record to disk
let session = sampler.record("out.wav").channels(2).start();
// ... audio callback writes via session.producer_mut() ...
session.stop();
```

## Features

- `soundfont` - SoundFont synthesis support

## License

MIT OR Apache-2.0
