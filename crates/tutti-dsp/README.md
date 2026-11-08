# tutti-dsp

Pure DSP algorithms over `&[f32]` for the Tutti audio engine: a CPU-independent
real FFT, audio analysis (waveform blocks, onsets, pitch, loudness, stereo
correlation, spectral shape, silence regions and vectorscope points), and the
algorithms behind the built-in effect nodes: filters, dynamics, the partitioned
convolver, delay lines and waveshapers.

Use it for offline or background analysis of audio you already hold as
samples: drawing a waveform, finding transients, tracking pitch, measuring
loudness or checking a stereo image. The algorithms have no opinion about
where the results go:

- `summarize` — min/max/RMS waveform blocks for a timeline, **per channel**.
  Folding to one series is `PeakBlocks::to_mono`, a caller's choice rather than
  this crate's default.
- `detect_onsets` — onset detection over four selectable detection functions.
- `yin` — monophonic pitch estimation (de Cheveigné & Kawahara, 2002).
- `correlate` — inter-channel phase correlation and stereo image.
- `measure_loudness` — EBU R128 integrated loudness, loudness range and true
  peak.
- `step_windowed_loudness` — momentary (400 ms) and short-term (3 s) loudness and
  true peak of a running stream, after every chunk.
- `step_spectrum` — a streaming spectrum: power in dB per bin, or per
  third-octave or log band, with smoothing, peak hold and a rolling
  spectrogram.
- `spectral_features` — centroid, flatness and rolloff of every STFT frame.
- `strip_silence` — the sample ranges that hold signal, with a minimum silence
  length and padding.
- `vectorscope_points` (`step_vectorscope` for blocks) — a stereo pair as
  (side, mid) points for a goniometer.
- `cross_correlate` — FFT cross-correlation of two buffers at every lag.
- `align` — GCC-PHAT delay between two takes: lag, sub-sample offset, confidence.
- `estimate_tempo` / `track_beats` — tempo (BPM, confidence, octave alternatives) by
  autocorrelation under a log-Gaussian prior, and beat positions by dynamic
  programming (Ellis 2007); `_from_novelty` forms take your own detection curve.
- `transcribe` — monophonic audio to notes: a pitch (`Note` plus a cents bend),
  a span in samples and a level, split on onsets and pitch changes;
  `notes_from_track` takes your own pitch track.
- `chroma` / `step_chroma` — harmonic pitch class profile (Gomez 2006) of spectral
  peaks, with an estimated tuning in cents; `estimate_key` — Krumhansl-Schmuckler
  key with a confidence and a runner-up; `detect_chords` — major and minor
  triads smoothed by Viterbi, as spans in samples.
- `stft` / `istft_transform` — the short-time Fourier transform, in three result
  types so invertibility is a compile-time question.

## Effect algorithms

`tutti-nodes` wraps each of these in a graph node that owns the params, the
ramps and the graph shape. Here they take plain typed values per call.

- **Filters** — `SvfCoeffs` and `SvfCoefficients<F>` with `svf_step`
  (Simper's trapezoidal state-variable filter, eight responses by `SvfType`);
  `LadderCoeffs` and `LadderCoefficients<F>` with `ladder_step` (the Moog
  ladder, `tanh` feedback); `Crossover`, a 2 to 6 band Linkwitz-Riley 4th-order
  split with ramped frequency changes. `Real` is the `f32` or `f64` the state
  runs in; its transcendentals call `libm`, so results do not depend on the
  platform's C math library.
- **Dynamics** — `EnvelopeFollower` and `GateEnvelopeFollower`; the curves
  `compute_compressor_gain_reduction`, `compute_gate_gain`,
  `compute_limiter_gain` and `LimiterCurve`; `CompressorKernel` and
  `GateKernel`, whose `gain` takes a `CompressorBlock` or `GateBlock`;
  `LookaheadRing`, the limiter's delay with its sliding gain minimum; and
  `CircularBuffer` and `MonotonicMinDeque`.
- **Convolution** — `Convolver`, a uniform or non-uniform partitioned FFT
  convolution over an IR's shared, read-only `IrSpectra`.
- **Delay** — `DelayLine`, a fractional-read ring with an `InterpolationMode`,
  and `SweptLines`, the chorus and flanger kernel (the LFO's sine solved every
  `COEFF_INTERVAL` samples and interpolated between).
- **Waveshaping** — `ShapeKind`, six memoryless curves with block kernels.

FFT is `RealFft`, the engine's one real-input FFT: rustfft's
*scalar* planner under realfft's even-length real passes (an odd size runs a
full-length complex transform). Its arithmetic does not depend on the CPU, so
a spectrum, a pitch or an onset is bit-identical on every CPU on the same OS,
libm and rustfft version (the twiddles call the C library's `sin` and `cos`, so not across
libms). `tutti-sampler`'s vocoder and `tutti-nodes`' convolver use the same
transform, so values cross freely as `num_complex::Complex<f32>`. This crate
plans lazily, per scratch, on the cold path.

## Scope

- **No graph, no framework, no ECS.** Nothing here is a graph node or knows
  about one, and there is no Bevy feature.
- **No threading and no scheduling.** "Live" is a property of a call site, never
  of an algorithm, so nothing here is named for it. A host that wants these
  results on a background thread owns that plumbing itself.
- **No display.** These return numbers, not pixels.

## Configs, and carries where they are needed

Every algorithm takes a **config**: immutable, validated once, so an invalid
combination fails at construction rather than silently producing nothing.

Six of them additionally need a **carry** between frames — onset detection
diffs against the previous spectrum, waveform blocking holds a partial block,
strip silence holds the region it is growing, chroma a running mean, windowed
loudness its meter, and the spectrum its ring of samples and its readings.
Those expose the carry as an explicit value and a `step` function, and their
batch entry points (`detect_onsets`, `summarize`, `strip_silence`) *fold that
same step*, so the two paths cannot drift. Tests pin the equality across chunk
sizes and channel layouts.

The rest are stateless: `yin`, `correlate`, `cross_correlate`,
`spectral_features` are pure functions of their input (the vectorscope's
`step_vectorscope` carries only its stride phase between blocks),
and `stft` is batch-only — there is no incremental transform. Meter ballistics
(`step_ballistics`) carries a smoothed reading, but that is a filter over results
rather than a step of the correlation itself.

## Quick start

```rust
use tutti_dsp::{
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
# Ok::<(), tutti_dsp::Error>(())
```

## Reading from a running graph

Nothing here knows about the graph, so the seam is a tap (`tutti-graph`'s
`Editor::open_tap`, or a session's `Graph::open_tap`): the audio thread copies
the sources into a ring a lane and a control thread drains them through the
`TapReader`. What arrives on this side is an ordinary `&[f32]`, which is the
whole reason these algorithms need no engine vocabulary.

```rust
use tutti_dsp::correlate;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Transport, Unforkable};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{Beat, Bpm, ChannelLayout, NodeKey, SampleRate, Samples, StereoPlanes};
use tutti_types::{TapAlign, TapId};

/// A stereo source: 0.5 on the left, -0.5 on the right.
struct Tone;
impl Node for Tone {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.5);
        io.output(1).fill(-0.5);
        Status::Modified
    }
    fn reset(&mut self) {}
}

// A tap on the graph's two outputs.
let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(64)));
editor.insert(NodeKey(1), "tone", Unforkable(Tone));
let out = |port| Source::Node(OutPort { node: NodeKey(1), port });
editor.spec_mut().topology.outputs = vec![out(0), out(1)];
let mut tap = editor.open_tap(TapId(0), vec![out(0), out(1)], TapAlign::ToOutput, Samples(256)).expect("the ports exist");
editor.commit().expect("compiles");
executor.apply_pending();
editor.collect();

// The audio-callback side: a block of 2 frames.
let (mut l, mut r) = ([0.0f32; 2], [0.0f32; 2]);
let transport = Transport::new(true, Bpm::of(120.0), Beat(0.0), None);
executor.process(2, &transport, &[], &mut [&mut l[..], &mut r[..]]);

// The analysis side: two interleaved frames, deinterleaved.
let mut drained = [0.0f32; 4];
assert_eq!(tap.pop_into(&mut drained), Samples(2));
let (left, right) = ([drained[0], drained[2]], [drained[1], drained[3]]);
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

`Result`, not `assert!`, throughout: a caller feeding a short buffer gets an
error rather than a panic.

## Allocation

The batch entry points allocate their results, so call them off the audio
thread. The `step_*` functions and `FftScratch` let a caller reuse buffers
across calls. `step_spectrum` allocates nothing once its state is built, and
`step_windowed_loudness` allocates nothing either, but reads three-second windows
on every call, so it belongs on the consumer side of a tap's `TapReader`, not in an
audio callback.

## Features

None: `default = []`.

The `tutti` crate re-exports this one as `tutti::dsp` behind its
`dsp` feature. `tutti-export` uses it for loudness measurement and
`tutti-sampler` for its analysis needs.

## License

MIT OR Apache-2.0
