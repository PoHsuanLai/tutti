# tutti-dsp

Pure DSP algorithms over `&[f32]` for the Tutti audio engine: a CPU-independent
real FFT and the algorithms behind the built-in effect nodes: filters, dynamics,
the partitioned convolver, delay lines, reverb, waveshapers and the export
resampler. The analyzers (loudness, pitch, onsets, spectra, ...) are `tutti-measure`,
built on this crate's FFT, windows and `StftGeometry` (whose refusals are `Error`).

## Effect algorithms

`tutti-nodes` wraps each of these in a graph node that owns the params, the
ramps and the graph shape. Here they take plain typed values per call.

- **Filters** — `SvfCoeffs` and `SvfDesign<F>` (Simper's trapezoidal
  state-variable filter, eight responses by `SvfType`);
  `LadderCoeffs` and `LadderDesign<F>` (the Moog
  ladder, `tanh` feedback); `CrossoverDesign` and `Crossover`, a 2 to 6 band Linkwitz-Riley 4th-order
  split (the design on a 16-frame grid, the state with ramped frequency changes). The three
  designs are one `Filter<K>` over a `ChannelKernel`, realized by `Realized<PerChannel<K>>`. `Real` is the `f32` or `f64` the state
  runs in; its transcendentals call `libm`, so results do not depend on the
  platform's C math library.
- **Dynamics** — `EnvelopeFollower` and `GateEnvelopeFollower`; the curves
  `compute_compressor_gain_reduction`, `compute_limiter_gain` and
  `LimiterCurve`; `CompressorKernel` and
  `GateKernel`, whose `gain` takes a `CompressorBlock` or `GateBlock`;
  and `LookaheadRing`, the limiter's delay with its sliding gain minimum.
- **Convolution** — `Convolver`, a uniform or non-uniform partitioned FFT
  convolution over an IR's shared, read-only `IrSpectra`.
- **Delay** — `DelayLine`, a fractional-read ring with an `InterpolationMode`,
  and `SweptLines`, the chorus and flanger kernel (the LFO's sine solved every
  `COEFF_INTERVAL` samples and interpolated between).
- **Waveshaping** — `ShapeKind`, six memoryless curves with block kernels.
- **Reverb** — the reverb model of design 022, a `Network` with analytic queries
  (RT60, tail, stability, densities), and two designs over it: `FreeverbDesign` and
  `FdnDesign`, the feedback delay network.
- **Spatial** — vector base amplitude panning (`SpeakerLayout`, `VbapKernel`) and the
  HRTF binaural convolver (`HrtfBinaural`).
- **Resampling and dither** — `FftResampler` and `StreamingResampler` for export, and
  `DitherNoise` with `DitherSink`, the dither path export and recording share.

Each family is laid out as its design (controls to a model), its model (a `System`, and
an `Lti` where it is linear and time-invariant) and its kernel (the per-frame math that
`Realized` runs), the split of design 024.

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
- **No threading and no scheduling.** A host that wants these results on a
  background thread owns that plumbing itself.

## Features

- `contract` (off by default): the `contract` module, the cases a `Design` and its
  `Realization` are held to (design 024 section 6). Test support.

The `tutti` crate re-exports this one as `tutti::dsp` behind its `dsp` feature. `tutti-nodes`, `tutti-sampler` and
`tutti-export` use it.

## License

MIT OR Apache-2.0
