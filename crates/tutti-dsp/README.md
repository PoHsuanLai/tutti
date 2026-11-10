# tutti-dsp

Pure DSP algorithms over `&[f32]` for the Tutti audio engine: a CPU-independent
real FFT and the algorithms behind the built-in effect nodes: filters, dynamics,
the partitioned convolver, delay lines, reverb, waveshapers and the export
resampler. The analyzers (loudness, pitch, onsets, spectra, ...) are `tutti-measure`,
built on this crate's FFT, windows, `StftGeometry` and `Error`.

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
- **No threading and no scheduling.** A host that wants these results on a
  background thread owns that plumbing itself.

## Features

None: `default = []`.

The `tutti` crate re-exports this one as `tutti::dsp` behind its `dsp` feature. `tutti-nodes`, `tutti-sampler`, `tutti-spatial` and
`tutti-export` use it.

## License

MIT OR Apache-2.0
