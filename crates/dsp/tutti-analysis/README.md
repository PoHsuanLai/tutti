# tutti-analysis

Audio analysis over `&[f32]`: waveform blocks, onsets, pitch, loudness and
stereo correlation.

## What this is

Algorithms, and no opinion about where the results go:

- `summarize` — min/max/RMS waveform blocks for a timeline, **per channel**.
  Folding to one series is `PeakBlocks::to_mono`, a caller's choice rather than
  this crate's default.
- `detect_onsets` — onset detection over four selectable detection functions.
- `yin` — monophonic pitch estimation (de Cheveigné & Kawahara, 2002).
- `correlate` — inter-channel phase correlation and stereo image.
- `measure_loudness` — integrated loudness.
- `stft` / `istft_transform` — the short-time Fourier transform, in three result
  types so invertibility is a compile-time question.

FFT is [rustfft](https://crates.io/crates/rustfft)'s. That is deliberate and
differs from the realtime side of the engine, where `tutti-sampler`'s vocoder
calls [microfft](https://crates.io/crates/microfft)'s fixed sizes directly:
analysis windows are arbitrary-size and cold-path, so rustfft's planner
and SIMD are the right trade, where microfft's allocation-free fixed sizes are
what the realtime graph needs and this crate does not. Both are
`num_complex::Complex<f32>` underneath, so values cross freely.

## What it does not own

- **No graph, no framework, no ECS.** Nothing here knows about `Net`, and there
  is no Bevy feature. The live-analysis ECS surface this crate once carried
  computed four analyses nobody read, and was removed rather than gated.
- **No threading and no scheduling.** "Live" is a property of a call site, never
  of an algorithm, so nothing here is named for it. A host that wants these
  results on a background thread owns that plumbing itself.
- **No display.** These return numbers, not pixels.

## Configs, and carries where they are needed

Every algorithm takes a **config**: immutable, validated once, so an invalid
combination fails at construction rather than silently producing nothing.

Two of them additionally need a **carry** between frames — onset detection diffs
against the previous spectrum, and waveform blocking holds a partial block.
Those two expose the carry as an explicit value and a `step` function, and their
batch entry points (`detect_onsets`, `summarize`) *fold that same step*, so the
two paths cannot drift. Tests pin the equality across chunk sizes and channel
layouts.

The rest are stateless: `yin` and `correlate` are pure functions of their input,
and `stft` is batch-only — there is no incremental transform. Meter ballistics
(`step_ballistics`) carries a smoothed reading, but that is a filter over results
rather than a step of the correlation itself.

## Quick start

```rust
use tutti_analysis::{
    correlate, detect_onsets, summarize, yin, DetectionFunction, FftScratch,
    OnsetConfig, PeakConfig, StftGeometry, YinConfig,
};
use tutti_types::{ChannelLayout, Interleaved, Samples, StereoPlanes};

let sample_rate = 44100.0;
let samples: Vec<f32> = vec![0.0; 44100];
let mut fft = FftScratch::new();

// Waveform blocks for display — one series per channel.
let blocks = summarize(
    &PeakConfig::new(Samples(512), ChannelLayout::STEREO),
    Interleaved::new(&samples, ChannelLayout::STEREO),
);
let left = blocks.channel(0).expect("stereo has a channel 0");
// A meter wants one number per block; a waveform draws both channels.
let merged = blocks.to_mono();

// Onsets, via spectral flux.
let geometry = StftGeometry::new(sample_rate, Samples(2048), Samples(512))?;
let onsets = detect_onsets(
    &OnsetConfig::new(geometry, DetectionFunction::SpectralFlux),
    &samples,
    &mut fft,
)?;

// Pitch. An inverted range is refused here, not silently unvoiced later.
let pitch = yin(&YinConfig::standard(sample_rate)?, &samples)?;

// Stereo correlation. The planes are paired once — a length mismatch is
// refused here rather than silently truncated inside the measurement.
let planes = StereoPlanes::new(&samples, &samples).expect("equal lengths");
let reading = correlate(planes);
# Ok::<(), tutti_analysis::Error>(())
```

## Reading from a running graph

Nothing here knows about the graph, so the seam is a `tutti_core::AudioTap`: the
audio thread pushes each block into it and a control thread drains it. What
arrives on this side is an ordinary `&[f32]`, which is the whole reason these
algorithms need no engine vocabulary.

```rust
use tutti_analysis::correlate;
use tutti_core::AudioTap;
use tutti_types::StereoPlanes;

let tap = AudioTap::new();
let _consumer = tap.open().expect("a fresh tap has no consumer");

// The audio-callback side. `frames` is a FRAME count, so an interleaved
// stereo block of 2 frames is 4 samples.
let block = [0.5f32, -0.5, 0.5, -0.5];
tap.push(&block, 2);

// The analysis side, once the drained frames are deinterleaved. Draining the
// ring itself needs `ringbuf`'s `Consumer` trait, which is the consumer's
// dependency rather than this crate's.
let (left, right) = ([0.5f32, 0.5], [-0.5f32, -0.5]);
let planes = StereoPlanes::new(&left, &right).expect("drained in lockstep");

// `Correlation` is a MEASUREMENT type, deliberately distinct from the control
// types (`Mix`, `Depth`) despite the coinciding range.
let reading = correlate(planes);
```

## Constraint: an invalid combination is refused at construction, never mid-measurement

The config constructors are where `Error` comes from — a hop that exceeds or does
not COLA-divide its window, an inverted or above-Nyquist frequency range, a
non-positive sample rate. Refusing there is the point: a detector that *accepted*
an inverted range would report "unvoiced" forever instead, which is a silent
wrong answer rather than an error.

The measurement entry points are correspondingly thin on failure. `correlate` and
`summarize` are infallible — the pairing and layout checks already happened, in
`StereoPlanes::new` and `PeakConfig`. `measure_loudness` returns `Option`,
because too little audio to gate is an absence rather than a fault. Only `yin`
and `detect_onsets` return `Result`, and both do so before touching a sample.

`Result`, not `assert!`, throughout: a caller feeding a short buffer deserves
better than a panic on the audio path.

## Features

`default = []`.

- `serde` — pulls the `serde` dependency. Note it currently derives on exactly
  one type, `pitch::PitchResult`, which is `pub(crate)`: the flag has **no
  observable effect on the public API today**. Turning it on for a public result
  type is a live gap, not a documented capability.

There is no `bevy` feature, and none for caching: a thumbnail cache is a host's
storage policy, not an algorithm.

## License

MIT OR Apache-2.0
