# tutti-export

Offline rendering and audio export for the Tutti audio engine.

## What this is

Render a Tutti audio graph to a file or to in-memory buffers, with a full
mastering chain in between:

1. **Render** — drive the graph block-by-block into stereo `f32` buffers
   (buffered) or pipe them directly through an encoder (streaming, constant
   memory).
2. **Process** — resample, normalize (peak or EBU R128), dither, and
   optionally downmix to mono.
3. **Encode** — write WAV, FLAC, AIFF, or OGG Vorbis.

Uses [hound](https://crates.io/crates/hound) for WAV,
[flacenc](https://crates.io/crates/flacenc) for FLAC,
[vorbis_rs](https://crates.io/crates/vorbis_rs) for OGG,
[rubato](https://crates.io/crates/rubato) for resampling, and
[ebur128](https://crates.io/crates/ebur128) for loudness metering.

## Quick start

```rust,ignore
use tutti_export::{Export, Normalize, BitDepth};

// From a Tutti graph:
Export::graph(net, 44100.0)
    .duration_seconds(10.0)
    .bit_depth(BitDepth::Int24)
    .normalize(Normalize::lufs(-14.0))
    .to_file("master.flac")   // format inferred from extension
    .run()?;

// From already-rendered buffers:
Export::buffers(left, right, 44100.0)
    .to_file("clip.wav")
    .run()?;
```

## Execution modes

Every terminal returns a `Run<T>`. Pick one:

```rust,ignore
// Block this thread.
Export::graph(net, 44100.0)
    .duration_seconds(3.0)
    .to_file("out.wav")
    .run()?;

// Block with a progress callback.
Export::graph(net, 44100.0)
    .duration_seconds(600.0)
    .to_file("long.flac")
    .run_with(|phase, progress| {
        eprintln!("{:?} {:.0}%", phase, progress * 100.0);
    })?;

// Spawn a worker thread; poll from your UI loop.
let mut handle = Export::graph(net, 44100.0)
    .duration_seconds(3600.0)
    .to_file("podcast.wav")
    .spawn();

loop {
    match handle.poll() {
        tutti_export::State::Running { phase, progress } => { /* update UI */ },
        tutti_export::State::Done(file)                  => break file,
        tutti_export::State::Failed(e)                   => return Err(e.into()),
        tutti_export::State::Pending                     => {},
    }
}
```

## Streaming (constant-memory) exports

```rust,ignore
Export::graph(net, 44100.0)
    .duration_seconds(28_800.0)   // 8 hours
    .stream_to_file("longform.wav")
    .run()?;
```

Streaming currently supports WAV only. Normalization and resampling require
the full signal and return an error when combined with `.stream_to_file(...)`.

## Render to buffers

```rust,ignore
let rendered = Export::graph(net, 44100.0)
    .duration_seconds(1.0)
    .to_buffers()
    .run()?;
println!("{} samples @ {} Hz", rendered.left.len(), rendered.sample_rate);
```

## MIDI-driven offline render (`midi` feature)

```rust,ignore
use tutti_export::{Export, MidiTrack};

let mut midi = MidiTrack::new();
midi.note_on(0.0, synth_uid, 60, 0x8000)
    .note_off(1.0, synth_uid, 60);

Export::graph(net, 44100.0)
    .duration_beats(4.0, 120.0)
    .with_midi(midi)
    .to_file("rendered.wav")
    .run()?;
```

## Feature flags

- `wav` (default) — WAV encoding via hound (and BWAV, streaming).
- `flac` (default) — FLAC encoding via flacenc.
- `aiff` (default) — AIFF encoding (pure Rust, no external dependency).
- `ogg` (default) — OGG Vorbis encoding via vorbis_rs.
- `midi` — `MidiTrack` + `with_midi` for MIDI-driven exports.

## License

MIT OR Apache-2.0
