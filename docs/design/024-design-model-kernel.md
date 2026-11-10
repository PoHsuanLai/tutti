# 024: design, model, kernel

[Doc 022](022-reverb.md) gave the reverb a three-part shape: a **design** (a pure
function from typed controls to a model value), a **model** (a value with
analytic queries: ring time, stability, latency, bounds) and a **kernel** (the
hand-written runtime that realises the model), with a contract suite that holds
each design to the model's invariants and each kernel to the model. PR #367
(branch `feat/reverb-kernel`) adds the generic node around it, `ReverbNode<D>`:
redesign in place on the audio thread, the tail report, `longest()`, parking and
fork.

Nothing in that shape is about reverb. A filter has a response that its poles and
zeros give exactly; a compressor has a static curve; a resampler has a kernel
whose latency and stopband are known before it runs; a waveshaper has a transfer
curve whose harmonics follow from it. Each is a pure function of controls to a
description, run by a kernel that is held to the description. This design says
which of tutti's algorithms the shape covers, which traits are shared when it is
generalised, what the generic node and the contract machinery become, and in what
order. It is also the producer side of two gaps in
[doc 023](023-plugin-capabilities.md): G8 (a curve response) and G9 (a readout),
with G5 (quality) as a design input.

**The owner's constraint, which shapes everything below** (2026-11-09, on
generalising the reverb traits): the abstraction must not force a runtime
mechanism that makes algorithms hard to optimise or gives them a suboptimal
fixed shape to write; tutti hit that once in older versions of the modulation
crate (doc 022 section 4.1, doc 014 section 1). So **the traits, the node and the
contract machinery are shared; the model is not.** Each family keeps its own
model type with its own fields and its own queries. There is no universal model,
no generic interpreter, and no per-sample dispatch.

How to read this against the code: everything on `main` is cited by path; the
reverb node, `Kernel`, `Setup`, `Effort` and the kernel contract are on branch
`feat/reverb-kernel` (head `474d7621c`, PR #367, not merged) and are cited as
"on #367". Types this document proposes are marked **(proposal)**. The traits in
section 4 were type-checked in a scratch crate (the three families of section
4.8 implement them, and a case bounded on `Response` refuses a model without it);
nothing else here has been built, and no figure is measured except those doc 022
already measured.

## Summary

- **Inventory (section 3): 45 rows. 20 fit, 11 fit in part, 14 do not.** The
  filters, the dynamics, the delay, the convolver, the waveshaper, the panners
  (VBAP, HRTF), the envelopes and oscillators, and the channel folds fit. Analysers,
  the FFT, the buffers, the click, the automation lanes and the mix bus do not,
  and section 3 says why for each.
- **Share twelve small traits and no model** (section 4): `Design` (with an
  associated `Model`), `Model`, `Kernel<M>`, `Effect<M>` (the planar block shape),
  and capability traits for the queries: `Latent` and `Ringing` (every model
  answers these; every node declares both in its `Shape`), then `Stable`,
  `Response`, `StaticCurve`, `Readout` (on the kernel, not the model), `Fidelity`
  and `Graded` (quality) where a family has the answer.
  A contract case is a function bounded on its capability, so a case exists
  exactly where the query does (checked in the scratch crate).
- **`ReverbNode<D>` becomes `DesignNode<D>`** (section 5), with the family
  differences as associated types of a `NodeDesign` trait, not runtime flags:
  `Blend` (dry/wet or none), `Readouts` (none or a kernel's readout) and a
  `CADENCE` constant (how often a design is solved inside a block).
- **The first finding against the reverb-only node: the cadence.** `retarget`
  once per block is right for a reverb and wrong for the filters, which solve
  coefficients every `COEFF_INTERVAL` = 16 frames inside a block
  (`tutti-dsp/src/control.rs`), and for the three dynamics nodes, which accept a
  per-frame threshold lane. Forcing a per-block redesign on them would regress
  them. Section 4.5 makes the cadence a constant of the design, and section 11
  (Q3) says where it stops: a per-frame lane is a second, optional kernel shape,
  and a node that needs it keeps its hand-written `process` and adopts only the
  model and the contract.
- **The contract machinery** (section 6) moves to a public `contract` module of
  `tutti-dsp` behind a `contract` feature (the pattern of
  `tutti-graph/src/lib.rs`), so families in `tutti-polysynth`, `tutti-soundfont`
  and `tutti-spatial` run it. Cases are per capability, each with a mutant.
- **G8 and G9 fall out as queries** (section 7): a filter's `Response` and a
  compressor's `StaticCurve` are the curve; its `Readout` is the gain reduction. A
  native node and a hosted plugin meet in doc 023's `Display` and one
  `CurveResponse`.
- **Plan (section 10): six steps after #367, each a PR.** Generalise on reverb; filters
  (delivers G8); dynamics (delivers G9); then delay and modulation effects, the
  waveshaper, and convolution; then the resampler, the panners and the
  instruments' envelopes and oscillators.

## 1. Current state

What 022 built, and which of it is reverb in name only. Paths on `main` unless
marked "on #367".

| Piece | Where | Reverb-specific? |
|---|---|---|
| `Network`, `Stage`, `Inline`, the queries (`rt60`, `rt60_bound`, `tail`, `is_stable`, `is_lossless`, densities, `min_delay`, `latency`) | `tutti-dsp/src/network.rs`, `network/queries.rs` | The model: yes. `tail`, `is_stable` and `latency` are what every family has |
| `Ring { Finite(Seconds), Unbounded }` | `network/queries.rs` | No: every family's tail |
| `Design { Controls, design_into, design, bounds, channels }`; on #367 also `type Kernel`, `fn kernel` | `network/design.rs` | Names `Network` and `Bounds` (the reverb's) in its signatures |
| `Ceiling { rate, max_decay, floor }` | `network/design.rs` | `max_decay` is; `rate` and `floor` are not |
| `Bounds { lines, stages, taps, outputs, line, delay, ir, pre_delay, tail }`, `Bounds::of`, `dominates` | `network/design.rs` | Every field but `tail` |
| On #367: `Kernel`, `Setup { rate, max_block, input, output }`, `Effort` | `network/kernel.rs` | `Kernel` names `Network` and `Bounds`; `Setup` and `Effort` are not reverb at all |
| `design_contract!`, `DesignCase`, `Mutant`; on #367 `kernel_contract!`, `KernelCase`, `Defect` | `network/contract.rs`, `network/kernel_contract.rs` | Crate-private and `cfg(test)`; the cases mix generic ones (pure, bounds dominate, reset, fresh, block independence, click-free, latency, no allocation) with reverb ones (`rt60`, `band_order`, `decorrelated`, `line_lengths`, freeze) |
| On #367: `ReverbNode<D>`, `ReverbDesign`, `ReverbParams`, `DryAlign` | `tutti-nodes/src/reverb.rs`, `dry_align.rs` | The control path, tail report, `longest`, parking, cut-to-frame, fork and dry alignment are generic; the names are not |
| On #367: node tests | `tutti-nodes/tests/reverb_node.rs`, `reverb_node_generic.rs` | Generic cases through a lying design |

Two things the code already does that this design builds on rather than adds.
`SvfCoeffs::new` (`tutti-dsp/src/filter/svf.rs`) and
`compute_compressor_gain_reduction` (`dynamics/curves.rs`) are **already pure
functions from typed controls to a description**; the filter and the compressor
have a design today and no model type around it. And `MultibandControls::gain_reduction_db(band)`
(`tutti-nodes/src/dynamics/multiband.rs`) is **already a readout**; it is
reachable only through that one node's own handle.

## 2. The pattern, and what must not be forced

```
controls (typed units) --design--> model (a value, queries) <--held to-- kernel (runs)
        pure, total, allocation-free      reads no audio          hand-written, per family
```

The constraints, each of which is a test the design must pass in section 4.8:

1. **No universal model.** `Network`, a biquad, a compressor curve and a gain
   matrix have nothing in common but the capability traits they implement. A
   shared `enum AnyModel` or a bag of optional fields is the settings bag doc 022
   section 5.1 refused, in a new place.
2. **No generic interpreter.** No kernel runs "any model". Doc 022 section 4.2's
   opt-in `Generic` kernel stays a reverb-only idea and is not generalised here.
3. **No per-sample virtual call.** Every trait is used statically. `Design` is not
   object-safe (`type Controls`, `type Model`, `type Kernel`), and none of the
   capability traits is called from the audio thread through `dyn`. The node
   calls `Kernel::process` once per block (or per segment, section 4.5), the same
   dispatch frequency as `Node::process`.
4. **No fixed buffer shape.** `Kernel<M>` has no `process`: the block shape is a
   second trait (`Effect<M>`: planar slices in, planar slices out, equal length).
   A generator, a resampler that consumes and produces different counts, or an
   instrument that takes events has its own shape trait and its own node, and
   still uses `Design`, `Model`, `Kernel<M>` and the contract.
5. **The kernel reads what it wants of the model.** A model may carry more than a
   kernel uses (Freeverb ignores `width` and `wet`, doc 022 "Step 1b as built"),
   and two kernels may realise one model (a plain reference and a SIMD one).
6. **A hand-fused kernel stays hand-fused.** Freeverb's exact float order, the
   convolver's partition schedule, the polysynth's eight-lane SIMD bank
   (`tutti-polysynth/src/kernel.rs`) are the kernels. Each step's bit-identity
   test is the proof that moving to the shape did not touch them (section 10).
7. **A family's own cases are its own.** `rt60` and `decorrelated` are reverb
   cases and stay in the reverb's module; the generic contract does not grow a
   union of every family's checks.

## 3. The inventory

One row per algorithm. **Kernel today** says what runs now and whether its
controls are already typed units (`Param<Hz>` and the like, per CLAUDE.md "Unit
types"). **Fits** names the design (controls to model), the model and its exact
queries; **Partly** says what fits; **No** says why the shape is wrong. Counts
are in section 3.1.

### 3.1 Tutti-dsp: filters, dynamics, time-based

| # | Algorithm (where) | Kernel today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|
| 1 | **State-variable filter**, eight responses (`tutti-dsp/src/filter/svf.rs`; nodes `SvfFilterNode`, `EqBandNode` in `tutti-nodes/src/filter/`) | `svf_step` over `SvfCoefficients<F>`; coefficients solved every 16 frames and `lerp`ed (`control.rs`, `svf.rs` `segments`). Typed: `Param<Hz>`, `Param<Q>`, `Param<Db>` | **Fits** | Design: `(SvfType, Hz, Q, Db, rate)` to the model, which is `SvfCoeffs` (`a1..a3`, `m0..m2`, already pure) plus the rate. Queries: `Response` exactly (the trapezoidal SVF is the bilinear map of the analogue prototype with `g = tan(pi fc / fs)`, `k = 1/Q`; the contract proves the claim against the kernel's impulse response), poles and `Stable` (`k > 0`), `Ringing` from the pole radius (the slowest pole of `s^2 + k s + 1`), `Latent` zero. Bell and shelf gains are in the response. A series stack of `EqBandNode`s has the product response (Q4) |
| 2 | **Moog ladder**, LP12/LP24/HP12/HP24 (`filter/ladder.rs`, `LadderFilterNode`) | `ladder_step`, four stages with a `tanh` at the input (`ladder_step`), `LadderCoefficients`. Typed: `Hz`, `Resonance`, `Drive` | **Partly** | The small-signal model fits: design `(Hz, Resonance, rate)` to `(g1, k)`; `Response` of the linearised ladder (`(s/(1+s))^n` forms are in the type docs), `Stable` (resonance below the self-oscillation gain), `Ringing` (`Unbounded` at maximum resonance: it self-oscillates). The `tanh` makes the large-signal response drive-dependent, which `Response` does not claim: it is stated "small signal" and the contract measures it at an input level far below the `tanh` knee |
| 3 | **Linkwitz-Riley crossover**, 2 to 6 bands (`filter/crossover.rs`, `CrossoverNode`) | `Crossover::split_block`; design and state fused in one struct (`set_frequency` re-solves). Typed: `Hz` | **Fits** | Design: `(frequencies, rate)` to per-split `Split` coefficients (already a private type). Multi-output `Response` (`outputs() = bands`): LP/HP at `fc` are -6.02 dB and in phase, the bands sum to unity magnitude (the file's own header states it), and the sum is the Butterworth all-pass. `Stable`, `Ringing`, `Latent` zero. Needs design and state separated, which is the one real refactor |
| 4 | `Real` float trait (`filter/real.rs`) | `f32`/`f64` | **No** | A numeric abstraction below the shape. A kernel generic over `F: Real` is still one kernel |
| 5 | **Compressor** (`dynamics/compressor.rs`, `curves.rs`; `CompressorNode`) | `CompressorKernel` (envelope + `EnvelopeFollower`), `CompressorBlock { threshold, knee, ratio, makeup_from, makeup_to }`. Typed: `Db`, `CompressionRatio`, `Seconds` | **Fits** | Design: `(threshold, knee, ratio, makeup, attack, release, rate)` to a model holding the curve and the two one-pole coefficients. Queries: `StaticCurve` (`compute_compressor_gain_reduction`, `In = Db`, `Out = Db`: zero below `threshold - knee/2`, the quadratic over the knee, slope `1 - 1/ratio` above), time constants (step response reaches 63% in `attack`), `Ringing` zero (silent in, silent out; the node declares no tail, `compressor.rs` line 443), `Latent` zero. `Readout`: `CompressorKernel::gain_reduction_db` exists |
| 6 | **Gate** (`dynamics/gate.rs`; `GateNode`) | `GateKernel`, `GateEnvelopeFollower`, `GateBlock`. Typed: `Db`, `Seconds` | **Fits** | The settled curve is a step (range below the threshold, unity above: `gate_gain_at`), so `StaticCurve` is a degenerate one and the timing matters more: attack, hold and release as step-response queries. `Readout`: `is_open`/`gate_level` exist |
| 7 | **Lookahead limiter** (`dynamics/limiter.rs`, `curves.rs`; `LimiterNode`) | `LookaheadRing`, `MonotonicMinDeque` for the window minimum, `LimiterCurve`. Typed: `Db`, `Seconds` | **Fits** | `StaticCurve` (`LimiterCurve::gain`: gain `ceiling - threshold` up to the threshold, `ceiling/peak` past it, so output `<= ceiling`), `Latent` = the lookahead (the node declares it, `limiter.rs` line 340), `Ringing` zero, `Readout` gain reduction. The lookahead is a build parameter of the design |
| 8 | **Brickwall clip** (`limiter.rs` `brickwall_clip`; `BrickwallLimiterNode`) | one `clamp` | **Fits** | A memoryless `StaticCurve` over amplitude (`In = Out = f32`), `Latent` and `Ringing` zero. Trivial, and the same trait as the waveshaper (row 17) |
| 9 | **Multiband compressor** (`MultibandCompressorNode`) | crossover plus a `CompressorKernel` per band | **Partly** | A composite: the crossover model (row 3) and one compressor model per band. Per-band `StaticCurve` and `Readout` (`gain_reduction_db(band)` exists). The summed `Response` is not a single model query (it is signal dependent). Fits as a model that holds sub-models |
| 10 | **Envelope followers** (`dynamics/envelope.rs`; `EnvelopeFollowerNode` in `signal/follower.rs`) | `EnvelopeFollower`, `GateEnvelopeFollower`; the node outputs a control signal | **Partly** | A building block of rows 5 to 7, whose attack and release are their time-constant queries. The node is a measurement with a control output: its model is the two coefficients and a step response, with no response or curve to query |
| 11 | **Delay line** (`delay.rs`, `buffer.rs` `CircularBuffer`; `DelayLineNode`) | `DelayLine::read_sample` with `InterpolationMode`, a per-channel `Ramp` of the time. Typed: `Seconds`, `Feedback`, `Mix` | **Fits** | Design: `(time, feedback, mix, interpolation, rate)` to a model of delay in fractional frames and loop gain. Queries: `Bounds` (the maximum delay, a build parameter, `delay.rs`), `Ringing` (`time x trips to the floor`, as the all-pass term of doc 022 section 3.1), `Stable` (`|feedback| < 1`, `Feedback::MAX_STABLE`), `Response` (the comb `1/(1 - g z^-D)` for the frozen setting), `Latent` zero (musical delay is not latency). The interpolation's droop is a term of `Fidelity` |
| 12 | **Chorus and flanger** (`swept_delay.rs` `SweptLines`; `ModDelayNode` in `modulation/mod_delay.rs`) | `SweptLines::run`, LFO solved at control points and interpolated. Typed: `Seconds`, `Hz`, `Feedback`, `Mix`, `PhaseIncrement` | **Partly** | Fits: `Bounds` (`max_delay`), `Ringing` (`feedback_max` makes the tail exactly `max_delay` at none, per the type docs), `Stable`, the frozen comb `Response` at a sweep position, the LFO's period. Does not: a time-varying response has no single curve; the sidebands of a sinusoidal delay modulation (Bessel `J_n(beta)`) are a query only a chorus has |
| 13 | **Phaser** (`allpass.rs` `allpass_coeff`, `allpass_chain`; `PhaserNode`) | the swept first-order all-pass chain. Typed: `Hz` range, `Depth`, `Feedback`, `Mix` | **Partly** | Fits: the all-pass phase at a sweep position is exact, so the notch frequencies and depths for a mix are a `Response` of the frozen chain; `Stable` (feedback), `Ringing`. Does not: the sweep is time-varying, as row 12 |
| 14 | `interp.rs` `cubic_hermite`, `InterpolationMode` | four-point interpolation | **No** | A primitive below the shape. Its response is a term in rows 11 and 12 |
| 15 | **Reverb**, Freeverb and the `Network` (`reverb.rs`, `reverb_design.rs`, `network/`) | on #367: `Freeverb` is the kernel; `FreeverbControls { room, damp }` | **Fits** | The first family: this is doc 022 |
| 16 | **Partitioned convolver** (`convolver.rs`; `ConvolverNode`) | `Convolver`, `IrSpectra` behind an `Arc`, non-uniform levels from a cost model. Typed: `Mix`, `Amplitude` (`convolution/params.rs`) | **Fits** | Design: `(IR, block size B, quality)` to a model that carries what analysis needs: `Response` is the IR's FFT magnitude, `Ringing` the IR length, `Latent` = `B` (the file header: output at frame `n` is the convolution at `n - B`), `Bounds` the IR length and level plan. The IR is data, so the control path is a **published** model, not param cells (`ArcLend`, section 5.3). Doc 022's `FirSummary` is this model's reverb face |

### 3.2 Tutti-dsp: nonlinear, rate and format, infrastructure, analysis

| # | Algorithm (where) | Kernel today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|
| 17 | **Waveshaper** (`waveshape.rs` `ShapeKind`, `Shaper`; `DistortionNode`) | `Shaper::shape`, `shape_block*`; memoryless. Typed: `Drive` | **Fits** | Design: `(ShapeKind, drive)` to `Shaper` (already a value). `StaticCurve` with `In = Out = f32`. Exact queries from the curve: DC offset `eval(0)`, odd symmetry (`eval(-x) = -eval(x)`, hence no even harmonics for a sine), the harmonic amplitudes of a sine of amplitude `A` (the DFT of `eval(A sin t)` over one cycle, computed on the control side), and `Fidelity` as the aliasing bound (the highest harmonic above the floor against Nyquist; `Crush` is a quantiser and has none that decays). `Latent` and `Ringing` zero |
| 18 | **FFT resampler** (`resample/fft.rs`, `streaming.rs`; `tutti-export` `process/resample.rs`) | `FftResampler::process_into_buffer`, `StreamingResampler`, `ChunkSize`. Plain `SampleRate` pair | **Partly** | Design, model and queries fit: `(source, target, chunk size)` to the ratio (exact, from `gcd`), `Latent` (`output_delay()` frames, a figure the file header already computes), `Response` of the windowed-sinc low-pass (passband ripple, stopband attenuation, cutoff), `Fidelity`. The kernel shape does not: input and output counts differ (`input_frames_next`, `output_frames_max`), so it is not an `Effect<M>`; it gets its own shape trait and no `DesignNode` |
| 19 | **Dither** (`dither.rs`; `tutti-export` `process/dither.rs`) | `DitherNoise::apply` in place, xorshift32. Typed: `Amplitude` (the LSB) | **Partly** | The model is the noise's density: `Rectangular` has variance `lsb^2/12`, `Triangular` `lsb^2/6`, and the error power of a quantiser with it; these are the exact queries. It adds noise to silence, so `Ringing` is `Unbounded` and it never parks. In place, not planar in/out. One query (`noise power`) is not in the vocabulary; it is an inherent method, not a trait |
| 20 | `control.rs` (`Ramp`, `segments`, `COEFF_INTERVAL`, `lane_group`) | helpers | **No** | The cadence machinery the node uses (section 4.5), not an algorithm |
| 21 | `buffer.rs` (`CircularBuffer`, `MonotonicMinDeque`) | containers | **No** | Containers below the shape |
| 22 | `fft/` (`RealFft`, `StagedRealFft`, `FftScratch`) | transforms | **No** | A primitive below the shape; the contract uses `RealFft` to measure responses |
| 23 | `analysis/` (spectrum, loudness, pitch, tempo, onset, chroma, chord, key, notes, peaks, silence, stereo, vectorscope, align, beat, window, xcorr, yin, ...) | streaming config/state/step | **No** | Measurements, not processors. They have a design (a window length fixes the frequency resolution `fs/N` and the latency) but no sound to realise; the pattern that fits them is a config, an analyser, and a `Readout`-like result, which this design does not cover |
| 24 | `tutti-export` `pump_blocks` pipeline | driver | **No** | A pipeline that uses rows 18 and 19, not an algorithm |

### 3.3 Tutti-nodes: the rest

| # | Node (where) | Kernel today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|
| 25 | **LFO and modulators** (`lfo.rs`, `modulator.rs`; `ModulatorNode<Wave>`) | `Wave` (in `tutti-modulation`) is already a pure `cycles -> value` function. Typed: `Hz`, `Depth`, `PhaseIncrement` | **Partly** | The model is the kernel: there is no second thing to hold it to. The exact queries (period, range, Fourier series of a saw, triangle, square) are inherent methods on `Wave`. `Wave` is in `tutti-modulation`, which depends on `tutti-types` alone, so it cannot implement `tutti-dsp`'s traits (layering, section 9) |
| 26 | **Note envelope** (`signal/note_envelope.rs`; `NoteEnvelopeNode`), `Adsr` in `tutti-types/src/value/adsr.rs` | segments sent to control lanes | **Fits** | A generator. Design: `(Adsr, curve, rate)` to the segment list; queries: segment durations, total time to a level, `Ringing` = release time to the floor |
| 27 | **Click** (`click.rs`; `ClickNode`) | transport-driven source reading `Env` per frame | **No** | A function of the transport, not of controls; its sound is a table. Nothing to analyse that is not the table |
| 28 | `ramp.rs` (`LastGood`, `block_driven`, `clamp_finite`) | helpers | **No** | The control reads every node shares; the generic node uses them (section 5) |
| 29 | **Automation and control lanes** (`automation/lane.rs`, `control_lane.rs`, `recording.rs`) | a curve evaluated at the beat | **No** | The model is user data (a curve) and already a pure function of position |
| 30 | **Channel sum** (`mix_bus.rs`; `ChannelSumNode`) | summing | **No** | Plain summing: no controls, no design |
| 31 | **Bus strip** (`strip.rs`; `BusStripNode`) | gain, balance, mute: `balance_gains` | **Fits** | Design: `(Amplitude, Pan, mute)` to a 2x2 gain matrix. Queries: the matrix, unity at centre, flat `Response`. Small, and the same kind of model as `Fold` (row 32): a gain matrix |
| 32 | **Downmix and `Fold`** (`downmix_unit.rs`; `tutti-types/src/fold.rs`) | `Fold` is built on the control side and applied without allocating | **Fits** | Already the split: `Fold` is the model (a gain matrix), `Fold::frame` the kernel. Queries: the matrix, power preservation, `FoldError`. `Fold` is in `tutti-types` and cannot implement `tutti-dsp` traits, so a wrapper in `tutti-nodes` would |
| 33 | `reconcile.rs` | graph editing | **No** | Modulation reconciliation (doc 014), not an algorithm |

### 3.4 Instruments and the other crates

| # | Algorithm (where) | Kernel today and controls | Verdict | Design, model, queries, or why not |
|---|---|---|---|---|
| 34 | **Polysynth oscillators** (`tutti-polysynth/src/kernel.rs`: `sine`, `saw` and `pulse` with PolyBLEP/PolyBLAMP, `triangle`, `white`, `pink`) | `f32x8` lanes, no state, called from `bank.rs` | **Fits** | A generator. Design: `(OscillatorType, frequency, pulse width, rate)` to a model; queries: harmonic amplitudes (saw `1/n`, square odd `1/n`), the aliasing bound after PolyBLEP (`Fidelity`). The SIMD kernel is exactly the thing that must not be reshaped (section 2, constraint 6) |
| 35 | **Polysynth envelope** (`bank.rs`, `Adsr`) | straight-line stages converted to per-sample slopes | **Fits** | The ADSR model of row 26, with `Ringing` = release |
| 36 | **Unison and portamento** (`unison.rs`, `portamento.rs`) | `UnisonEngine`, `Portamento` | **Fits** | Small pure designs: unison to a table of detune, pan and gain per voice (`UnisonVoiceParams`); portamento to a glide curve and time |
| 37 | **Voice allocation** (`voice.rs`) | stateful allocator | **No** | Already `tutti-voicing`'s `Voicing::step`, a pure step over state; a different pattern with its own contract |
| 38 | **SoundFont envelopes** (`tutti-soundfont/src/volume_envelope.rs`, `modulation_envelope.rs`) | dB-linear stages from SF2 timecents | **Fits** | Segment durations are exact (the file's tests assert a stage falls 100 dB in its time); `Ringing` = release to the floor |
| 39 | **SoundFont LFO** (`lfo.rs`) | delay then a triangle at a frequency | **Partly** | Period and delay are exact; row 25's remarks apply |
| 40 | **SoundFont low-pass biquad** (`bi_quad_filter.rs`) | `set_low_pass_filter(cutoff, resonance)` then `process` | **Fits** | A biquad: the same `Response`, `Stable`, `Ringing` as row 1, with its model a coefficient set. `tutti-soundfont` does not depend on `tutti-dsp` yet; it would |
| 41 | **SoundFont voice render** (`render.rs`, `voice.rs`, `region_ex.rs`; the generator sum of SF2 9.4) | sample playback with interpolation, oscillator | **Partly** | The design half is pure and fits: SF2 generators (cents, timecents, centibels) to voice parameters through the 9.3 conversions. The sample-playback oscillator is a resampling read whose model is a pitch ratio and a loop; no response to speak of |
| 42 | **Stretch unit** (`tutti-sampler/src/stretch/unit.rs`; phase vocoder) | `Unit::tick`/`process`, source-pulled; latency depends on bypass | **Partly** | Fits: `(stretch factor, cents, FFT size, enabled)` to a model whose `Latent` is `latency_samples()` (a function of the FFT size and whether it is processing, per the method's docs), the duration ratio, `Graded` (FFT size is the quality). Does not fit: nonlinear, so no `Response`; the kernel consumes at a variable rate (`input_rate`), so not an `Effect<M>` |
| 43 | **Sampler butler and voices** (`tutti-sampler/src/butler/`, `voice/`) | streaming I/O | **No** | I/O and scheduling, not an algorithm |
| 44 | **VBAP** (`tutti-spatial/src/vbap/`) | `VbapPanner::solve_block` with angle smoothing | **Fits** | Design: `(azimuth, elevation, spread, width, speaker layout)` to the gain matrix. Queries: the matrix; power normalisation (`sum g^2 = 1`); at most a few non-zero speakers; continuity in azimuth. The smoothing is the kernel's |
| 45 | **HRTF binaural** (`tutti-spatial/src/hrtf/`) | `HrtfBinaural`, HRIR convolution with a de-zipper | **Fits** | Design: `(azimuth, elevation, rate)` to an interpolated HRIR pair (from the sphere: data). `Response` is the HRIR's FFT, `Ringing` is `ir_len - 1` (`ring_out`), `Latent` the delay compensation. Published like row 16 |

Whether the instruments' controls are typed units was not audited row by row.
What the code shows: `Adsr` is `Seconds` and `Amplitude`
(`tutti-types/src/value/adsr.rs`); the VBAP panner takes `Azimuth`, `Elevation`,
`Spread` and `StereoWidth`, and the `vbap` crate it calls takes bare `f64` degrees
(the code's own comment: "The types stop at the trigonometry"); the polysynth's
`SynthConfig` is a plain config struct whose fields a step-6 PR would type or
justify (CLAUDE.md "Unit types").

### 3.5 Counts

| Verdict | Rows | Count |
|---|---|---|
| Fits | 1, 3, 5, 6, 7, 8, 11, 15, 16, 17, 26, 31, 32, 34, 35, 36, 38, 40, 44, 45 | **20** |
| Partly | 2, 9, 10, 12, 13, 18, 19, 25, 39, 41, 42 | **11** |
| No | 4, 14, 20, 21, 22, 23, 24, 27, 28, 29, 30, 33, 37, 43 | **14** |

The 14 "no" rows are of three kinds, and the kind says what to do with them. Eight
are **infrastructure or primitives** (4, 14, 20, 21, 22, 24, 28, 33): they sit
below or beside the shape and need nothing. Four are **not processors of
controls** (23 analysis, 27 click, 29 lanes, 30 sum): their sound is data or
a measurement. Two are **a different pattern that exists already** (37, a pure
step over state, in `tutti-voicing`; 43, I/O). None is "no" because the shape
bends to it.

### 3.6 Which capability each family answers

`L` = `Latent` and `R` = `Ringing` (every model answers both). A dash is "not
claimed", and the reason is in section 3.1 to 3.4.

| Family (row) | Stable | Response | StaticCurve | Readout | Fidelity | Graded | Family-only queries |
|---|---|---|---|---|---|---|---|
| Reverb (15) | yes | none | none | none | none | no | `rt60`, bands, densities, freeze, `min_delay` |
| SVF, biquad (1, 40) | yes | yes | none | none | none | no | poles, Q |
| Ladder (2) | yes | small signal | none | none | none | no | self-oscillation |
| Crossover (3) | yes | multi-output | none | none | none | no | sum is all-pass |
| Compressor (5) | yes | none | yes | gain reduction | none | no | step response |
| Gate (6) | yes | none | step | open/level | none | no | hold, attack, release |
| Limiter (7) | yes | none | yes | gain reduction | none | no | lookahead |
| Brickwall, waveshaper (8, 17) | yes | none | yes | none | aliasing | would (oversampling) | DC, harmonics |
| Delay (11) | yes | comb | none | none | interpolation droop | no | max delay |
| Chorus, flanger, phaser (12, 13) | yes | frozen | none | none | none | no | sidebands, notches |
| Convolver (16) | yes | IR spectrum | none | none | none | yes (partition) | IR length |
| Resampler (18) | yes | low-pass | none | none | stopband | yes (kernel length) | ratio |
| Dither (19) | yes | none | none | none | noise floor | no | noise power |
| Stretch (42) | no | none | none | none | none | yes (FFT size) | duration ratio |
| VBAP, fold, strip (31, 32, 44) | yes | flat | none | none | none | no | gain matrix |
| HRTF (45) | yes | HRIR spectrum | none | none | none | no | ITD |
| Envelope, oscillator (26, 34, 35, 38) | n/a | none | none | none | aliasing (osc) | no | segments, spectrum |

## 4. The traits

All in `tutti-dsp`, in a new neutral module (section 9). Sketches are Rust against
the real code: `Setup`, `Effort`, `Channels` and `Ring` are on #367 and `main`
and move unchanged.

### 4.1 `Design`, `Model`, `Bounds`

```rust
/// A description of what one setting of an algorithm does, readable without
/// running it.
pub trait Model: Latent + Ringing + Sized {
    type Bounds: Bounds;
    /// A model for `rate` with nothing in it: what `design_into` overwrites.
    fn empty(rate: SampleRate) -> Self;                   // Network::new(rate)
    /// This model's size, or its tail measured to `floor`.
    fn bounds(&self, floor: Db) -> Self::Bounds;          // Bounds::of
}
pub trait Bounds: Copy + PartialEq {
    /// Whether this bound holds `other` in every figure.
    fn dominates(&self, other: &Self) -> bool;            // today's Bounds::dominates
}

/// The pure function from a family's controls to its model.
pub trait Design: Clone + Send {
    type Controls: Copy + PartialEq + Send;               // as today
    type Model: Model;                                    // new
    type Range: Copy;                                     // new: Ceiling's family part
    type Kernel: Kernel<Self::Model>;                     // generalised
    const CADENCE: Cadence = Cadence::Block;              // new, section 4.5

    fn channels(&self) -> Channels;
    fn design_into(&self, c: &Self::Controls, rate: SampleRate, out: &mut Self::Model);
    fn design(&self, c: &Self::Controls, rate: SampleRate) -> Self::Model { /* empty + design_into */ }
    fn bounds(&self, ceiling: &Ceiling<Self::Range>) -> <Self::Model as Model>::Bounds;
    fn kernel(&self) -> Self::Kernel;
}
```

Why `design_into` stays the required method: a `Network` is 56 KB and cannot be
built by value on an audio thread (doc 022 "Step 1a as built"). The other models
are a few dozen bytes, where in-place is merely harmless; one signature serves
both. Why `Model::empty`: the provided `design` needs to build a value for the
control side, a test and a UI, and `Network::new(rate)` is exactly that.

**`Bounds` and `Ceiling`, generalised.** Today's `Bounds` is a struct of the
reverb's sizes; here it is the model's associated type (`Network` keeps its
struct as it is). `Ceiling` is the node's declared limits: the rate, the
level a tail is measured to, and the family's own range of a control.

```rust
pub struct Ceiling<R> { rate: SampleRate, floor: Db, range: R }   // validated as today
// Reverb: R = MaxDecay(Seconds), capped at 60 s as Ceiling::MAX_DECAY_CAP is.
// SVF, compressor, VBAP: R = ().
// Delay: R = MaxDelay(Seconds). Limiter: R = MaxLookahead(Seconds) if lookahead is a range.
```

`Ceiling::new` keeps the rate and floor checks and takes `R`'s own validation from
`R`'s constructor, so `Ceiling<()>` has the checks and nothing else. A bound that
a design cannot make dominate its own designs fails `bounds_dominate` in the
contract, as it does for the reverb today.

### 4.2 `Kernel<M>` and the block shape

```rust
/// The runtime that realises a model.
pub trait Kernel<M: Model>: Send + Sized {
    fn prepare(&mut self, bounds: &M::Bounds, setup: &Setup);   // control side; allocates
    fn retarget(&mut self, model: &M);                          // audio thread; no allocation
    fn reset(&mut self);
    fn latency(&self) -> Samples { Samples::ZERO }
    fn effort(&self) -> Effort { Effort::Heavy }
    #[must_use] fn fresh(&self) -> Self;
}

/// The planar block shape a graph node drives.
pub trait Effect<M: Model>: Kernel<M> {
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]);
}
```

Why the kernel is generic over the model and not an associated type of the
design: **one kernel, several designs.** The SVF design and an RBJ-cookbook
design both produce a coefficient model; the SoundFont's biquad (row 40) and the
SVF's `Biquad`-shaped queries can share a kernel type; a second reverb design with
the same stages shares Freeverb's kernel. The kernel is tied to the model it
realises, not to who produced it. A kernel for two models is two impls.

Why `process` is not in `Kernel<M>`: constraint 4. `Effect<M>` is the shape a
`DesignNode` drives. A resampler, a stretcher and an oscillator bank implement
`Kernel<M>` for the lifecycle and the contract, and a different shape trait for
their block. The contract's measuring helpers render through a small harness
trait (`Render`, **(proposal)**) that each shape implements, so the same
measurement (an impulse, a tone burst, a noise run) works for any of them.

Everything else is the reverb's `Kernel` unchanged: `prepare` sizes from bounds
and never inspects a model, `retarget` allocates nothing, `process` overwrites
`output` with the processed signal (for an effect with a dry path, the **wet**
signal; mixing is the node's), and `fresh` is the fork rule of doc 022.

### 4.3 Capability traits

One small trait per query. The reasons not to use one fat `Queries` trait:

- A fat trait needs a default for every query a family lacks (`None`, `Unknown`,
  `NaN`), so a contract cannot tell "no answer" from "wrong answer", and a UI
  cannot tell a flat response from an absent one. With separate traits the
  question "does this model have a response" is a bound, answered at compile time.
- Every family would pay for the union in its docs and in its impl.
- Bounds compose: `where D::Model: Response + Stable`. A contract case that needs
  a capability states it in its signature, and the scratch crate confirms that
  `response_case(&CompDesign, ..)` fails to compile with "`CompModel: Response`
  is not satisfied".

The cost: one `impl` line per capability, and no run-time discovery of "what can
this model do", which a UI that holds a `dyn` would want. That is answered where
the type is still known: the node builds its `Describe` surface at construction
(section 7). The names `Latent` and `Ringing` avoid `tutti_types::Latency` and
`tutti_types::Tail`, which are the graph's wrappers of the same facts.

| Trait | One-sentence boundary | Signature (sketch) |
|---|---|---|
| `Latent` | Processing latency, in frames, that a model declares | `fn latency(&self) -> Samples` (`Network::latency` today). Never a pre-delay or a delay time |
| `Ringing` | How long a model's output lasts after its input ends | `fn tail(&self, floor: Db) -> Ring` (`Network::tail` today); `Finite(0)` for a memoryless model |
| `Stable` | Whether a model's recursion is bounded | `fn is_stable(&self) -> bool` (`Network::is_stable` today). A sufficient test, as the reverb's |
| `Response` | The complex gain from an input to an output of a linear model at given frequencies | `fn outputs(&self) -> usize { 1 }`; `fn response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64])`; provided `magnitude`, `phase`, `group_delay`. Linear and time-invariant at the frozen setting; a model with a nonlinearity says "small signal" in its docs |
| `StaticCurve` | A memoryless map from a level to a level that a model settles to | `type In; type Out; fn eval(&self, x: Self::In) -> Self::Out`. Compressor: `Db -> Db` (reduction); waveshaper: `f32 -> f32` |
| `Fidelity` | The worst-case departure of a model from its ideal | `fn error_floor(&self) -> Db`: aliasing, stopband, interpolation droop, quantisation. Lower is better |

Two are on the **kernel**, not the model, because they are about what runs:

```rust
/// A value a running kernel reports about itself, the deepest since last taken.
pub trait Readout { type Value: Copy; fn take(&mut self) -> Self::Value; }
```

`Readout` is state (gain reduction is not a function of the controls), so it
cannot be a model query. `Graded` is on the design:

```rust
/// A design with a quality tier (doc 023 G5); section 4.7.
pub trait Graded: Design { fn quality(&self) -> Quality; #[must_use] fn with_quality(self, q: Quality) -> Self; }
```

Queries a single family has stay inherent methods of its model (`Network::rt60`,
`echo_density`, `is_lossless`; a shaper's `harmonics`). A query graduates to a
trait when a second family answers it and a contract case is written once for both.
The test: two implementers and one case.

### 4.4 What a model must answer, and what it may

`Latent` and `Ringing` are supertraits of `Model`, because every node declares
both in its `Shape` (`Shape::with_latency`, `with_tail`) and the park/cut logic of
the node needs only these two. A memoryless model answers `Finite(0)`; the
compressor and limiter answer zero too (the nodes declare `Tail::None`: a tail
excludes latency, and the gain follower multiplies the input, so silence in is
silence out). Dither's
`Unbounded` is the honest answer for a kernel that makes sound from silence.
Everything else is opt-in.

### 4.5 Cadence: how often the design is solved

```rust
pub enum Cadence {
    /// One design per block whose controls changed: reverb, delay, dynamics (today).
    Block,
    /// A design at the end of every segment of at most `n` frames: filters (`COEFF_INTERVAL`).
    Every(NonZeroUsize),
}
```

`DesignNode` splits a block with `tutti_dsp::segments(size, unprimed)` (it exists
and is what `SvfFilterNode` calls) and, per segment, reads the controls at the
segment's last frame, designs, calls `retarget` and `process` on the segment's
slices. For `Block` that is one segment and the reverb is unchanged. For `Every(16)`
it reproduces what the SVF does: the coefficients are solved at the solve points
and the kernel `lerp`s from the previous solve to the new one across the segment
(`SvfCoefficients::lerp`), arriving exactly on the segment's last frame, as `Ramp`
does (`control.rs`). The kernel owns the interpolation; the node owns where the
segments are. The first block (`unprimed`) solves at its first sample, as the SVF
does.

**Where cadence stops.** The compressor, gate and limiter nodes take a threshold
(and the limiter a ceiling) as a **per-frame lane** (`io.param(0).frames()`,
`compressor.rs` line 469, `limiter.rs` line 365). `Every(1)` per-frame would pay
the node's per-segment overhead on every frame, which is exactly the fixed shape
the owner warned against. So per-frame lanes are not a `Cadence`; they are a
second, optional kernel shape (**(proposal)** `Driven<M>`: `process_driven(input,
output, lane)`, chosen by the node on a block whose port is `Frames`), or the node
keeps its own `process` and adopts the model and the contract only. Section 11 Q3
recommends deciding that against the code in step 3, and states the fallback,
which loses nothing: the dynamics get their curves, readouts and contract either way.

### 4.6 The control side of a node

`ReverbParams` (on #367) is the `Param` cells of an algorithm and how a block
reads them. It is generic already and gets a neutral name (`ControlParams`) and two
changes: `read` takes the frame to read at (the segment's last frame, for
`Every(n)`; the block's last frame, as today, for `Block`), and the `longest` hook
becomes declarative where it can (section 8.2). `tutti-dsp` still holds no `Param`.

### 4.7 Quality (doc 023 G5) as a design input

AU's `RenderQuality` is graded (0 to 127); VST2's `noRealTime` and the others carry
`RenderMode` only (doc 023 rows A18, S20, C38). Native algorithms have real
quality choices: the convolver's partition (a latency/cost trade), the export
resampler's kernel length and chunk size (`ChunkSize::PRESETS`), the stretcher's
FFT size, a waveshaper's oversampling, a reverb's line count.

```rust
// tutti-types, beside RenderMode (doc 023 step 10). (proposal)
#[non_exhaustive] #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Quality { Draft, Standard, High, Best }
impl Quality { pub fn from_au(render_quality: u32) -> Self { /* 4 bands of 0..=127 */ } }
```

It enters **as part of the design value (`&self`), not `Controls`**, because it
changes `Bounds` (what is allocated) and often `Latent`. A control is retargeted
per block; a quality change is a rebuild through the graph's `crossfade`, as
doc 022 section 5.1 already says of any structural change. `Graded` is the
capability a design implements to say so. The model records the consequence in its
queries, so nothing about quality is invisible: latency changes, the tail changes,
`Fidelity::error_floor` improves.

The contract's `quality` case (section 6) runs over `tiers()` in order and asserts
the model's `Fidelity` is non-increasing in dB, `Kernel::effort` is non-decreasing,
and `Bounds` of a lower tier is dominated by a higher one. Doc 023 says G5 is read
only from AU on the plugin side; a native node reads it as a field.

### 4.8 Three families against the traits

The test the owner's caution sets: the sketch must not bend to one family. Each of
these three was written out and compiled in the scratch crate.

**Filter (SVF).** The model is a coefficient set; no state; the kernel is today's
`svf_step`.

```rust
#[derive(Clone, Copy)] pub struct SvfModel { /* SvfCoeffs + rate + type */ }
impl Latent  for SvfModel { /* 0 */ }
impl Ringing for SvfModel { /* pole radius r = |z|: tail = ln(10^(floor/20)) / ln r frames */ }
impl Stable  for SvfModel { /* k > 0 */ }
impl Response for SvfModel { /* H(z) from g, k, m0..m2 */ }
impl Design for SvfDesign {
    type Controls = SvfControls;          // { ty: SvfType, freq: Hz, q: Q, gain: Db }
    type Model = SvfModel; type Range = (); type Kernel = SvfKernel;
    const CADENCE: Cadence = Cadence::Every(NonZeroUsize::new(COEFF_INTERVAL).unwrap());
    /* design_into = SvfCoeffs::new + rate; bounds = a trivial () */
}
impl Kernel<SvfModel> for SvfKernel { /* retarget stores `next`; process lerps across the segment */ }
impl Effect<SvfModel> for SvfKernel { /* svf_step over lane groups (control.rs LANES) */ }
```

**Dynamics (compressor).** No `Response`; a `StaticCurve`; a readout on the kernel.

```rust
impl StaticCurve for CompressorModel { type In = Db; type Out = Db;
    fn eval(&self, x: Db) -> Db { compute_compressor_gain_reduction(x, self.threshold, self.ratio, self.knee) } }
impl Latent for CompressorModel { /* 0 */ }  impl Ringing for CompressorModel { /* Finite(0) */ }
impl Readout for CompressorKernel { type Value = Db; fn take(&mut self) -> Db { /* deepest since last take */ } }
```

**Reverb.** No `Response`; the family's own queries stay inherent.

```rust
impl Model for Network { type Bounds = reverb::Bounds; /* empty = Network::new, bounds = Bounds::of */ }
impl Latent for Network {} impl Ringing for Network {} impl Stable for Network {}
impl Design for FreeverbDesign { type Model = Network; type Range = MaxDecay; type Kernel = Freeverb; /* Cadence::Block */ }
```

None of the three needed another's field, and none carries a method it does not
answer. The reverb's `Bounds` struct, the filter's `()` and the compressor's `()`
are three different `Bounds`. The filter is the only `Every(n)`; the compressor
alone has a `Readout`.

## 5. The generic node

`ReverbNode<D>` becomes **`DesignNode<D>`** in `tutti-nodes`. It is the node of
doc 022 section 5 and "Step 1b as built" with the reverb removed from its names.

```rust
pub trait NodeDesign: Design + 'static
where Self::Kernel: Effect<Self::Model> {
    type Params: ControlParams<Controls = Self::Controls>;   // was ReverbDesign::Params
    type Blend: Blend;                                       // NoBlend | DryWet
    type Readouts: Readouts<Self::Kernel>;                   // () | a kernel's readout, published
}
pub struct DesignNode<D: NodeDesign> { /* design, kernel, params, mode, blend, readouts,
    model: Box<D::Model>, longest: Box<D::Model>, applied, current, ceiling, rate,
    channels, ring, since, silent */ }
```

### 5.1 What it carries, and what is optional

| Part | Source today (on #367) | Per family |
|---|---|---|
| The control path: cells, modulated ports read per block, audio-thread redesign in place when the controls changed | `ReverbNode::process`, `ReverbParams::read`, `apply` | **Always.** Doc 022's finding holds for every family: a lane writes the node's *port*, never its cell, so only the audio thread sees a modulated value |
| `Fixed` vs `Live` mode (a send with no cells declares the exact tail of its one model) | `Mode::{Fixed, Live}`, `fixed()` | Always; a family with no `Fixed` use never calls `fixed` |
| The tail report and the `ring` high-water rule | `io.report_tail(tail_of(self.ring))` | **Always**, from `Model: Ringing` (a mandatory supertrait) |
| `longest()` for the tail over a block | `ReverbParams::longest` | Only for a family whose model has a controllable tail: `Params::longest` returns `None` by default; section 8.2 makes it a declared direction |
| Parking: scan for the last non-zero input frame, owe `ring + latency` frames, cut to the frame, `reset`, `Status::Idle` | the body of `process` | **Always**, and uniformly from `Ringing`: a memoryless model owes zero frames; dither's `Unbounded` never parks; a kernel that lies about its tail is caught by `assert_honest_tail` (doc 013 step 0b) and the node contract |
| Dry/wet | `mix: Param<Mix>`, `DryAlign`, the blend loop | **Optional through `D::Blend`**: a reverb, delay or chorus has a wet; a filter, a compressor or a panner has none |
| A readout | not on #367 | **Optional through `D::Readouts`** |
| Fork | `fork_fresh`: design clone, `Kernel::fresh`, cells detached, `ParamNode` | Always; export forks with preroll (doc 013) |
| Shape | `Shape::audio(..).with_cost(effort).with_latency(kernel.latency()).with_tail(ceiling).with_param_addrs(..)` | Always; `Cost` is a `match` over `Effort` |

### 5.2 Optional parts are associated types, not flags

```rust
pub trait Blend: Send + 'static {
    /// The addresses it adds to the node's ports, ahead of the algorithm's.
    const ADDRS: &'static [UnitAddr];
    fn prepare(&mut self, latency: Samples, width: usize, max_block: MaxBlock);   // DryAlign rings
    fn finish(&mut self, io: &mut Io<'_>, cut: usize, size: usize);               // the blend loop
}
pub struct NoBlend;   // ADDRS = [], every method empty: the node's ports start at 0
pub struct DryWet { /* mix: Param<Mix>, good_mix: LastGood, dry: Vec<DryAlign>, dry_buf, last_mix */ }
```

`NoBlend::finish` is an empty function the optimiser removes; a filter pays no
branch and no `Param<Mix>`. `Readouts<K>` is the same with `fn publish(&mut self,
kernel: &mut K)` called once at the end of `process`; `()` implements it for any
`K`, and a family with a readout names a type that implements it for its kernel
(section 7). Nothing is a runtime flag in the audio path: the choice is a type
parameter fixed when the node is built.

One consequence for ports: `ReverbNode` puts `Wet` at port 0 and the algorithm's
addresses after it (`first = 1`). With `NoBlend` the algorithm's start at 0, so
`first` is `D::Blend::ADDRS.len()`. A test pins the port order of the reverb
unchanged (it is persisted in saved graphs by `UnitAddr`).

### 5.3 A published model

The IR families (rows 16 and 45) cannot design on the audio thread: the IR is data.
Doc 022 step 2 puts the convolver's IR behind `ArcLend`. `DesignNode` does this
with a **second control path**, not a flag: `ControlParams` has an associated
`Published` mode where `Controls` is a handle (`Arc<Ir>`) read through a `Reader`
once per block, and `design_into` is replaced by a control-side build that
`ArcLend::publish`es a model the kernel `retarget`s from a `Loan` (lend once per
block, never park a `Loan`; CLAUDE.md "Publishing to the audio thread"). The cells
and the lane-driven ports of the other families do not exist for it. This is the
largest piece of the convolution step (section 10) and is not needed by steps 1 to 4.

### 5.4 Descriptions and the Bevy side

`param_graph_node!(DesignNode<D>)` is one line per `D`, as for the reverb today;
`bevy-tutti` composes nothing new (`scripts/check-adapter.sh` holds). The old node
names stay as type aliases where they are public API: `FreeverbNode =
DesignNode<FreeverbDesign>` already is one on #367, and `SvfFilterNode<F>`,
`CompressorNode` and the others would become aliases of `DesignNode` over their
designs (Q9 covers the inherent methods those nodes expose today, such as
`SvfFilterNode::frequency()` returning an `Arc<AtomicF32>`).

## 6. The contract machinery

Parameterised by capability, in `tutti-dsp::contract` (section 9). Doc 022's
`design_contract!` and `kernel_contract!` split into a **core** that every family
runs and **capability groups** a family opts into. Every figure is analytic or
from the spec, never another implementation's render (CLAUDE.md, "Do not compare
a render against another reference render in a new test").

### 6.1 Core cases (every family)

These are the generic cases already on #367, with the reverb's figure replaced
where it was one:

| Case | Figure | Mutant |
|---|---|---|
| Pure | same controls give the same model; non-finite controls leave the last good ones; `design_into` under `assert_no_alloc` | read a clock |
| Bounds dominate | `bounds()` holds `design()` for every reachable control | undersize by one |
| Latency | `Kernel::latency()` equals `Model::latency()` equals the measured shift of the first output sample | return 0 |
| Reset, fresh | bit-identical to a freshly built kernel at the same controls | `reset` does nothing; `fresh` shares state |
| Block independence | blocks of 1, 17, 64, 512 bit-identical (time-domain kernels; a convolver within a declared round-off) | a block-dependent ramp |
| Click-free retarget | a step of the controls mid-signal: the largest jump within a small factor of the steady-state bound | apply a coefficient unslewed |
| No allocation | `retarget` and `process` under `assert_no_alloc` with changing models | n/a (it is the gate) |

The no-allocation case needs a counting allocator, which needs `unsafe` that a
`forbid(unsafe_code)` library cannot hold; today it is `tests/network_alloc.rs`.
The machinery ships it as a macro (`no_alloc_contract!`) that a family expands in
its own `tests/` file, where the crate may define the allocator.

### 6.2 Capability groups

Each group is a set of functions bounded on its capability, plus a case trait that
supplies what only the family knows (probes and tolerances). The reverb's present
`DesignCase`/`KernelCase` split by capability.

| Capability | Design case (no kernel) | Kernel-realises-model case | Mutant pattern |
|---|---|---|---|
| `Ringing` | `tail` is non-decreasing along the family's slower-pair list (`decay_pairs`, `q_pairs`); `Unbounded` exactly when the family says lossless | measured time from input end to each of -40, -60, -90 dB (the loudest windowed RMS the render reached is the reference, as 022's `tail_covers`) is **within** `tail(floor)`, and `tail` is **not more than a declared slack above it** (tightness: a lazily huge tail fails) | halve `tail` (covers fails); double it (tightness fails); scale a loop gain by 1.03 |
| `Stable` | `is_stable` for the whole reachable grid; refuses hostile controls (design clamps) | 60 s of noise at the extreme controls is finite and under the family's peak bound | a loop gain above 1; a pole outside the unit circle |
| `Response` | the model's magnitude at the family's **anchors** equals the spec: LP at `fc` is -3.01 dB at Butterworth Q, a bell's centre is its gain exactly, a shelf is half its gain at `fc`, LR4 is -6.02 dB at `fc` and the bands sum flat | the FFT (`RealFft`) of the kernel's impulse response against `Response::magnitude` over a log grid, within the family's dB tolerance, and the phase where declared | detune the cutoff 2% (the skirt moves 0.2 dB or more); swap Q; drop a zero |
| `StaticCurve` | monotone where the family says (compressor reduction is non-decreasing in level; slope at most `1 - 1/ratio`); zero below the knee; the limiter's output never above the ceiling; a waveshaper's symmetry and bound | a **level sweep**: a steady tone (or DC) at each level, the envelope settled (at least five release constants), the measured gain against `eval(level)` within the tolerance. A memoryless curve is exact to the ulp | threshold +1 dB; ratio off by 10%; ignore the knee |
| `Readout` | (kernel only) | after the sweep, `take()` equals the curve at the level, and also equals the gain measured on the output (two kernel facts that must agree); `take()` is zero when idle and clears what it took | flip the sign; report the gain before makeup |
| `Fidelity` | `error_floor` is non-increasing across `Graded` tiers | a measured aliasing/ripple/quantisation floor (a tone near Nyquist through a shaper; a swept tone through a resampler) is not above `error_floor` | drop the anti-alias; shorten the kernel |
| `Graded` | `Bounds` of a lower tier dominated by a higher; `Effort` non-decreasing | the tier's `Latent` equals its measured latency | a tier that changes nothing |
| Core with family cases | the family's own (`rt60`, `band_order`, `line_lengths`, freeze for the reverb; `ring_from_poles` for a filter; `sum_flat` for the crossover; attack/release constants for dynamics) | same | each its own |

### 6.3 Opting in

```rust
// In the family's test module (or tutti-dsp tests for the families in tutti-dsp):
design_contract!(SvfDesign: [Ringing, Stable, Response]);
kernel_contract!(SvfDesign: [Ringing, Stable, Response], tolerance = ...);
```

1. The family implements its capabilities. A listed capability the model lacks is
   a compile error (the case is bounded on it).
2. The family implements `DesignCase` and one small probe trait per listed
   capability (`ResponseCase::anchors`, `StaticCurveCase::sweep`, ...).
3. **A capability implemented but not listed is not a compile error** (a macro
   cannot see every `impl`). `scripts/check-contract-coverage.sh` **(proposal)**
   greps every `impl <Capability> for` in the workspace and fails unless the
   implementing type appears in a `*_contract!` list, as `check-range-types.sh`
   does for its rule. The plugin contract's `gaps = N` ratchet is the precedent
   for a count that a reviewer must raise.
4. Each case has a `Defect`/`Flawed` mutant run by a self-test in `tutti-dsp`
   (`a_defective_kernel_fails_its_case` on #367) so that a case that cannot fail
   cannot ship.

### 6.4 The node contract

The cases that need a node and a graph (not just a kernel) are today
`tutti-nodes/tests/reverb_node.rs` and `reverb_node_generic.rs`. They generalise to
`node_contract!(D)` in `tutti-nodes`'s `testing` module (feature `testing`,
`Cargo.toml` line 29), over `tutti_graph::contract::Driven`: the declared tail
is honest after the cut (`assert_honest_tail`), park and wake equal a fresh node,
a port-driven block equals a cell-driven one, the dry path is aligned to the
kernel's latency and not its delay (`Blend` families only), a fork renders from
silence, and a serial/pooled render is bit-identical (`tests/parallel.rs`). The
"lying design" fixture (an echo that rings past its claim, with a latency) becomes a
shared test double.

The measuring helpers (Schroeder integration, band energy, tone bursts, first
non-zero sample, level sweep, impulse-response magnitude by `RealFft`) go in
`tutti_dsp::contract`, public under the feature, so the plugin probe of doc 023
section 6 can use the same ones on a hosted plugin (Q13).

## 7. Doc 023's gaps, and one read surface

### 7.1 G8 (curve response)

Doc 023 sketches `trait CurveResponse { fn magnitude(&self, freqs: &[Hz], out: &mut [Db]); }`
and says "a native node can compute the answer exactly". Here it is exact because
`Response` is a query of the model:

```rust
/// What a curve display asks. (proposal; doc 023 section 5.8 has the magnitude half)
pub enum Curve<'a> {
    Magnitude(&'a dyn CurveResponse),     // dB over Hz: an EQ, a crossover, a reverb's none
    Transfer(&'a dyn TransferCurve),      // dB over dB: a compressor, a gate, a limiter
}
```

A `DesignNode<D>` where `D::Model: Response` returns a `Curve::Magnitude` built
on the control side from the cells' base values (`ControlParams::snapshot`,
**(proposal)**, the cells without a port) by `design` then `Response::magnitude`.
Where `D::Model: StaticCurve` it returns `Curve::Transfer` the same way. It is
**available only where the capability exists**, by an inherent `impl` with a
`where D::Model: Response` bound (`DesignNode::curve()`); no run-time "does this
node have a curve" flag is stored. The design is cheap (0.2 us for the reverb; one
`tan` for an SVF), so a drag handle can call it per UI frame.

What a native curve can do that a plugin's cannot: ask a **hypothetical**. A
`CurveOf<D>::at(&controls)` designs at any controls, so a UI drags a handle and
draws the curve for the value under the cursor before it commits. It shows the
**base** value; a live curve at the modulated value needs the applied `Controls`
published back. They are `Copy` and `PartialEq`, so a sequence lock (`tutti-rt`'s
`seq_cell`) carries them, written once per changed block (Q10).

### 7.2 G9 (readout)

Doc 023: three formats declare a gain-reduction value (CLAP gain-adjustment, VST3
`Comp:GainReduction`, AU meter-read-only), and the dynamics nodes are the native
producer. The readout is a kernel fact. The node's `Readouts` associated type
publishes it; the cell is the one meters already use: `tutti_rt::level_cell`
(`LevelWriter::fold(peak, sum_sq, frames)`, `LevelReader::take`), whose
"a block lands whole in one take, peak and energy together" is what a gain
reduction needs (the peak is the deepest reduction since the last take, the energy
its mean). It has a loom model already, so no new lock-free structure
(CLAUDE.md "New lock-free ... goes in `tutti-rt`"), and a quiet cell is swept by
the same `Reclaimer` rule. The reader is one consumer (`&mut self`, as meters).
`CompressorKernel::gain_reduction_db` and `MultibandControls::gain_reduction_db`
are the two producers that exist today.

### 7.3 One surface for native and hosted

Doc 023 section 5.1 puts `Display { pages, readouts: Vec<Readout>, curve: bool }`
in the hosted plugin's `Capabilities`, and Q1 recommends an optional `Describe`
trait implemented by a node and read by `tutti-session` beside
`SessionNode::params`. The same trait is the surface for both:

```rust
/// What a node tells an application about itself. Not on the audio path. (proposal; doc 023 Q1)
pub trait Describe {
    fn display(&self) -> Display;                          // doc 023 section 5.1
    fn curve(&self) -> Option<Curve<'_>>;                  // control side
    fn readout(&self, kind: ReadoutKind) -> Option<ReadoutReader>;
}
```

| Question | Native `DesignNode<D>` | Hosted plugin (doc 023) |
|---|---|---|
| Curve | `Response`/`StaticCurve` of `design(controls)`: exact, at any controls | CLAP mini-curve-display, AU `FrequencyResponse`: a main-thread call through the `Controller` (016), only at the plugin's current parameters |
| Readout | the node's `level_cell` reader | the plugin's meter parameter or function name, polled and folded into the same cell shape |
| Latency and tail | `Is(model.latency())`, `Is(model.tail())`: analytic, and held by the contract | `Is(plugin claim)`, with `measured: Some(..)` from the probe (doc 023 section 6) |
| Quality | `Graded::quality()` | `Reported<QualityRange>` from AU, `Unasked` elsewhere |

An application reads `Describe` and never asks which it holds. The one asymmetry is
named in the type: `Processing::measured` is `None` for a native node because its
claims are contract-checked, not probed. The same measurement code runs in both
places (Q13): the probe measures a plugin's latency and tail as the kernel contract
measures a kernel's. Doc 023's `CurveResponse` gains the `Transfer` variant (a
one-line change to its section 5.8, made when step 3 lands).

## 8. Modulation: what the reverb found, applied generally

Doc 022 sections 10 and 11 and "what the reverb found" on #367. Each finding, for
every family.

### 8.1 Cost of a design per block, per family

Doc 022 measured the reverb in release: `design_into` 0.2 us, `Network::tail`
1.1 us (a libm `exp10` per band per line), `retarget` 5 ns; a lane that changes
every block costs about 1.3 us against 2.3 us to render 64 frames. **Nothing below
is measured except that row**; each step measures its own and records it here.

| Family | A design solves | Cadence | Smoothing policy | Tail query |
|---|---|---|---|---|
| Reverb | a few hundred float ops per line; `tail` dominates | `Block` | Freeverb steps; the FDN ramps lengths and `lerp`s absorption (022 sections 10.2, 10.3) | libm per band: skip in a block that cannot lengthen it |
| SVF, biquad, ladder | one `tan` (and a `powf` for gain) per solve (`svf.rs`) | `Every(16)` | coefficient `lerp`, arriving exactly at the segment end | pole radius: one `sqrt`, one `ln` |
| Crossover | a `tan` per split | `Block` today (`block_driven`) | re-solve at the block's target | as the filters |
| Compressor, gate, limiter | trivial arithmetic | `Block`; a per-frame threshold lane is a second shape (4.5) | the follower smooths; makeup ramps across the block (`CompressorBlock::makeup_from/to`) | zero |
| Delay, chorus | trivial | `Block` | a per-channel `Ramp` of the time, fractional reads, so a moving length is a pitch bend (022 section 10.3) | `time x trips` |
| Waveshaper | trivial | `Block` | `Drive` stepped; memoryless | zero |
| Convolver, HRTF | not a design: a published model | on publish | crossfade or a de-zipper (`HrtfBinaural`) | IR length |
| Resampler, stretch | structural: rebuild | none | n/a | `output_delay` |

### 8.2 Extremum hooks: `longest()` generalised

On #367 each `ReverbParams` writes `longest`: the controls that ring longest among
the values a modulated port takes anywhere in the block, because the port gives
every frame but the **direction** in which a param lengthens a tail is the
algorithm's knowledge. That is correct, and it is repeated once per family.
Many families' directions are monotone per parameter (a filter's tail grows with Q
and falls with cutoff; a delay's with feedback and time), so the direction can be
a table the node reads:

```rust
// in ControlParams (proposal)
const LENGTHENS: &'static [(UnitAddr, Dir)];     // Dir::{Up, Down}: more of this, longer tail
fn longest(&self, io: &Io<'_>, first: usize) -> Option<Self::Controls> { /* default: pick the
    frame extreme of each LENGTHENS port, the base for the rest */ }
```

and the design contract's tail case verifies the table: for each entry a
`tail_pairs` check that the model's `tail` is non-decreasing in the declared
direction (a wrong direction fails, which is the mutant). A family whose
parameters interact (the tail is not separable) overrides `longest`, as the reverb
does today. The node's `since`/`ring` logic is unchanged.

### 8.3 A family whose redesign must ramp

The filters. Two choices exist: design per sample, or design at solve points and
interpolate the coefficients. The SVF already measured it (`control.rs`,
`COEFF_INTERVAL`): per-sample solving is the dominant cost of a modulated SVF (a
`tan` per sample); solving every 16 frames and interpolating costs a quarter of a
64-frame block's `tan`s, with 3e-5 worst error on a 200 Hz to 8 kHz sweep in 85 ms
(4.6e-4 at 64 frames, exactly 0 at 1). So **interpolate**, at `Every(16)`. The
guarantee the contract holds is the one that matters for correctness: at a
segment's end the kernel's frozen response **is** the model's (a `Response` check
after the segment), so the interpolation can never leave the filter at a setting
the design did not ask for. Coefficient `lerp` does not by itself guarantee
stability between two stable sets; the existing SVF relies on the segment being
short and the error being the measured one, and the `Stable` case runs the filter
under a worst-case sweep so that this is a measured fact, not an assumption.

The other policies, for the record: a step (Freeverb: the output is read from the
past, so a step in the gain is a step in what the next trip carries, which the
click-free case holds to a bound), a slew at a bounded rate (the FDN's lengths),
a block ramp of a gain (the compressor's makeup, the strip's volume), and a
crossfade through the graph (any structural change).

### 8.4 The rest of 022's findings, carried over

- **The audio thread must be able to design.** Held: `design_into` is in place and
  total. A family whose model is data (rows 16, 45) is the published exception (5.3).
- **A `ModRange` above the ceiling is harmless**, because the design clamps and
  `Ceiling<R>` bounds the tail. Held unchanged.
- **`report_tail` alone does not park** a node: the cut, the reset and `Idle` are
  the node's, now once for every family.
- **New `UnitParam` kinds are append-only.** A family that needs one (the filters
  need none: `Cutoff`, `Q`, `Gain` exist; the compressor's exist) appends it with
  its pinned id (doc 022 section 10.4).

## 9. Where things live

Layering per CLAUDE.md and `scripts/check-layout.sh` (lines 381 to 383:
`tutti-dsp` and `tutti-modulation` may depend on `tutti-types` alone).

| Item | Crate | Layer | Check |
|---|---|---|---|
| `Model`, `Bounds`, `Design`, `Kernel<M>`, `Effect<M>`, `Setup`, `Effort`, `Channels`, `Ceiling<R>`, `Cadence`, `Ring`, the capability traits (`Latent`, `Ringing`, `Stable`, `Response`, `StaticCurve`, `Fidelity`, `Readout`, `Graded`), `Complex64` use | `tutti-dsp`, a neutral module (`model`), exported at the crate root | 2 | no new tutti edge; `forbid(unsafe_code)`; no atomics or `Arc`-published state; every control arrives per call |
| The families' models and designs | `tutti-dsp` (`filter/`, `dynamics/`, `delay`, `waveshape`, `convolver`, `reverb`, `resample/`, `dither`) | 2 | pure algorithm over slices; a node file holds only the adapter (CLAUDE.md) |
| `contract` module: core cases, capability groups, `design_contract!`, `kernel_contract!`, `no_alloc_contract!`, the measuring helpers, `Defect` and `Flawed` | `tutti-dsp`, behind a `contract` feature (off by default; `rand` becomes an optional dependency of it) | 2 | the precedent is `tutti-graph`'s `contract` feature (`lib.rs` lines 201 to 222; `tutti-nodes` turns it on in its dev-dependencies) |
| `Quality` | `tutti-types`, beside `RenderMode` | 1 | vocabulary only |
| `DesignNode<D>`, `NodeDesign`, `ControlParams`, `Blend`, `DryWet`, `NoBlend`, `Readouts`, `DryAlign`, `node_contract!` | `tutti-nodes` (`node_contract!` behind `testing`) | 4 | the node holds the adapter only |
| `Describe`, `Curve`, `ReadoutReader`, the session read | `tutti-session` (with doc 023's `Display`) | 7 | Bevy-free, re-exports no crate whole |
| Instruments' models | their crates' call to `tutti-dsp` (`tutti-polysynth` and `tutti-spatial` already depend on it; `tutti-soundfont` would add the edge) | 5 | layer 5 over layer 2 |
| `param_graph_node!(DesignNode<D>)` | `bevy-tutti` | 8 | a line per `D`; `check-adapter.sh` holds |
| `scripts/check-contract-coverage.sh` | scripts | | proposal (section 6.3) |

**Why the contract is a feature of `tutti-dsp` and not a `testing/` crate.** A
`testing/` crate that depended on `tutti-dsp` while `tutti-dsp`'s own unit tests
depended on it would give those tests two copies of `tutti-dsp` (the library and
the `cfg(test)` build), and their `Design` types would not be the same type. A
feature on the one crate avoids it, and is what `tutti-graph` does.

**Layering consequences found in the inventory.** `Wave` (`tutti-modulation`) and
`Fold` and `Adsr` (`tutti-types`) are below `tutti-dsp` and cannot implement its
traits; rows 25, 26 and 32 keep their queries as inherent methods or take a wrapper
in `tutti-nodes`. Moving the traits down to `tutti-types` would put an interface
into the vocabulary crate (CLAUDE.md: "vocabulary only"), and is not recommended.

### What gets renamed

| Today (on #367 or `main`) | Becomes |
|---|---|
| `ReverbNode<D>` | `DesignNode<D>` (`FreeverbNode` stays, an alias) |
| `ReverbDesign` | `NodeDesign` |
| `ReverbParams` | `ControlParams` |
| `network::contract::{DesignCase, cases}` | `contract::{DesignCase, cases}` and `reverb::cases` for the reverb-only ones |
| `network::kernel_contract::{KernelCase, Defect, Flawed}` | `contract::` the same |
| `Network` (the model) | unchanged; `Network: Model` |
| `Ceiling` | `Ceiling<R>`; the reverb's is `Ceiling<MaxDecay>` |
| `Bounds` (the struct) | stays the reverb's, as `Network`'s `Model::Bounds` |
| `Kernel` (non-generic) | `Kernel<M>` and `Effect<M>` |
| `Network::{tail, latency, is_stable}` | `impl Ringing`, `Latent`, `Stable for Network`; method names unchanged |
| CLAUDE.md "the `tutti-dsp` paragraph names `Design`, `Kernel` and `Network`" | names the traits of this module (doc 022 already promised it for 1a) |

## 10. Staged plan

Each step is one PR that passes `just ci`. Bit-identity rule: where a kernel is
unchanged, record a digest of the node's render of seeded input at the commit
before the step, and assert it after (the method of doc 022 "Bit-identity of the
General MIDI send"); a render that calls libm (`tan`, `powf`, `exp10`) uses
`tutti_nodes::testing::assert_golden`, on the target it was recorded on. No new
test compares a render against another reference render.

| Step | Content | Moves | Proves it | After |
|---|---|---|---|---|
| 0 | This document | | | |
| 1 | **Generalise on the reverb.** The `model` module and `Design`'s `type Model`/`Range`, `Kernel<M>`, `Effect<M>`, `Ceiling<R>`, `Cadence` (only `Block` used), the capability traits for `Latent`, `Ringing`, `Stable`; the `contract` feature split into core, `Ringing`, `Stable` and reverb-only groups; `ReverbNode` to `DesignNode` with `DryWet`, `NoBlend` and `()` as `Readouts`; renames of section 9 | `network/{design,kernel,contract,kernel_contract}.rs`, `tutti-nodes/src/reverb.rs`, `dry_align.rs` | The GM digest `the_general_midi_send_renders_what_it_always_did` and `retargeting_to_the_general_midi_network_changes_no_coefficient` pass unchanged; every 022 case passes under its new name and still fails its `Defect`; `tests/reverb_node*.rs` pass; port order pinned | #367 merges, ideally 1c (the FDN) so **two reverb kernels** run the generalised traits; if 1c is not in, the test-only `ToyFdn` is the second |
| 2 | **Filters: SVF, ladder, LR crossover.** `SvfModel`, `LadderModel`, `CrossoverModel` with `Response`, `Stable`, `Ringing`; `Cadence::Every(16)` in the node; separate the crossover's design from its state; `Describe::curve` and `Curve::Magnitude`; `ControlParams::snapshot`. **Delivers G8** | `filter/*.rs` in `tutti-dsp`; `SvfFilterNode`, `LadderFilterNode`, `CrossoverNode`, `EqBandNode` become `DesignNode` aliases | `Response` case: the FFT of the kernel's impulse response against the model's magnitude (mutant: a 2% cutoff error); analytic anchors; ring time from the slowest pole against the measured tail; node digests (golden-guarded, `tan`) before and after, including a swept-cutoff block, which pins that `Every(16)` is the old schedule; the segment-end case | 1 |
| 3 | **Dynamics: compressor, gate, limiter, brickwall.** `StaticCurve`, `Readout`, `Readouts` publishing through `level_cell`, `Describe::readout`, `Curve::Transfer`. Decide `Driven<M>` against the code (Q3). **Delivers G9** | `dynamics/*.rs`; the dynamics nodes | The level-sweep case (mutants: threshold +1 dB, ratio off, knee ignored); the readout equals the measured gain; node digests; a `multiband` readout test unchanged | 2 (the plumbing for `Describe`) |
| 4 | **Delay, chorus, flanger, phaser; the waveshaper.** `Response` of the frozen comb and all-pass chain; the waveshaper's `StaticCurve`, its harmonics and `Fidelity`; `Graded` for oversampling if the kernel gains it | `delay.rs`, `swept_delay.rs`, `allpass.rs`, `waveshape.rs` and their nodes | Comb response against the FFT; notch frequencies; DC and odd-symmetry cases; the aliasing case; digests | 2 |
| 5 | **Convolver, HRTF.** The published control path (5.3), `FirSummary` unified with the convolver's model, `Graded` for the partition; `ReverbKind` gains the convolution variant (doc 022 step 2) | `convolver.rs`, `ConvolverNode`, `tutti-spatial` hrtf | `Response` against the IR FFT; `Latent` = `B`; a Loan is never parked; the export-fork test | 1 and doc 022 step 2 |
| 6 | **The rest, each in the PR that needs it, in this order:** resampler (own shape trait; `Fidelity`, `Graded`, `Latent`; used by export); panners (VBAP matrix); envelopes and oscillators (a generator shape, in `tutti-polysynth` and `tutti-soundfont`, gated on a `tutti-dsp` edge for the latter); stretch; dither; strip and fold wrappers | per family | per family, with a mutation record | the step 1 shape |

**Why this order.** Step 1 is the only step that changes a shipped API, so it goes
first and on the family whose behaviour is pinned by a digest. Step 2 is next
because filters are the family that **disagrees with the reverb's cadence**: if the
traits survive the SVF's `Every(16)` and a coefficient `lerp`, they are not
reverb-shaped, and it delivers G8, whose native side (`Response`) is the
strongest use doc 023 found. Step 3 is the family that most tests the
"don't force a runtime mechanism" line (per-frame lanes), so it should come once the
node and contract are stable, and it delivers G9. Steps 4 to 6 are independent;
step 5 waits on doc 022's own step 2; the instruments are last because they are
the families where the generic node is **not** the destination, only the model and
the contract.

Doc 013's change note is updated when step 1 lands (the tail and park logic is now
the node's, not the reverb's); doc 022 gains a pointer here, and its section 4/5
text is not rewritten.

## 11. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | `Latent` and `Ringing` as supertraits of `Model`, or opt-in like the rest? | Supertraits. Every node declares both in `Shape`, the park logic needs both, and a memoryless model's answer is one line |
| Q2 | Name the shared concept `Model`? | Yes. It is doc 022's word and the owner's. `Description` is no clearer |
| Q3 | Per-frame control lanes (the dynamics' threshold and ceiling): `Cadence::Frame`, a second kernel shape `Driven<M>`, or leave those nodes hand-written? | Decide in step 3 against the code. Recommended order of preference: `Driven<M>` (the node picks per block, `Frames` vs `Base`, a branch per block); else the three nodes keep their `process` and adopt `Design`, `Model` and the contract only. Not `Cadence::Frame`: it is the fixed shape the owner warned against |
| Q4 | A series stack of EQ bands has the product response; where does a chain model live? | The application multiplies each band's `Response`. No `Chain` model until a second consumer (a multiband summed response) needs it |
| Q5 | Capability-coverage gate: a script or a macro? | A script (`check-contract-coverage.sh`), as `check-range-types.sh`. A macro cannot see every `impl` |
| Q6 | Contract as a `tutti-dsp` feature or a `testing/` crate? | The feature (the owner's direction, and the dev-cycle argument of section 9) |
| Q7 | `Quality`: four tiers or AU's 0 to 127? | Four tiers with `from_au`. A native algorithm has few real tiers, and 128 steps would be false precision |
| Q8 | May an export render at a different `Quality` from the live graph? | Not yet. It changes `Latent`, so PDC and the fork's plan differ from the live one. Revisit with `RenderMode::Offline` once a family has a tier where it pays |
| Q9 | The nodes' public inherent API (`SvfFilterNode::frequency()` returning an `Arc<AtomicF32>`, `ModDelayNode::set_depth`, ...) | Keep it by having each family's `ControlParams` expose the same handles; make the old node names aliases; fix callers in the same PR (the project's rule against shims) |
| Q10 | Publish the applied `Controls` back for a live curve at the modulated value? | Yes, as a `seq_cell` written once per changed block, in step 2, if the application wants a live curve; the base curve works without it |
| Q11 | Filters generic over `F: Real` (f64 by default): is the design generic too? | No: the model is always `f64` (analysis) and only the kernel is generic over `F` |
| Q12 | Instruments: a node of their own? | A separate design. They share `Design`, `Model`, `Kernel<M>` and the contract; the event-driven shape and `tutti-voicing` are their node's concern |
| Q13 | Share the contract's measuring helpers with doc 023's plugin probe? | Yes: `tutti-plugin` (layer 6) may depend on `tutti-dsp` with the `contract` feature, so a hosted plugin's latency and tail are measured by the code that measures a native kernel's |
| Q14 | Is `Response` single-input? A sidechained compressor has two inputs | Yes. `Response` is input 0 to `output`; the sidechain has its own curve only if a family wants it, as a second `StaticCurve` |
| Q15 | Does an analyser (row 23) deserve its own pattern? | Not here. Record that `Readout` is the likely surface and revisit when a result is wanted in the application |
