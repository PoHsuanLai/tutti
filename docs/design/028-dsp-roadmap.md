# 028: the DSP roadmap, by family

[Doc 024](024-design-model-realization.md) sorts tutti's algorithms by the
mathematical class they belong to (an LTI system, a static map, a generator, a
time-scale map) and gives each class a trait whose queries are answered from the
model's structure. This document asks what is missing from the engine against
the field, and answers it in the same terms: for each family it names the
mathematical object, the traits that state it (an existing class trait where one
fits, a new trait or combinator where none does), the queries the object
answers, the contract cases those queries make possible, and where the code
lives. The ranking and the staging come after that, because the order in which
families are built is a consequence of what each one needs from the traits, not
the other way round.

Status: design only; nothing here is implemented. It does not contradict
[doc 022](022-reverb.md) (reverb: Dattorro, early reflections and velvet noise
stay its steps; shimmer stays excluded until §9 Q2 is answered) or
[doc 026](026-stretch.md) (stretch: the listening gate is still the owner's, and
formant preservation is proposed in §3.9 as a new realization, not a change to
026's). The new traits of §3 were type-checked in a scratch crate outside the
repo (`~/tutti-audit/scratch/doc028-sketch`, `cargo check` clean against stand-ins
for the vocabulary types and doc 024 §4.2's traits restated verbatim); nothing
else here has been built, and no figure is measured. Tutti's own claims were
checked against `main` at `c7bc9f625`; external claims against the sources in
§11, and each one that could not be checked is marked there.

## Summary

- **Where tutti stands (§1).** The linear fundamentals are strong: a Simper
  trapezoidal SVF, a ladder, Linkwitz-Riley crossovers, a sidechained compressor,
  multiband, gate, a lookahead limiter, chorus/flanger/phaser, delay, Freeverb,
  an FDN, a partitioned convolver, R128 loudness with true-peak measurement,
  PolyBLEP/PolyBLAMP oscillators, VBAP and HRTF. The gaps are nonlinear
  fidelity (the waveshaper aliases; nothing oversamples), mastering safety (the
  limiter is sample-peak), a utility layer (polarity, mid/side, DC block,
  tremolo, pan laws), EQ depth, noise-shaped dither, spectral effects, pitch and
  formant processing, wavetable and FM synthesis, and ambisonics.
- **A few new abstractions, each placed against doc 024 (§2, §3).** Doc 024's
  classes cover most of the list as they stand: an EQ is `SeriesN<SvfModel, N>`,
  a pan law is an order-0 `Lti`, a limiter is a `GainControl`. The following are
  not expressible and get a trait or a combinator: `Antiderivative` (a
  memoryless map that carries its antiderivatives), `Interpolator<L>` and
  `Reconstruction` (a class 6 resampler whose prototype is an `Lti`, and the
  overshoot bound a true-peak detector needs), `Scheduled` (an `Lti` system
  selected by a settled detector level: the dynamic EQ), `GainMatrix` with the
  spherical-harmonic encode, rotation and decode models (ambisonics), `Filtered`
  and `NoiseTransfer` (a noise process through an `Lti`: noise-shaped dither),
  `BandLimited` (a generator's harmonic count against Nyquist) and `Lure` (a
  nonlinearity inside a feedback loop, which doc 024 §3 row 2 says no class
  describes). Spectral processing and formant preservation add two structures
  (`StftPair`, `PitchShift`) over existing classes.
- **What stays inherent (§3.13).** Warp markers, scale quantisation, grain
  scheduling, the tempo and beat trackers, voice allocation and a neural
  inference core do not fit a class without forcing it (doc 024 §2.4), and are
  left as data, event logic or a plain type with their own tests.
- **Spatial moves into `tutti-dsp` first (§4).** The VBAP solve, the HRTF
  convolver, the HRIR sphere build and the angle smoother go into a private
  `spatial` module of `tutti-dsp`; the nodes stay in `tutti-spatial` until
  doc 024's `DesignNode` replaces them. The owner's `vbap` crate is copied in and
  brought to tutti's shape (no `glam`, no allocation after build, unit newtypes),
  and the dependency is dropped. This is the prerequisite for ambisonics.
- **Order (§5, §6).** Table stakes first: oversampling and ADAA, the true-peak
  limiter, the utility pack and pan laws, the EQ family, noise-shaped dither;
  then stretch (doc 026), formant and pitch, reverb (doc 022), the STFT
  framework, wavetable and FM, nonlinear virtual analog; then ambisonics, which
  needs §4. Oversampling, ADAA and the true-peak limiter ride doc 024 steps 4 and
  5. Neural inference is watched, not built (§7, §9 Q4).
- **Non-goals (§7).** Real-time neural stem separation, embedding GPL or closed
  libraries (Rubber Band, élastique, the IEM and SPARTA plug-ins), and wave
  digital filters for now.
- **Open questions with recommendations (§9):** formant preservation, shimmer,
  ambisonics, neural inference, the oversampler's filters and how it declares
  latency, the home of the STFT framework, the `Lure` class, the VBAP tuple
  selection's provenance, and the SOFA reader.

## 1. Where tutti stands

Every row was checked in the code on `main`. Where this disagrees with the
research notes the roadmap grew from, the code wins and §10 lists the
correction.

### 1.1 What is there

| Area | What the code has | Where |
|---|---|---|
| Filters | Simper's trapezoidal SVF, eight responses (`SvfType`: low/high/band-pass, notch, all-pass, bell, low and high shelf); a four-stage ladder; LR crossovers; one SVF band per `EqBandNode` | `tutti-dsp/src/filter/` |
| Dynamics | compressor with external sidechain, multiband, gate, a lookahead limiter over a monotonic-minimum window and a brickwall clamp, envelope followers | `tutti-dsp/src/dynamics/`, `tutti-nodes/src/dynamics/` |
| Modulation, delay | chorus and flanger (one swept delay), phaser (swept all-pass chain), interpolated delay line | `swept_delay.rs`, `allpass.rs`, `delay.rs` |
| Reverb | Freeverb, an FDN, a partitioned convolver; the doc 022 `Network` | `reverb.rs`, `fdn.rs`, `convolver.rs`, `network/` |
| Nonlinear | `Shaper`: `Tanh`, `Atan`, `Softsign`, `HardClip`, `Crush`, `SoftCrush`, each one line, no oversampling and no antialiasing | `waveshape.rs` |
| Dither | `DitherNoise`, a xorshift32, `Rectangular` and `Triangular` (TPDF), no shaping | `dither.rs` |
| Pan | `balance_gains`: `left = min(1, 1 - p)`, `right = min(1, 1 + p)`, a linear balance that boosts nothing; VBAP's constant-power normalisation is the only power-preserving law | `tutti-dsp/src/balance.rs`, `tutti-spatial` |
| Measurement | EBU R128 integrated, range, momentary, short-term and **true peak** (through the `ebur128` crate, which oversamples 4x below 96 kHz and 2x from 96 kHz to below 192 kHz, and reads the sample peak above); YIN, onsets, beats, key, chords, transcription, spectra | `tutti-measure` |
| Synthesis | `f32x8` oscillator bank: sine, saw and pulse with PolyBLEP, triangle with PolyBLAMP, white and pink noise, `tanh`; unison, portamento, MPE; no wavetable, FM or granular | `tutti-polysynth/src/kernel.rs` |
| Resampling, stretch | FFT resampler (adapted from rubato), a windowed-sinc kernel, cubic Hermite for the sampler read; a phase-vocoder stretch that doc 026 replaces | `resample/`, `interp.rs`, `tutti-sampler` |
| Spatial | VBAP over the `vbap` crate; HRTF binaural by partitioned FFT convolution over an HRIR sphere in the `hrir_sphere_builder` format (not SOFA); `ChannelTopology` records that B-format is not speaker positions, and doc 023 G11 plans only the topology tag | `tutti-spatial`, `tutti-types/src/topology.rs` |

### 1.2 The gaps

Verified absent by search of `tutti-dsp`, `tutti-nodes`, `tutti-spatial`,
`tutti-sampler`, `tutti-polysynth`, `tutti-measure` and `tutti-types`:
oversampling and antiderivative antialiasing; a true-peak limiter (the limiter's
detector is `fold_peak`, the max-abs over the frame, and the graph's level
meter is `block_stats`, also a sample peak); polarity, mid/side, width, DC
block, tremolo and auto-pan, ring modulation, frequency shifter and Hilbert
pair; higher-order EQ slopes, tilt, dynamic and linear-phase EQ; noise-shaped
dither; spectral effects; pitch correction and formant processing; warp markers;
wavetable, FM and granular synthesis; spherical-harmonic code; stem separation.

Product claims about what other engines ship (Pro-Q 4's dynamic and
linear-phase bands, Live's Auto Shift, Logic's Stem Splitter, Valhalla's shimmer)
come from the research notes' web searches and were not re-checked here; the
roadmap does not rest on any of them. Each item below is justified by what it
does for the engine, not by who ships it.

## 2. How a family is placed

Doc 024's rule is the one used throughout: a **design** is a pure function from
typed controls to a **model**; a model is a system up to equivalence, held by a
class trait whose queries are answered from the model's structure and never by
expanding it to a canonical form (doc 024 §2.1, §2.4 item 7); a **realization**
is one hand-written structure that computes the map, held to the model by the
contract. A class trait is added only when a second family answers the same
query, and a query only one family answers stays an inherent method of its model
(doc 024 §4.2).

Three tests decide where a family lands:

1. **Does an existing class trait state its mathematics?** If yes, implement it
   and write nothing new. This is the case for most of the list (§3.4, §3.5's
   pan laws, §3.12).
2. **If not, is the missing piece a query, a structure or a class?** A query
   (an antiderivative, a polyphase `l1` bound, a harmonic count against
   Nyquist) is a trait beside the class trait. A structure (the STFT pair, a
   spherical-harmonic encode) is a model type. A class (a nonlinearity inside a
   feedback loop) is a trait of its own and goes to doc 024.
3. **If it forces a shape, leave it inherent** and say so (§3.13).

The new vocabulary, in one table. Each row is sketched and type-checked in §3.

| New item | Kind | Extends | Answers | Section |
|---|---|---|---|---|
| `Antiderivative` | trait on a `Memoryless` map | class 3 | `f1`, `f2` with `f1' = eval`, `f2' = f1` | 3.1 |
| `Adaa1<S>`, `AdaaModel<S>` | realization and model | class 4 (`SteadyState`) | steady state is `S`; one-sample memory | 3.1 |
| `Interpolator<L: Lti>` | model | class 6 (`TimeScale`, `Fidelity`) | ratio; stopband of the prototype | 3.2 |
| `Reconstruction` | trait on a `TimeScale` | class 6 | `overshoot_bound`, `branch_l1` | 3.2, 3.3 |
| `Oversampled<U, S, D>` | model | class 4 | steady state is `S`; latency is the sum | 3.2 |
| `Scheduled` | trait | class 2 / 4 | the frozen `Lti` at a settled level | 3.4 |
| `LinearPhase` | trait on an `Lti` | class 1 | constant group delay | 3.4 |
| `GainMatrix` | trait on an order-0 `Lti` | class 1 | matrix, spectral norm, column power | 3.5 |
| `SphericalField`, `ShEncode`, `ShRotation`, `ShDecode` | models | class 1 order 0 | degree energy; isometry; commutation | 3.5 |
| `Filtered<N, L>`, `NoiseTransfer` | combinator, trait | class 5 (`Noise`) with class 1 | shaped psd; Gerzon-Craven integral | 3.6 |
| `BandLimited` | trait on a `Periodic` | class 5 | top harmonic; aliasing harmonics | 3.7 |
| `StftPair`, `BinGain`, `StftEffect<G>` | structure, trait, model | class 1 up to `Fidelity` | perfect-reconstruction error; latency | 3.8 |
| `PitchShift` | trait on a `TimeScale` | class 6 | transposition and formant shift, independently | 3.9 |
| `Lure` | trait | **new class** | loop-open `Lti`, nonlinearity, small-signal system, critical gain | 3.10 |

## 3. The families

Each subsection gives the mathematics, the trait sketch (real Rust, as
type-checked), the queries, the contract cases (analytic or spec-derived: no case
compares one render against another, CLAUDE.md), and where it lives, which doc 024
or 022 or 026 step hosts it, and the effort. Effort is S (days), M (about a
week), L (more). Every contract case carries a mutant that must fail it, as doc
024 §6 requires; the mutant is named where it is not obvious.

Everything is in `tutti-dsp` (layer 2) unless stated: the crate that holds
the class traits and the `contract` feature. A node file holds only the adapter.

### 3.1 Antialiased nonlinearity: antiderivatives

**Mathematics.** A static map `f` applied to a sampled signal aliases, because
`f(x(t))` has harmonics past Nyquist. Antiderivative antialiasing (ADAA, Parker,
Zavalishin and Le Bivic, DAFx-16) treats the sampled signal as continuous
(piecewise linear), applies `f`, and low-passes by a box of one sample before
sampling again; the box has a closed form in the antiderivative `F1` of `f`:

```
y[n] = ( F1(x[n]) - F1(x[n-1]) ) / ( x[n] - x[n-1] )          (first order)
```

The second order uses `F2`, and the same construction gives a box of two samples.
When `x[n] - x[n-1]` is small the quotient is ill-conditioned and the
realization falls back to `f` at the midpoint (the limit). The stateful case
(a nonlinearity inside a filter) is Holters's extension and is not what this
section covers; it belongs to §3.10.

**Trait.** A map that can supply its antiderivatives is a `Memoryless` map with
two more functions, and the ADAA realization is generic over the trait, so a
shaper that supplies them gets ADAA with no further code:

```rust
/// A memoryless map that carries its antiderivatives.
/// Invariants: `f1' = eval`, `f2' = f1`, both continuous, and `f1(0)`, `f2(0)`
/// chosen so the realization's quotients stay finite at zero (the contract's
/// derivative case checks the first two by central differences at fixed points).
pub trait Antiderivative: Memoryless<In = f32, Out = f32> {
    fn f1(&self, x: f64) -> f64;
    fn f2(&self, x: f64) -> f64;
}

pub struct Adaa1<S: Antiderivative> { map: S, x1: f64, f1_prev: f64 }   // the realization
pub struct AdaaModel<S: Antiderivative> { pub map: S, pub order: u8 }   // the model
impl<S: Antiderivative> SteadyState for AdaaModel<S> {                  // steady state is S
    fn steady_state(&self, x: f32) -> f32 { self.map.eval(x) }
    /* time_constants: none; peak_gain = map.max_gain() */
}
```

The model is `SteadyState`, not `Memoryless`: the output depends on the previous
sample (doc 024 §2.2 class 4, and the `Memoryless` case of §6 fails it by
construction). Its steady state at a constant input is `f(x)` exactly (the
quotient's limit), and its `tail` is `order` frames. `Tanh`, `Atan`, `Softsign`,
`HardClip` all have elementary antiderivatives (`ln cosh`, `x atan x - ln(1+x^2)/2`,
`|x| - ln(1+|x|)` with a sign, and the clamp's piecewise quadratic); `Crush` and
`SoftCrush` are staircases and get no ADAA (their antiderivative is piecewise
linear and is the point of a bit crusher's aliasing; they keep oversampling, §3.2).

**Queries.** `steady_state`; `peak_gain`; the group delay (half a sample for
first order, a fractional figure that is `group_delay`, not `latency`: doc 024's
`latency` is a whole frame count, §4.2); and the aliasing floor as
`Fidelity::error_floor`. The model predicts the floor from structure: the box
attenuates harmonic `k` by `sinc(k f0 / fs)`, so the aliased bin's level is
`sum |c_k sinc(k f0/fs)|` over the harmonics that fold onto it, with `c_k` from
`Memoryless::harmonics` of the map. The derivation for a linearly interpolated
(not sinusoidal) input is an obligation of the first PR; until it is written the
floor is asserted by an inequality against the closed-form fold of the *naive*
harmonics weighted by the box, which is an analytic figure, not a render.

**Contract cases** (the `Memoryless` and `SteadyState` groups plus):

| Case | Figure | Mutant |
|---|---|---|
| Antiderivative | `f1' = eval` and `f2' = f1` by central difference at 64 fixed points per kind | a sign error in `f2` |
| DC | a constant input gives `f(x)` to 1e-6 (the fallback branch) | drop the fallback |
| Alias bin | a sine at `f0` through `Tanh` at drive 8: the folded bin at most the model's predicted level plus a stated margin | apply `f` to the samples (the naive realization) |
| Continuity | a ramp through the `dx` threshold has no step in `y` larger than `lipschitz * dx` | threshold off by 100 |
| Quotient | no non-finite output for a long run of equal inputs | divide without the guard |

**Lives in** `tutti-dsp/src/waveshape.rs` (the trait and the kernels) with
`DistortionNode` gaining an `order` control; no new crate. **Hosted by** doc 024
step 5, a second realization of the waveshaper's `Memoryless` model; the kernels
may ship before step 5 as plain functions with the analytic tests above.
**Effort** S for order 1, S more for order 2.

### 3.2 Multirate: the oversampler and the interpolator

**Mathematics.** Oversampling by `N` is: zero-stuff, low-pass with a prototype
`h` to `fs/2` of the base rate, process at `N fs`, low-pass again and decimate.
It is doc 024's class 6 (periodically time-varying, with an LTI prototype), and
the prototype's response is what the queries read. Two filter families do the
job: a linear-phase FIR (constant group delay `(L-1)/2` high-rate frames, exact
stopband from the design) and a polyphase all-pass half-band IIR cascade
(cheap, non-linear phase, group delay that varies with frequency).

**Trait.** There is no new class: a multirate stage is `TimeScale` (doc 026 §3.2,
whose `ratio` is output frames per input frame) with `Fidelity` for its stopband.
What the oversampler adds is a model that owns an `Lti` prototype and answers
two queries from it:

```rust
/// A rate change by `factor` through an LTI prototype.
/// `ratio` is `factor`; `error_floor` is the prototype's worst stopband gain,
/// read from `frequency_response` over [fs/2, factor*fs/2).
pub struct Interpolator<L: Lti> { pub factor: u32, pub prototype: L, pub taps_per_branch: usize }
impl<L: Lti> TimeScale for Interpolator<L> { fn ratio(&self) -> f64 { self.factor as f64 } }
impl<L: Lti> Fidelity  for Interpolator<L> { /* max |H| over the stopband, in dB */ }

/// What an interpolated signal can reach: a structure query, not a measurement.
/// `overshoot_bound = max over polyphase branches p of sum_k |h_p[k]|`,
/// the least `B` with `|y| <= B` for every input with `|x| <= 1`.
pub trait Reconstruction: TimeScale {
    fn overshoot_bound(&self) -> f64;
    fn branch_l1(&self, branch: u32) -> f64;
}

/// Up, a memoryless map at the high rate, down: the model of an oversampled shaper.
/// Latency is the sum of the two stages'; steady state is the map's.
pub struct Oversampled<U, S: Memoryless, D> { pub up: U, pub map: S, pub down: D }
```

`Oversampled` is class 4 (`SteadyState`): its steady state is `S`, its
`peak_gain` is `S::max_gain` times the down-stage's gain, its latency is the sum
of the stages'. It is a `Wiener`-shaped composition whose linear parts change
the rate, which is why it is a model of its own and not `Wiener` (the algebra of
doc 024 §4.9 is rate-preserving).

**Queries.** `ratio`; the stopband (`Fidelity`); `latency` and `tail`;
`overshoot_bound` (an exact sup, because for a fixed branch the worst input is
the sign pattern of the branch's taps: the bound is attained, not merely
sufficient); and the passband ripple from the same `frequency_response`.

**One open point (§9 Q7).** Doc 024 defines `latency` as the processing part of
the onset of the impulse response, in whole frames, and `group_delay` as a
response-derived query that is a different thing: an IIR has group delay and no
latency. A half-band IIR oversampler's group delay is a function of frequency,
yet PDC needs one number. The recommendation is that the stage declares the
rounded group delay at DC as its `latency` (a stated, documented choice, the same
one the plan's PDC would otherwise have to make) and the contract measures the
residual phase error as part of `Fidelity`.

**Contract cases** (`TimeScale` and `Fidelity`):

| Case | Figure | Mutant |
|---|---|---|
| Ratio | output frame count is `factor` times input, exactly | off-by-one phase |
| Stopband | a tone swept above `fs/2` through up-down at unity gain: residual under the model's `error_floor` | a prototype with half the taps |
| Passband | a tone below `0.45 fs`: gain within the model's ripple | wrong gain normalisation (`factor` missing) |
| Latency | the measured onset of an impulse equals `latency` plus the stated choice for the IIR | a delay mismatch of one |
| Overshoot | a full-scale sign-pattern input through the up stage reaches `overshoot_bound` within 1e-6 and no input exceeds it | bound from the sum of all branches |
| Quality | `Draft` to `Best`: `error_floor` non-increasing, `latency` non-decreasing (`Graded`, doc 024 §4.7) | a tier that changes nothing |

**Lives in** `tutti-dsp/src/resample/` (the polyphase FIR and the half-band
kernels, beside the windowed-sinc kernel that doc 024 consolidation 1 already put
there; the FFT resampler is a separate realization). **Hosted by** doc 024 step 5
(`Graded` for oversampling is named in its row) and step 7 for the resampler's
adoption of `TimeScale`. The kernels ship first as plain code. **Effort** M.

The same `Interpolator` is the sampler's polyphase sinc tier (doc 026 Q5: the
alias floor of Hermite at transposition above 1) and the async SRC beyond the
FFT resampler: a `TimeScale` with a kernel table, the engine research's item 7.

### 3.3 True peak, and the inter-sample-peak limiter

**Mathematics.** The true peak of a sampled signal is the peak of its
band-limited reconstruction, which can exceed every sample (an inter-sample
peak, ISP). BS.1770 Annex 2 estimates it by 4x interpolation with a specified
48-tap filter; the figure is a *measurement of a given signal*. A limiter
needs the other thing: a **guarantee over all signals**, and for the
reconstruction by a known interpolator `P` that guarantee is `overshoot_bound`
of §3.2, a structure query.

**Composition.** The limiter is doc 024's class 4 with a different detector:

```
model = GainControl< Wiener< TruePeakDetector<R>, GainMap > >

TruePeakDetector<R: Reconstruction>   // up by R's factor, max-abs over the phases, window of `window` frames
GainMap                                // the ceiling curve, class 3
```

with the gain applied to the *base-rate* signal after a lookahead delay, as
today's `LimiterNode` does. The detector is the only new part; `GainControl`,
`Wiener` and `Memoryless` are doc 024 §4.9's.

**The guarantee, as a query.** Let `g[n]` be the gain applied to `x[n]` and let
the realization hold `g` at its window minimum over the interpolator's support
(`taps_per_branch` frames either side: the lookahead of today's
`MonotonicMinDeque` already is this window minimum, sized to the support). Then
the reconstruction of `g x` satisfies

```
|y_rec| <= overshoot_bound(R) * max_window g * max |x| <= ceiling
```

whenever the detector's gain is `ceiling / (overshoot_bound * max |x| over the
window)`. The model's `PeakGuarantee::output_true_peak_bound` states it, and the
contract checks it against sign-pattern worst cases (below). The BS.1770 filter
and the limiter's own interpolator need not be the same: the meter reports what
the standard defines, the limiter proves what it guarantees with its own
`overshoot_bound`, and the contract checks the *guarantee* against an
oversampled measurement at a higher factor, never against another limiter.

**Contract cases** (`SteadyState` group of doc 024 §6.2 plus):

| Case | Figure | Mutant |
|---|---|---|
| ISP | a full-scale pair of alternating-sign samples at `fs/4` phase-shifted so the sample peak is 0.707 and the true peak 1.0: after the limiter the 8x-oversampled peak is at most `ceiling + 0.2 dB` (the model's stated margin) | detect on the sample peak (today's limiter) |
| Worst case | the sign-pattern input of the interpolator's worst branch: output true peak within `output_true_peak_bound` | window shorter than the support by one |
| Transparent | a signal whose true peak is under the ceiling by 1 dB: gain exactly 1 | detector bias |
| Latency | declared `latency` equals the lookahead plus the interpolator's; PDC aligned | omit the interpolator's |
| Steady state | `steady_state(level)` equals the ceiling curve at a settled level | wrong knee |

**Lives in** `tutti-dsp/src/dynamics/` (a `TruePeakDetector` and the extension of
`LimiterKernel`, which doc 024 consolidation 9 creates); `tutti-nodes` gets a
mode on `LimiterNode`. The meter side (`tutti-measure`) is unchanged: it already
measures true peak through `ebur128`. What is missing is a **streaming true
peak as a plan-op meter** (`Editor::open_meter` is sample peak), which is a
separate, small item: `Op` over the same interpolator, per channel, read as a
`level_cell`. **Hosted by** doc 024 step 4 (dynamics) on top of §3.2. **Effort**
S to M.

### 3.4 The EQ family

**Parametric EQ.** Doc 024 Q4 resolved it: one design whose model is
`SeriesN<SvfModel, N>`, with an exact product `frequency_response`, stability of
the worst band, ring time of the slowest (doc 024 §4.9). Nothing new. Higher-order
slopes (Butterworth cascades of 12 to 96 dB per octave) are `SeriesN` of the same
band model with the pole angles of the Butterworth polynomial; tilt and
Baxandall shelves are closed-form biquads, one more `SvfType` or a sibling model.
Contract: the response equals the analytic Butterworth magnitude
`1/sqrt(1 + (f/fc)^(2n))` within the bilinear warp of the design (the warp is a
stated bound); `-3.01 dB` at `fc` exactly; slope at one decade is `20 n dB`.

**Dynamic EQ.** A dynamic band is a bell whose gain follows a detector on a
band-passed sidechain. Doc 024 has the pieces (`GainControl` over a
`Wiener<Follower, GainMap>`) but the controlled thing is not a gain on the
signal: it is the *gain parameter of an LTI filter*. The structure is an `Lti`
system chosen by a settled level, which doc 024 states only as the frozen-time
approximation of `Ltv` (a function of time, not of a level). The new trait is
the level-indexed version:

```rust
/// A system that is `Lti` at each settled detector level: a dynamic EQ band,
/// a gain-scheduled filter. The frozen `Lti` at a level is the model's answer
/// to every class-1 query; the time constants are the detector's.
pub trait Scheduled: System {
    type Frozen: Lti;
    fn at(&self, level: Db) -> Self::Frozen;
    fn time_constants(&self, out: &mut [Seconds]) -> usize;
}
```

`Scheduled` is to a level what `Ltv::frozen` is to time. The queries are the
class-1 ones at a level (`at(level).frequency_response`) and the detector's
`SteadyState` (the gain at a level). **Contract:** at a steady sine of level `L`
(settled for ten time constants) the measured gain at the band's centre equals
`|at(L).frequency_response|` within 0.1 dB; the response at the threshold is the
static bell; the attack and release time constants are measured from a level
step against `time_constants`; and an `Lti` property that must *not* hold, as a
mutant check: superposition fails (the system is nonlinear), which keeps the
model out of `Lti`.

**Linear-phase EQ.** A symmetric FIR designed by frequency sampling from the
magnitude `H(f)`, run by the existing partitioned `Convolver`. The model is an
`Lti` whose group delay is a constant:

```rust
pub trait LinearPhase: Lti {
    /// Constant group delay in frames: `(taps - 1) / 2` for a symmetric FIR.
    fn group_delay_frames(&self) -> f64;
}
```

Its `System::latency` is that delay (it is processing latency: a convolver
partition and the filter's centre are both declared to PDC, as the convolver's
partition already is), and the contract's latency case measures the impulse's
onset at the centre tap. **Contract:** the measured phase is linear
(`phase(f) = -2 pi f * delay` within the frequency-sampling error), the
magnitude equals the design's on the design grid exactly and between grid points
within a stated interpolation bound, and `l1_norm` equals the sum of absolute
taps (an exact `Lti` query).

**Match EQ and spectral dynamics** are not families of their own: match EQ is a
`tutti-measure` spectrum, a smoothing and the linear-phase design above;
frequency-selective dynamics is a `BinGain` over the STFT (§3.8).

**Lives in** `tutti-dsp/src/filter/` (the cascade designs, tilt, the FIR design)
and `tutti-nodes` (a multi-band `EqNode`, which holds the adapter only).
**Hosted by** doc 024 steps 2 to 4 (the SVF and `SeriesN`, then dynamics for the
detector). **Effort** M for the parametric node with slopes and tilt; M for
dynamic EQ; M for linear-phase.

### 3.5 Gain matrices: pan laws, VBAP and ambisonics

**Mathematics.** Every panner is a gain matrix: doc 024 §4.8 already states it
(order-0 `Lti`, `spectral_radius() == 0.0`, latency 0, tail `Finite(0)`,
`frequency_response` the matrix at every frequency), and rows 31, 32, 44 and 52
are that. Ambisonics is a gain matrix as well, with structure that the plain
trait hides: the encode is a matrix of real spherical harmonics evaluated at a
direction, a rotation is a block-diagonal orthogonal matrix on the harmonic
coefficients, and a decoder is a matrix from coefficients to loudspeakers.

**Trait.** One trait lets the queries that are true of every gain matrix be
asked of all of them, and the spherical-harmonic models add the invariants:

```rust
/// An order-0 `Lti` with its matrix exposed (`rows` outputs, `cols` inputs).
pub trait GainMatrix: Lti {
    fn rows(&self) -> usize;  fn cols(&self) -> usize;
    fn matrix(&self, out: &mut [f32]);
    /// The largest singular value; exactly 1 for an isometry (an energy-preserving map).
    fn spectral_norm(&self) -> f64;
    /// Sum over rows of the squared gain from input `col`: power preserved when 1.
    fn column_power(&self, col: usize) -> f64;
}

pub struct Order(pub u8);                   // channels = (order + 1)^2
pub enum ShNorm { Sn3d, N3d }               // ACN channel order throughout

/// Real spherical-harmonic encode of a plane wave from `(azimuth, elevation)`.
pub struct ShEncode   { pub order: Order, pub norm: ShNorm, pub azimuth: f64, pub elevation: f64 }
/// Rotation of a sound field: block-diagonal, one orthogonal block per degree.
pub struct ShRotation { pub order: Order, pub norm: ShNorm, pub block_diag: Vec<f64> }
/// Decode to `speakers` loudspeakers.
pub struct ShDecode   { pub order: Order, pub speakers: usize, pub weights: Vec<f64> }

pub trait SphericalField: GainMatrix {
    fn order(&self) -> Order;
    /// Sum over m of Y_nm(d)^2 for degree n: 1 for SN3D and 2n + 1 for N3D, at
    /// every direction d (the addition theorem).
    fn degree_energy(&self, n: u8) -> f64;
}
```

**Queries and invariants.**

- *Pan laws.* A law is a function `Pan -> (gl, gr)`; its model is a 2x2 `GainMatrix`
  whose queries are the figures that define the law: `column_power` is 1 for the
  constant-power law (`gl = cos(theta), gr = sin(theta)`, `theta = (p + 1) pi/4`),
  `gl + gr` is constant for the constant-amplitude law, `gl = gr = 10^(-p/20)` at
  centre for the -3, -4.5 and -6 dB laws (`p` the centre attenuation in dB), and
  mono compatibility (`gl + gr` at the extremes) is a stated number. These are the
  contract cases (the spec is the oracle; the laws are named conventions, not
  anyone's implementation).
- *VBAP.* The matrix has at most three non-zero entries per column (a tuple), all
  non-negative, `column_power = 1` (the normalisation of Pulkki 1997), and the
  active tuple's gains are all positive when the direction is inside its
  triangle. Continuity in azimuth and elevation is the case that a bad tuple
  selection fails.
- *Spherical harmonics.* `degree_energy(n)` is the invariant above, at random
  directions. `ShEncode` of a direction has `column_power` equal to
  `sum_n degree_energy(n)` (`(order+1)^2` for N3D at full order, `order + 1` for
  SN3D). **Rotation is an isometry**: `spectral_norm = 1` and `column_power = 1`
  for every column, `hinf_norm = l1_norm`-bounded by `sqrt(channels)`. **Rotation
  commutes with encode**: `R(q) E(d) = E(q d)`, a figure of the two *models*
  (two matrices computed independently, one from the Wigner-D recurrences and one
  from the harmonics at the rotated direction), not of a render. The sketch checks
  the identity as a function over `GainMatrix`:

  ```rust
  pub fn rotation_commutes<E: SphericalField, R: GainMatrix>(
      e_of_d: &E, r: &R, e_of_qd: &E, tol: f64) -> bool
  ```
- *Decoders.* A basic (sampling) decoder has `Z^T Z ~ I` for a regular layout;
  a max-rE weighting lowers the energy spread (its `rE` figure is a query of the
  decoder: `rE = max-rE` closed form per order); AllRAD (Zotter and Frank 2012)
  decodes to a virtual dense layout and pans with VBAP: **its decoder matrix is
  the composition `Series<VbapMatrix, ShDecode>` of this section's two models**
  over an imaginary layout, which is the reason VBAP must be a clean `GainMatrix`
  in `tutti-dsp` first (§4). Binaural decode is `Series` of a decoder to a virtual
  layout and the HRTF's `Lti` pair (doc 024 row 45).

**Channel convention.** ACN order and SN3D normalisation (the ambiX convention);
`ShNorm::N3d` for conversion. FuMa is an import/export concern, not a model.
Doc 023 G11's `Ambisonic` topology (order, ordering, normalisation) in
`tutti-types` is a prerequisite: a bus must be able to say what it carries
before a node can refuse to wire it wrongly.

**Lives in** `tutti-dsp/src/spatial/` (the matrices, §4) and
`tutti-nodes` or `tutti-spatial` (the nodes: encode, rotate, decode,
binaural). **Hosted by** doc 024 step 7 (panners and fold are class 1 at order 0)
and step 6 (the HRTF as a published model); ambisonics waits on §4 and G11.
**Effort** S for pan laws; M for VBAP in `tutti-dsp` (§4); L for ambisonics
(encode, rotation to order 3, decode, binaural, the `Ambisonic` topology).

### 3.6 Dither and noise shaping

**Mathematics.** A quantiser with error feedback is
`v[n] = x[n] - sum_k c_k e[n-k]`, `y[n] = Q(v[n])`, `e[n] = y[n] - v[n]`, so
`y - x = e * (1, -c_1, ..., -c_K)`: the error is the quantisation noise filtered
by a **noise transfer function** `NTF(z) = 1 - sum_k c_k z^-k`, monic by
construction. With subtractive-equivalent dither the error `e` is a noise
process of known density (rectangular: variance `lsb^2/12`; the triangular dither of `DitherNoise`, on
`[-1, 1]` LSB: `lsb^2/6`; doc 024 row 19 holds these figures; the quantiser's own
error adds `lsb^2/12`), and the output noise is
`e` shaped by the NTF.

**Trait.** Doc 024's `Noise` (variance, psd) is the process; the shaping is an
`Lti`. The combinator that says so is generic and useful outside dither (a
coloured noise source, a filtered generator):

```rust
/// A noise process through an LTI filter: `psd(f) = psd_N(f) * |H(f)|^2`,
/// `variance <= variance_N * l1_norm(H)^2` (an upper bound).
pub struct Filtered<N: Noise, L: Lti> { pub noise: N, pub filter: L }
impl<N: Noise, L: Lti> Noise for Filtered<N, L> { /* as above */ }

/// A monic NTF.
pub trait NoiseTransfer: Lti {
    fn is_monic(&self) -> bool;
    /// Mean of ln|NTF(e^jw)| over the circle: 0 for a minimum-phase monic NTF
    /// (the Gerzon-Craven theorem), positive otherwise.
    fn log_integral(&self) -> f64;
}
```

**Queries.** The shaped psd at any frequency (the figure a designer reads), the
total noise power, and the Gerzon-Craven integral: noise cannot be removed
everywhere, only moved, so a shaper that lowers the audible band by `A` dB
raises something else, and `log_integral` is the proof obligation. Weighted
shapes (a psychoacoustic curve) are *designs* of `c_k` against a stated
weighting; the proprietary curves (POW-r and others) are not copied: the filters
are designed in `tutti-dsp` from published weighting curves (Lipshitz, Wannamaker,
Vanderkooy 1992 for the theory).

**Contract cases:** monic (`h[0] = 1` exactly); minimum phase (all zeros inside
the unit circle: `log_integral` within 1e-6 of 0); the measured error spectrum of
a quantised silence with TPDF dither equals `Filtered::psd` within a stated
statistical tolerance over a long run (a figure from the model, with the seed
fixed); the total error power within the `variance` bound; the non-shaped dither
case reproduces doc 024 row 19's `lsb^2/6`. Mutants: a non-monic NTF; a feedback
sign error; shaping with the wrong sign of `c_k`.

**Lives in** `tutti-dsp/src/dither.rs` (the shaper with an error-feedback
state beside `DitherNoise`, and the designs of the NTF); `tutti-export` selects
it in `process/dither.rs`. **Hosted by** doc 024 step 7 (dither). **Effort** S
for a fixed set of NTFs (a first-order, a few higher-order with stated weighting);
M for a sample-rate-dependent weighted design.

### 3.7 Generators: band-limited oscillators, wavetable and FM

**Mathematics.** Doc 024 class 5, `Periodic`: a frequency and a harmonic
series. Aliasing is the harmonics above Nyquist folding back; the question a
generator must answer is "how many harmonics, at what level, lie above `fs/2`".

```rust
/// A periodic generator with a finite band. The count of harmonics above `floor`.
pub trait BandLimited: Periodic {
    fn top_harmonic(&self, floor: Db) -> usize;
    /// Harmonics `k` with `k * frequency > rate/2` above `floor`: the ones that fold.
    fn aliasing_harmonics(&self, rate: Hz, floor: Db) -> usize { /* provided */ }
}
```

- *PolyBLEP/PolyBLAMP* (what the polysynth has) are not band-limited: they
  attenuate the first alias harmonics. Their `top_harmonic` is unbounded and
  `aliasing_harmonics` is a measurement query, which is why they implement
  `Periodic` but not `BandLimited`; the existing saw and pulse keep their rows
  (doc 024 row 34).
- *Mipmapped wavetable.* A set of tables, one per octave band, each holding only
  the harmonics below Nyquist for the highest `f0` it serves. `WavetableModel`
  carries `tables: Vec<Vec<Complex64>>` (a table's harmonics) and the selected
  level; `harmonic(k)` is the table's coefficient and `top_harmonic` is the
  level's top. The invariant, a contract case: for every level `m` and every
  `f0` the level serves, `aliasing_harmonics(rate, floor) == 0`. Table build
  (the FFT, zeroing above the limit, the inverse FFT) is control-side and uses
  `tutti-dsp`'s `RealFft`; the realization is a lookup with a 4-point
  interpolation and a crossfade between adjacent levels, and the interpolator's
  droop is a `Fidelity` term.
- *FM and phase modulation.* A carrier at `fc` phase-modulated by a modulator at
  `fm` with index `beta` has sidebands at `fc + n fm` with amplitude `J_n(beta)`
  (Bessel), a closed form; `PmPair::harmonic(k)` sums the sidebands that land on
  harmonic `k` of the common fundamental `gcd(fc, fm)`. `top_harmonic` is Carson's
  rule, `ceil((fc + (beta + 1) fm) / f0)`, which is an engineering bound (98% of
  power), not an exact edge: the model states it as such, and the contract
  measures the energy beyond it. Feedback FM (an operator modulating itself)
  has no closed form for its sidebands and is inherent to the realization (it
  oversamples); only its first-order behaviour is queried.

**Contract cases:** harmonic amplitudes of a built table equal the source
coefficients to 1e-6 at table build; no harmonic above `top_harmonic` has energy
above `floor` in a rendered note at the top `f0` of each level (an FFT of the
render, which is a measurement of one render against its own model, not against
another render); the crossfade between levels has no discontinuity larger than
the interpolation error; the Bessel amplitudes match `J_n(beta)` for tabulated
`n` and `beta`; DC is zero.

**Lives in** `tutti-dsp/src/` for the table build, the lookup and the PM
operator maths (a new `wavetable.rs` and `pm.rs`); `tutti-polysynth` keeps its
`f32x8` bank and calls in (doc 024's "instruments' models call into
`tutti-dsp`"; a SIMD bank is the realization, and the model is the dsp type).
**Hosted by** doc 024 step 7 (oscillators). **Effort** M each. Additive and
physical modelling (Karplus-Strong, modal banks) are small `Periodic` or
`Lti` models (a delay line with a loss filter is an `Lti`; its `frequency` is
the comb's first peak) and ride step 7 as demand appears; granular is §3.13.

### 3.8 Spectral processing: one framework

**Mathematics.** A short-time Fourier transform processor analyses with window
`w_a`, hop `H`, applies a map in the frequency domain, and resynthesises with
`w_s` by overlap-add. **Perfect reconstruction** (the identity map returns the
input) holds when the product window is COLA at the hop:
`sum_m w_a[n - mH] w_s[n - mH] = 1` for every `n`. `StftGeometry` already checks
COLA of one window; the pair generalises it to two.

```rust
pub struct StftPair { pub size: usize, pub hop: usize, pub analysis: Vec<f32>, pub synthesis: Vec<f32> }
impl StftPair {
    /// The worst departure from perfect reconstruction: max over n of |sum_m w_a w_s - 1|.
    pub fn pr_error(&self) -> f64;
}

/// A per-bin real gain, constant in time: a spectral EQ, a settled spectral gate.
pub trait BinGain { fn gain(&self, bin: usize) -> f32; }

/// The model of an STFT effect with a fixed per-bin gain.
pub struct StftEffect<G: BinGain> { pub pair: StftPair, pub gains: G }
// latency = size - hop (centre-aligned analysis needs the whole window); tail = size;
// Fidelity = the time-aliasing of circular convolution at this size and these gains.
```

**What class is it?** Not `Lti` exactly: a per-bin gain applied to a frame is a
circular convolution, so the system is LTI only to the extent the gain's impulse
response is short against the window. `StftEffect<G>` therefore implements
`System` and `Fidelity` (the time-aliasing floor, derived from the length of the
gain's inverse transform) and offers the class-1 queries **at the sampled
response** (`gain(k)` at the bin centres) as an approximation stated with its
bound, as doc 024 Q23 states the frozen-time bound for `Ltv`. A map that is not
a per-bin multiplier (freeze, morph, a gate with hysteresis) is a nonlinear or
stateful map over frames: it is a `BinMap` that the realization runs, with its
own cases, and its model is `System` plus the pair, nothing more; the class
traits stay out (doc 024 §2.4 item 8, a family's own cases are its own).

**Relation to doc 026.** `LockedVocoder` is a *realization of `TimeScale`* over
the same pair: it changes the hop between analysis and synthesis (the time-scale
map) and keeps the pair's COLA at the synthesis hop. The two share `StftGeometry`,
`RealFft`, the windows and the pair; they share no per-bin map. §9 Q6 settles
where the pair lives.

**Contract cases:** `pr_error` under 1e-6 for the shipped window and hop pairs
(Hann with hop `size/4`, sqrt-Hann analysis and synthesis at `size/2`); identity
map returns the input delayed by `latency` to 1e-5; a constant gain of `g` is
`g` times the input; a bin gain of 0 at the Nyquist bin removes a Nyquist tone;
block independence (blocks 1, 17, 64 bit-identical, as the vocoder's); no
allocation. Mutants: a hop that breaks COLA; a missing synthesis window.

**Lives in** `tutti-dsp/src/stft.rs` (beside `geometry.rs` and `fft/`); the
kernels (gate, denoise by spectral subtraction with a Wiener gain, freeze,
blur, resonator) are `BinMap` types over it, each a small file, and
`tutti-nodes` holds one `StftNode`. A channel vocoder (filter bank and
followers) and a frequency shifter (Hilbert) are `Lti` and `Series`, not
STFT. **Hosted by** after doc 026 stage 2 (the vocoder is written in
`tutti-dsp`; reuse its pair). **Effort** M for the framework, S per kernel.

### 3.9 Pitch and formant

**Mathematics.** Doc 026 settles the time-scale half: `TimeScale`, `Retime<M>`,
`Transposed<R>` (pitch by resampling the stretcher's output). Formant
preservation changes what a pitch shift does to the **spectral envelope**: a
plain transposition by `k` moves the formants by `k` too (the chipmunk effect);
preserving them corrects the shifted spectrum by `E(f) / E_k(f)`, where `E_k` is
the envelope after the shift, so the envelope stays where it was. The envelope is the
smooth curve through the harmonic peaks, estimated by a cepstral method: the
true-envelope estimator of Röbel and Rodet (DAFx-05) iterates a cepstral
smoothing to touch the peaks. Pitch and formant are two independent controls:

```rust
/// A time-scale system whose pitch and spectral envelope move independently.
pub trait PitchShift: TimeScale {
    fn transpose(&self) -> Cents;
    /// How far the spectral envelope moves: 0 = formants preserved.
    fn formant_shift(&self) -> Cents;
}
pub trait SpectralEnvelope {
    fn log_magnitude(&self, f: Hz) -> f64;     // from a cepstrum of `order` coefficients
    fn order(&self) -> usize;
}
```

`Transposed<R>` implements `PitchShift` with `formant_shift == transpose` (the
envelope rides along). A realization that applies the envelope correction
implements it with `formant_shift == 0` and is not resampled twice (doc 026
§4.3 already reserves this). The envelope estimator is its own type, a `Design`
from `(order, f0 range)` to a model whose queries are the cepstral order and the
smoothing bandwidth (`fs / order`): the true envelope is a *measurement of a
signal*, so it lives with the realization, not the model.

**Contract cases** (analytic, on synthetic vowels): a harmonic stack shaped by a
known all-pole envelope (a Klatt-style formant resonator pair), shifted by
`+7` semitones with `formant_shift = 0`: the fitted formant centres move by under
`fs/order` while the pitch moves by 7 semitones within 1 cent; with
`formant_shift = transpose` the formants move by the same 7 semitones; the
envelope fit of a stack with a known envelope equals it within a stated dB at the
harmonic peaks; the spectral envelope estimator is block independent.

**Pitch correction** (a scale quantiser with a retune speed over YIN) is a pure
function of `(detected pitch, scale, speed)` plus a one-pole smoothing of the
cents error: the quantiser is data (inherent, §3.13), the smoothing is an `Lti`
`Smoother` (doc 024 consolidation 2), and the shifter is a `PitchShift`. It is a
node composition, not a family.

**Lives in** `tutti-dsp` (the envelope estimator in `stft` or `envelope.rs`,
`PitchShift` beside doc 026's `TimeScale`); the shifter as a live effect needs a
push shape (an input-fed `Effect`), because doc 026's `Retime` is
position-driven and rejects an input-fed trait ("a live input that would need
real latency has no consumer", doc 026 §12). **A live pitch shifter is
that consumer**; §9 Q1 asks whether to open it. **Hosted by** doc 026 stage 8
(other modes) for the offline and sampler case; the live effect after it.
**Effort** M for the envelope and the formant realization, M to L for the live
shifter and pitch correction.

### 3.10 A nonlinearity inside a feedback loop

**The gap in doc 024.** Row 2 (the ladder) is "Partly": class 4 is a composition
of a static map with an LTI part, which does not describe a nonlinearity *inside*
the loop. A diode clipper and a ladder with per-stage saturation are the same
shape. The mathematical object is a **Lur'e system**: a linear system `L(z)`
whose output passes through a memoryless nonlinearity `phi` back to its input,

```
u[n] = x[n] - k * phi(y[n]),      y = L(z) u        (the loop closes through phi)
```

and the classical results about it are exactly the queries the engine needs.

```rust
pub trait Lure: System {
    type Phi: Memoryless<In = f32, Out = f32>;
    type Open: Lti;
    fn open_loop(&self) -> &Self::Open;          // L(z)
    fn nonlinearity(&self) -> &Self::Phi;
    fn feedback_gain(&self) -> f64;              // k
    /// The `Lti` system with phi replaced by its slope at the origin.
    fn small_signal(&self) -> impl Lti;
    /// Sufficient for BIBO stability (small gain): hinf(L) * lipschitz(phi) * k < 1.
    fn small_gain_stable(&self) -> bool { /* provided */ }
    /// The k at which the small-signal loop reaches the unit circle (self-oscillation).
    fn critical_gain(&self) -> f64;
}
```

**Queries.** `small_signal` gives the frequency response at low level (what the
ladder's cutoff and resonance mean), `critical_gain` gives the resonance at
which it self-oscillates (for four identical one-poles in continuous time,
`k = 4` at `omega = omega_c`; the trapezoidal discretisation moves it, and the
model answers for the discrete system), `small_gain_stable` gives a guarantee
that does not depend on the realization's solver, and `max_gain` of `phi` bounds
the loop's output swing for the engine's headroom. The linearisation at a
*level* (the describing function) is a later query, not offered now.

**Realization.** The loop is delay-free: the output `y[n]` depends on `phi(y[n])`.
A trapezoidal discretisation turns it into an implicit equation per sample, which
a realization solves (a Newton step or two, or a table). Today's `ladder_step`
avoids the solve: it computes `u = tanh(x - k s3)` from the last stage's
**stored state**, not the solved output, so its feedback carries a unit delay and
only the loop-sum nonlinearity, with the four stages linear (§10, correction).
A solved realization is a second realization of the same `Lure` model (the
equivalence-class point of doc 024 §2.1): its `small_signal` and `critical_gain`
are the same figures, and the contract checks both realizations against them.

**Contract cases:** at an input level where `phi` is linear (below `1e-3`) the
realization's impulse response equals `small_signal.frequency_response` (the
`Lti` realization case, at low level); below `critical_gain` an impulse decays,
above it a small input grows until `phi` limits it (self-oscillation), with the
growth rate and the limit from the model; the output is bounded for bounded input
when `small_gain_stable`; DC: the fixed point of the loop equation equals the
realization's settled output; block independence; no allocation, and the solver's
iteration count is bounded (a mutant that lets it run unbounded fails the
no-alloc and time gate). Mutants: `phi` applied after the sum instead of inside
the loop; a solver that stops after zero iterations.

**Lives in** `tutti-dsp/src/filter/` and the `model` module (the `Lure` trait
goes to doc 024 §4.2 at the PR that first implements it; §9 Q8). Candidates:
the nonlinear TPT ladder, a diode clipper (state-space with a sinh-shaped
`phi`), a Moog-style resonance model. Nodal DK for multi-nonlinearity circuits is
the general case and is a later question; wave digital filters are an
alternative authoring method for the same class and are a non-goal (§7).
**Hosted by** after §3.1, §3.2 (these circuits need oversampling) and doc 024
step 2 (the filter model). **Effort** M.

### 3.11 Reverb

Doc 022 owns the reverb; this roadmap adds only what the model of doc 024
makes cheap and does not change 022's order. In the `Parallel`/`SeriesN`
algebra: the Dattorro plate (step 3) and early reflections as
`Parallel<Taps, Network>` (step 3b) are as 022 and 024 §4.9 say. **Two
additions from the literature that fit the existing `Network` and need only
design work:** a velvet-noise decorrelator for stereo width (a sparse `Fir`
`Lti`: its `l1_norm` is the number of non-zero taps, its response is the sum of
a few delayed unit pulses, exact), and colourlessness and matrix-optimisation
queries for the FDN (Schlecht and Habets on feedback matrices: the spectral
flatness of the lossless prototype is a query of the model, and the optimised
matrix is a design output). Both are `Network` designs and queries, in
`tutti-dsp/src/network/`; neither needs a trait. Shimmer is §9 Q2. **Effort** M
(022's own steps) plus S for each addition.

### 3.12 The utility pack

Each of these is a node over an existing class and needs no new trait.

| Item | Math and class | Queries and contract | Where |
|---|---|---|---|
| Polarity, mono sum, channel swap | order-0 `Lti` (a signed matrix) | the matrix; `column_power` | `tutti-nodes` over `Fold`-like matrices |
| Mid/side encode, decode, width | order-0 `Lti`: `M = (L+R)/2`, `S = (L-R)/2`; width scales `S` by `w` | `M/S` round trip is the identity to 1e-7; `w = 0` is mono; the matrix is orthogonal up to the stated scale | `tutti-dsp` (matrix) and node |
| DC blocker | `Lti`, one-pole high-pass `H = (1 - z^-1)/(1 - R z^-1)` | `H(1) = 0` exactly; `-3 dB` at the stated corner within the bilinear warp; stable for `R < 1` | `tutti-dsp/src/filter/` |
| Tremolo, auto-pan | `y = x (1 + d * lfo(t))`: a time-varying order-0 gain driven by a class 5 LFO | the modulation sidebands at `+-f_lfo` of amplitude `d/2` for a sinusoidal LFO (closed form); auto-pan has `column_power` varying as the law | `tutti-nodes` over `ModulatorNode` |
| Ring modulation | a product with a `Periodic` carrier: sidebands at `fc +- fm` | sideband amplitudes closed form; with an analytic carrier no aliasing for `fc + fm < fs/2` | `tutti-nodes` |
| Frequency shifter, Hilbert pair | the Hilbert transform is an `Lti` (two all-pass chains 90 degrees apart); single-sideband shift is a `Series` and a product with a complex carrier | phase difference of the pair `90 deg +- e` over the band, `e` a stated bound; sideband suppression `20 log10` of the ratio | `tutti-dsp/src/allpass.rs` |
| Pan laws (§3.5) | order-0 `GainMatrix` | §3.5 | `tutti-dsp/src/balance.rs` (generalise `balance_gains`) |
| Delay variants (ping-pong, tempo-synced, ducking) | `Lti` with feedback structure inside the model (doc 024 Q17: the loop stays structure); ducking is a `GainControl` in the feedback path | loop gain below 1 is the stability query; the echo spacing equals the tempo-derived time | `tutti-nodes` over `DelayLineNode`, tempo from the transport |
| De-esser, transient shaper | class 4: a band-passed detector `GainControl` (de-esser); a differential of two followers `GainControl` (transient shaper) | `steady_state` curve; time constants | after doc 024 step 4 |

**Effort** S each; the whole pack is a handful of small PRs.

### 3.13 What stays inherent

Doc 024 §2.4 says a family that cannot be stated by a class without forcing it
stays a plain type with its own tests. These do:

- **Warp markers and the beat map.** A monotone piecewise-linear map from beat
  to source frame: data, a pure function of position (doc 024 row 29's
  reasoning), consumed by doc 026's `Warp`. The auto-warp from `tempo` and
  `beat` is a `tutti-measure` analysis plus a data edit, not a signal class.
  Doc 026 §6.2 already makes this a longer segment list; it needs no new trait.
- **Scale quantisation and retune policy** (pitch correction): a pure function
  from a detected pitch to a target pitch, data plus event logic.
- **Granular synthesis.** Grains are events (class E) scheduled over a
  position-driven `Source` (doc 026's substrate); each grain is a windowed read.
  The scheduler is inherent; the window and the read are existing primitives. The
  queries that make sense (grain density, overlap) are the scheduler's, not a
  class's.
- **Tempo, beat, key and chord tracking, onset detection.** Measurements
  (class M).
- **Voice allocation, MPE state.** `tutti-voicing`'s `Voicing::step`.
- **A neural inference core** (recurrent LSTM/GRU and 1-D convolution layers for
  amp and pedal models, run from weights loaded as data). It is a stateful
  nonlinear system with no closed-form queries: its model is `System` (latency 0,
  `tail` `Unbounded`), plus the layer shapes and a `lipschitz` bound from the
  weights' norms, and nothing else; forcing it into a class would claim
  analytic answers it does not have. Its contract is the layer equations on hand-built
  tiny networks with known outputs (an LSTM with zero weights and a set bias has
  a closed-form state), run against the *spec of the layer*, never against
  RTNeural or NAM. See §9 Q4 for whether to build it.
- **Spring and tape saturation** (hysteresis): non-linear with memory in the
  nonlinearity itself; not a `Lure` (the memory is in the map), not class 4.
  A Jiles-Atherton hysteresis model is a state-space map with no class here and
  is left out of scope until a consumer exists.

## 4. Spatial consolidation

`tutti-spatial` is the one effect family outside the pattern CLAUDE.md sets
("a pure algorithm over slices goes in `tutti-dsp`; a node file holds only the
`Node` adapter"): about half its non-test source (roughly 1,550 of 3,100 lines)
is pure algorithm. The move is a prerequisite for ambisonics (§3.5): AllRAD is
the composition of a VBAP matrix and a decoder, and both must be `GainMatrix`
models in `tutti-dsp` where the traits and the `contract` feature live. The
review behind this section is the owner's own (read-only, 2026-11-11, option 2).

### 4.1 What moves, what stays

| Moves to `tutti-dsp/src/spatial/` (private module, types exported at the root) | Stays in `tutti-spatial` |
|---|---|
| `vbap/panner.rs`: the gain solve, `solve_block`, `gains_now`, `BlockGains` (doc 024 row 44) | `VbapPannerNode`, `VbapPannerControls`, `speaker_of_channel`, `render_channel` (loops over `Io`) |
| `hrtf/panner.rs`: `HrtfBinaural`, the partitioned convolution with de-zipper, `ring_out`; the public `HrtfBinauralError`, `MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`, `MAX_PREPARED_RATE` | `HrtfBinauralNode`, `HrtfBinauralControls` |
| `hrtf/sphere.rs`: `HrirSphere::parse(&[u8])` (pure over bytes, no I/O), `HrtfSet::build`, `resample_into`/`resample_batched` (on `WindowedSinc`), `Face`, the barycentric lookup | `layout.rs` (the SMPTE/WAV channel-order table: interchange knowledge), `fork.rs`, `SpatialTarget`, `hold_finite`, `mix.rs` (graph assembly; its `tutti-nodes` edge keeps `tutti-spatial` at layer 5), `VbapError` (see 4.2) |
| `smoothing.rs` and `target.rs`'s `AngleSmoother`: the angle variant over `tutti_dsp::Smoother` (doc 024 consolidation 2 left it in spatial) | |

Everything that moves is `pub(crate)` today, so no existing public path changes
for the items themselves. The three public items that the HRTF half owns
(`HrtfBinauralError` and the two rate constants) are defined in `tutti-dsp` and
re-exported from `tutti_spatial` unchanged. `tutti-spatial` stays at layer 5
with its `tutti-nodes` edge; `scripts/check-layout.sh` does not change; the
moved tests move with their code, unedited, and the libm golden digests
(`smoother_digest.rs`, `vbap_fold_digest.rs`) move or stay without edits and keep
their `TUTTI_GOLDEN` rules. Doc 024's sentence "*Spatial*: already a crate"
and rows 44 to 46 are updated in the PRs. The nodes leave only when doc 024
step 6's `DesignNode` replaces them, and **the question of dissolving the crate
is reconsidered then, not now** (the mix builder is graph composition that
`tutti-nodes` does not otherwise hold).

### 4.2 The `vbap` crate

`vbap = "0.1"` is the owner's own crate (`PoHsuanLai/vbap`, MIT, 1,473 lines:
`config.rs` 523, `panner.rs` 321, `math.rs` 179, `presets.rs` 188, `speaker.rs`
120, `error.rs` 43, `lib.rs` 99). Its solve is copied into `tutti-dsp` and
brought to tutti's shape, and the dependency is dropped. What the code does and
what changes:

| In the crate | What it is | In `tutti-dsp` |
|---|---|---|
| `SpeakerConfigBuilder`, `SpeakerConfig` | a builder of `(azimuth, elevation)` pairs that picks speaker **pairs** (2D, a 2x2 inverse each) or **triplets** (3D: candidates by `volume / side-sum`, pruning of crossing edges by shortest first via `lines_intersect`, an interior-speaker rejection, a 3x3 inverse), at build | a `SpeakerLayout` value built on the control side from the topology's speaker positions; the tuple table is a `Box<[Tuple]>` with `[u16; 3]` indices and a length, not a `Vec<usize>` per tuple; `build` returns a typed `SpeakerGeometryError` |
| `VBAPanner::compute_gains_into` | for each tuple, the candidate gains `M^-1 d`; the tuple with the largest minimum gain wins; the gains are clamped at 0 and normalised to `sum g^2 = 1` | `solve(&self, direction, &mut [f32])`: the same selection and normalisation, no allocation, narrowing from `f64` once at the edge; `compute_gains` (which allocates) is not copied |
| `math::spherical_to_cartesian`, `cartesian_to_spherical`, `lines_intersect` | the trigonometry in `DVec3`, using `libm::sincos` | the same maths on a private `[f64; 3]` with `dot`, `cross`, `angle_between`, `normalize_or_zero` and a 3x3 adjugate inverse, about 60 lines; `libm` is already a `tutti-dsp` dependency |
| `presets` | stereo, stereo wide, LCR, quad, 5.1, 7.1, 7.1.4, 5.1.4, hexagon, octagon | only the five tutti uses (stereo, quad, 5.1, 7.1, 7.1.4); the rest, if wanted, are data from `ChannelTopology` |
| `Speaker::with_distance` | a distance, unused by the solve | dropped |

**`glam` is not needed.** The crate uses it for `DVec2`/`DVec3`, `DMat2`/`DMat3`,
`cross`, `dot`, `angle_between`, `normalize_or_zero`, `determinant`, `inverse`
and `mat * vec`: a few dozen lines of plain `f64` arrays. `tutti-dsp` stays at
`tutti-types`, `rustfft`, `libm` and `thiserror`, which retires the spatial
review's recommendation to accept `glam` into `tutti-dsp`'s tree (its option (i))
in favour of its option (iii), done during the move instead of after it.

**Unit types.** Controls arrive per call as `Azimuth`, `Elevation`, `Spread` and
`StereoWidth`; the solve converts degrees to radians once and works in `f64`
(the crate's own comment, "the types stop at the trigonometry", is the stated
reason a bare float is used inside the kernel, per CLAUDE.md "Where the types
stop"). A speaker's position is `(Azimuth, Elevation)`.

**What this changes that the review did not list.** `VbapError::Vbap` wraps
`vbap::VBAPError` and is public (`tutti-session` wraps `VbapError` in its
`Error::Spatial`). Dropping the dependency replaces that variant with
`tutti-dsp`'s `SpeakerGeometryError` (`InsufficientSpeakers`,
`InvalidConfiguration`), a public-path change in `tutti-spatial`, `tutti-session`
and `tutti`, made in the same PR.

**Provenance, and the tests.** The crate says its tuple selection is "based on
Ardour's `choose_speaker_pairs()`" and `choose_speaker_triplets()` in
`vbap_speakers.cc`, and Ardour's file derives from Ville Pulkki's 1998 VBAP
library; Debian's copyright file for Ardour lists that file as
LGPL-2.1-or-later (the Debian testing file also contains a separate permissive
"vbap" notice whose scope could not be read; unverified at the source file).
`vbap` on crates.io is MIT. Copying the selection into a `MIT OR Apache-2.0`
workspace carries that provenance with it, and this is the owner's call
(§9 Q9). The tests do not depend on it: the spec is the oracle (CLAUDE.md), and
every VBAP property is a figure from Pulkki 1997 (sum of squares 1, non-negative
gains, at most three non-zero, positive gains inside the active triangle, gains
continuous across a triangle edge, a source at a speaker's position puts all
energy there), so the selection can be rewritten and held to the same cases
without comparing to the crate's output.

### 4.3 The PRs

1. **Smoother and VBAP.** `AngleSmoother` and the solve into `spatial/`; the
   `vbap` dependency and `VbapError::Vbap` replaced; `glam` leaves the lock.
   Proof: the moved tests and digests unedited; the new analytic cases above;
   `cargo tree -p tutti-dsp` shows no `glam`.
2. **HRTF set, convolver and sphere build.** `spatial/hrtf.rs`, `sphere.rs`;
   `HrtfBinauralError` re-exported unchanged. Proof: `hrtf_render.rs` and the
   contract suite unedited; the `RealFft` scalar-planner determinism note applies.
3. **Cleanup.** Trim or drop `tutti-spatial`'s `hrtf` feature (it then gates only
   the node), update CLAUDE.md's `tutti-dsp` paragraph, doc 024 rows 44 to 46 and
   the "already a crate" sentence, and the stale comment in `tutti-nodes`'
   `Cargo.toml` about the convolution. Effort: M in all, as #448 and #451 were.

After step 3 the HRIR file format question (§9 Q10) is the only spatial item
left open before ambisonics.

## 5. The ranked roadmap

Table stakes first, then differentiators. "Class" is doc 024's; "Kind" says
whether the item is a new `Design`, a new realization of an existing model, or a
node only. "Waits on" names the doc 024, 022 or 026 step that hosts it or what
it needs. Effort is S, M or L (§3).

| # | Item | Class | Kind | Lives in | Waits on | Pinned to | Effort |
|---|---|---|---|---|---|---|---|
| 1 | Oversampler (polyphase FIR and half-band IIR, 2/4/8x, declared latency) | 6 with `Lti` prototype | new model (`Interpolator`) and realizations | `tutti-dsp/resample/` | doc 024 step 5 (kernels first) | stopband, passband, ratio, latency, overshoot (§3.2) | M |
| 2 | ADAA waveshapers, 1st then 2nd order | 4 over a class 3 map | new realization of the waveshaper's model, `Antiderivative` | `tutti-dsp/waveshape.rs` | step 5 | derivative, DC, alias bin (§3.1) | S |
| 3 | True-peak limiter and streaming true-peak meter | 4 (`GainControl`) | new realization; new detector | `tutti-dsp/dynamics/`, plan op | step 4, items 1 | ISP and worst-case guarantee (§3.3) | S to M |
| 4 | Utility pack and pan laws | 1 order 0, 5 | node only, small designs | `tutti-dsp`, `tutti-nodes` | consolidation 10; step 7 for pan | matrices, sidebands, DC zero (§3.12, §3.5) | S |
| 5 | EQ family: slopes, tilt, multi-band node; dynamic; linear-phase | 1 (`SeriesN`), `Scheduled`, `LinearPhase` | new designs, two new traits | `tutti-dsp/filter/`, `tutti-nodes` | steps 2 to 4 | Butterworth magnitude, scheduled response, phase (§3.4) | M each |
| 6 | De-esser, transient shaper | 4 | new designs | `tutti-dsp/dynamics/` | step 4 | `steady_state`, time constants | S |
| 7 | Noise-shaped dither; loudness-normalise node | 5 with 1 | new combinator; node | `tutti-dsp/dither.rs`, `tutti-export` | step 7 (dither) | NTF psd, Gerzon-Craven (§3.6) | S |
| 8 | Stretch and pitch lock; warp markers | 6 | doc 026's | `tutti-dsp`, `tutti-sampler` | the owner's listening gate | doc 026 §7 | L |
| 9 | Formant-preserving pitch shift; then pitch correction | 6 with `PitchShift` | new realization | `tutti-dsp` | doc 026 stage 8; §9 Q1 | formant shift within `fs/order` (§3.9) | M to L |
| 10 | Reverb: Dattorro, early reflections, velvet decorrelation, FDN matrix queries | 1 | doc 022's, plus designs | `tutti-dsp/network/` | doc 022 steps 3, 3b | doc 022 §8 | M |
| 11 | STFT framework; spectral gate, freeze, blur | per kernel | new structure, kernels | `tutti-dsp/stft.rs` | doc 026 stage 2 | `pr_error`, identity, block independence (§3.8) | M |
| 12 | Wavetable (mipmapped) and FM/PM | 5 (`BandLimited`) | new designs | `tutti-dsp`, `tutti-polysynth` | step 7 | no alias harmonics, Bessel amplitudes (§3.7) | M each |
| 13 | Nonlinear virtual analog: nonlinear TPT ladder, diode clipper | `Lure` (new) | new class, new realizations | `tutti-dsp/filter/` | 1, 2; step 2 | small-signal response, critical gain (§3.10) | M |
| 14 | Ambisonics: bus, encode, rotate, AllRAD, binaural, SOFA-derived sets | 1 order 0 (`SphericalField`) | new models | `tutti-dsp/spatial/`, nodes | §4; doc 023 G11 | degree energy, isometry, commutation (§3.5) | L |
| 15 | Neural amp and pedal inference | inherent | own core | a new crate, later | after 1 to 13; §9 Q4 | layer equations on hand-built nets (§3.13) | L |

This follows the research ranking. One change: the pan-law row (the engine
research's item 6) joins the utility pack (item 4), because a law is an order-0
`GainMatrix` and the contract is small; and sampler interpolation tiers and the
async SRC (the engine research's item 7) are not an item of their own, they are
`Interpolator` (§3.2) with a kernel table, scheduled with item 1 and doc 026 Q5.

**Tracked, not scheduled here:** MSEG and multi-stage looping envelopes
(`tutti-modulation` curves; a class 5 `Piecewise`), chord and key beyond triads,
downbeat and tempo-change tracking, click and crackle repair, a `Separator`
seam for offline stem separation with no bundled model. Each is a `tutti-measure`
or modulation item with no model work.

## 6. The staged plan

Stages are PRs that pass `cargo xtask ci`. Kernels that need no new trait ship
early as plain `tutti-dsp` code with analytic tests and adopt the class traits
in the doc 024 step that hosts them, as the smoother did (consolidation 2). No
stage builds a parallel trait set to avoid doc 024's wait. Doc 024 steps 1 to 7
are not on `main` yet (no `model` module); the stages say what they wait on.

| Stage | Content | Proves it | After |
|---|---|---|---|
| A1 | Spatial PR 1: `AngleSmoother` and the VBAP solve into `tutti-dsp::spatial`; `vbap` and `glam` dropped (§4) | moved tests and digests unedited; `glam` gone from the lock; Pulkki's VBAP properties as new cases | doc 024 0b consolidation 2 (landed, #451) |
| A2 | Spatial PR 2 (HRTF) and PR 3 (cleanup) | `hrtf_render.rs`, contract suite; paths stable | A1 |
| B1 | Utility pack and generalised pan laws as plain nodes and `balance.rs` laws (§3.12, §3.5) | matrices, sidebands, DC zero; pan-law power and amplitude figures | 0b consolidation 10 |
| B2 | Oversampling and ADAA kernels (plain code), `Antiderivative` (§3.1, §3.2) | derivative, DC, stopband, passband, overshoot cases | none (parallel with doc 024 steps 1 to 4) |
| B3 | Doc 024 step 5 adopts them: waveshaper `Memoryless`, `Graded` oversampling, `Adaa1`/`AdaaModel`, `Interpolator` | the aliasing case; digests | B2, doc 024 step 5 |
| B4 | True-peak limiter and streaming true-peak meter (§3.3) | ISP and worst-case cases | B3, doc 024 step 4 |
| C1 | EQ family: slopes and tilt on `SeriesN`; then `Scheduled` and dynamic EQ; then `LinearPhase` (§3.4) | Butterworth magnitude; scheduled response; linear phase | doc 024 steps 2, 3, 4 |
| C2 | De-esser and transient shaper; noise-shaped dither and the loudness-normalise node (§3.6) | `steady_state`, NTF psd, Gerzon-Craven | doc 024 steps 4 and 7 |
| D1 | Doc 026 stages 1 to 8 as written (the owner's listening gate first) | doc 026 §7 | the gate |
| D2 | `PitchShift`, the envelope estimator and the formant realization (§3.9); the live shifter if §9 Q1 says so | formant shift within `fs/order` | D1 stage 8 |
| D3 | The STFT framework and first kernels (§3.8) | `pr_error`, identity, block independence | D1 stage 2 |
| E1 | Reverb: doc 022 steps 3 and 3b, then velvet decorrelation and FDN queries (§3.11) | doc 022 §8 | doc 022 steps 1c, 2; doc 024 step 3 |
| E2 | Wavetable and FM (§3.7) | no alias harmonics; Bessel amplitudes | doc 024 step 7 |
| E3 | `Lure`, the nonlinear TPT ladder, a diode clipper (§3.10) | small-signal response, critical gain | B3, C1; `Lure` into doc 024 §4.2 |
| F1 | Ambisonics: the `Ambisonic` topology (doc 023 G11), then encode, rotation and decode models, then nodes, AllRAD, binaural (§3.5) | degree energy, isometry, commutation | A1, A2; G11 |
| F2 | Neural core (§9 Q4), only if chosen | layer equations on hand-built nets | after E3 |

Doc 024 §10 step alignment: B3 is step 5; B4 and C2's dynamics half are step 4;
C1 is steps 2 to 4; B1's pan laws and E2 are step 7; A2's convolver half is step
6; D1 is doc 026's own order and its stage 1 is doc 024 step 7's "stretch" item
written first, as doc 026 §9 says.

## 7. Non-goals

- **Real-time neural stem separation and denoise in the graph.** An offline job
  at most. The cost on CPU is high, the model weights are often non-commercial
  (the common high-quality checkpoints are CC-BY-NC-SA, unverified here beyond
  the research notes), a model is a 100 MB-class asset, and a pure-Rust runtime
  (candle, tract) is slow at that size. The one thing worth keeping is a
  `Separator` seam (a trait, offline, progress, no bundled model), cheap and
  independent of any model; it is tracked in §5, not scheduled.
- **Embedding GPL or closed libraries.** Rubber Band is GPL-2-or-later with a
  paid commercial licence, élastique is closed, the IEM and SPARTA plug-in
  suites are GPLv3 (HO-DirAC non-commercial). The workspace is
  `MIT OR Apache-2.0` and the owner's rule is pure-Rust codecs and formats. The
  mathematics (real spherical harmonics, rotation, AllRAD, max-rE, ambiX
  conventions) is published and is implemented here from the literature.
- **Wave digital filters, for now.** They are an alternative *authoring* method
  for the same circuits `Lure` models, with a high authoring cost per circuit and
  no query that the Lur'e view does not already give. Watch only.
- **Neural-vocoder stretching and polyphonic audio-to-MIDI** (neural), and Dolby
  Atmos rendering (a licensing matter, not DSP): out.

## 8. What the contract gains

For the owner's check against doc 024 §6: no new family needs a class group
that another family could not reuse. The cases in §3 reduce to six patterns the
contract already has, with the new traits adding the queries they read:

| Pattern | Used by | New query it reads |
|---|---|---|
| Response against the impulse's FFT (`Lti`) | EQ, linear phase, DC block, Hilbert, `Lure::small_signal`, `Interpolator` prototype | `LinearPhase::group_delay_frames` |
| Static curve and harmonics (`Memoryless`) | ADAA map, `phi` of a `Lure`, the shapers | `Antiderivative::f1`, `f2` |
| Steady state and time constants (`SteadyState`) | `AdaaModel`, `Oversampled`, the limiter, `Scheduled`'s detector | `Scheduled::at` |
| Generator spectrum (`Periodic`, `Noise`) | wavetable, PM, shaped dither | `BandLimited::top_harmonic`; `Filtered::psd`; `NoiseTransfer::log_integral` |
| Time-scale (`TimeScale`, `Fidelity`) | the oversampler, the interpolator, the shifter | `Reconstruction::overshoot_bound`; `PitchShift::formant_shift` |
| Algebraic identity of two models | rotation against encode, `StftPair::pr_error`, M/S round trip | `GainMatrix::spectral_norm`, `column_power` |

The last row is a form of case doc 024 does not name: **an identity between two
models' answers** (not between two renders), checked on the control side. It is
consistent with CLAUDE.md's rule against comparing a render to a reference
render, because both sides are closed-form queries of a model.

## 9. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Formant preservation: doc 026 deferred it. Add it, and as a live effect or only in the sampler? | Yes, as a realization of `PitchShift` (§3.9), true-envelope (Röbel and Rodet) in `tutti-dsp`, after doc 026's listening gate. In the sampler first (it is position-driven and doc 026's shape already fits); a live effect needs an input-fed push shape that doc 026 §12 rejected for lack of a consumer, and a live shifter is that consumer, so open it only when the effect is built. Do not change doc 026 beyond one sentence in its §4.3 |
| Q2 | Shimmer: doc 022 excluded it (a pitch shifter in the reverb loop). Reopen? | Not as a reverb `Design`. After Q1, a shimmer is a graph composition (reverb node, a pitch-shift node, a feedback wire the graph already supports by a delayed edge), with the loop gain below 1 its stability query. Keep 022's exclusion; add no `Shimmer` kind. Revisit when a live shifter exists |
| Q3 | Commit to ambisonics? | Yes, to first through third order, ACN/SN3D, with the `Ambisonic` topology (G11) first and AllRAD and binaural decode as the deliverable. Not above order 3 until a consumer asks. The order-0 matrix models (§3.5) cost little and the VBAP move (§4) is wanted regardless |
| Q4 | Neural inference: build an own minimal core, or none? | None now. Hosted plugins already cover NAM-class models. If it is built: an own pure-Rust core (LSTM, GRU, 1-D convolution, dense) in a new crate above `tutti-dsp`, weights loaded as data on the control side, run with no allocation on the audio thread, no `candle`/`ort` on the audio thread, and the contract of §3.13. Decide after items 1 to 13 |
| Q5 | Oversampler filters: FIR, IIR or both? | Both, behind `Quality`. Draft: a half-band all-pass IIR cascade (cheap, minimum latency, non-linear phase). Standard and above: linear-phase FIR polyphase (exact stopband and constant delay). Export always uses the FIR tier. The limiter's detector uses the FIR so `overshoot_bound` is an exact figure |
| Q6 | Where does the STFT framework live relative to doc 026's vocoder? | In `tutti-dsp` beside `geometry.rs` as `stft.rs` (the `StftPair`, the per-bin trait, the kernels); doc 026's `LockedVocoder` is written in `tutti-dsp` already and uses `StftGeometry`, so it takes the pair as a dependency when D3 lands, not before: do not delay doc 026 stage 2. A `spectral` submodule is optional tidying, and a separate crate is rejected (the spatial review reached the same view: it would cut `tutti-dsp` along a boundary every processor crosses) |
| Q7 | How does an IIR half-band oversampler declare latency, given doc 024 says an IIR has group delay and no latency? | Declare the rounded group delay at DC as `latency`, state it in the model, and have `Fidelity` include the phase deviation above DC. The alternative (declare zero and leave a few frames uncompensated) is what doc 026 §12 rejected for the stretcher |
| Q8 | Does `Lure` go into doc 024 §4.2? | Yes, at the PR that implements the first `Lure` (E3), as a seventh class (a Lur'e system), and row 2 of the inventory changes from "Partly" to "Fits". Kept here until then because nothing implements it |
| Q9 | VBAP tuple selection's provenance (§4.2) | Owner's decision. Recommendation: re-derive the pair and triplet selection from Pulkki's published criteria and hold it to the spec cases in §4.2, so no Ardour- or Pulkki-library-derived code enters the workspace; keep the maths of the solve (the 2x2 and 3x3 inverse, normalisation) as it is |
| Q10 | An HRIR file format: SOFA (AES69-2022, a netCDF/HDF5 container) or keep `hrir_sphere_builder`'s? | Keep the sphere format as the runtime form. SOFA is read by an offline conversion tool (SOFA is a file format; the AES69-2022 text is paid and the reference tooling is Python and Matlab; whether a maintained pure-Rust HDF5 reader exists is unverified, and the owner's rule asks before a C library). Revisit only if a consumer needs SOFA at run time |
| Q11 | Do the new traits go to doc 024 or stay here? | `Lure` to doc 024 (Q8). The others (`Antiderivative`, `Reconstruction`, `Scheduled`, `LinearPhase`, `GainMatrix`, `BandLimited`, `Filtered`, `NoiseTransfer`, `PitchShift`) are per-family traits that doc 024 §4.2 says become traits when a second family answers the query. Each lands in `tutti-dsp/src/model/` with the first PR that implements it, and this doc is updated then |

## 10. Corrections to the research notes, and what could not be verified

**Corrected against the code:**

- The effects note says a true-peak **meter** is missing. It is not:
  `tutti-measure/src/loudness.rs` reports `true_peak` (dBTP) through `ebur128`
  (4x below 96 kHz, 2x from 96 kHz to below 192 kHz, the sample peak at or above
  192 kHz), in the file result and the windowed streaming reading. What is
  missing is a true-peak **limiter** and a streaming true-peak **plan-op meter**
  (the graph's meter is a sample peak).
- The effects and research notes say the ladder has "tanh at the input only".
  The code is `u = tanh(x - k * s[3])`: the nonlinearity is on the feedback sum,
  inside the loop, the four stages are linear, and the feedback is taken from the
  stored state of the last stage, not a solved delay-free loop (doc 024's wording
  is also inexact).
- The summary note calls the HRIR sphere "SOFA-style". It is the format
  `hrir_sphere_builder` writes (`"HRIR"`, rate, length, vertices, indices); there
  is no SOFA reader.
- The spatial review says moving the algorithms changes no public path. Dropping
  `vbap` removes the public `VbapError::Vbap(vbap::VBAPError)` variant (§4.2).
- The spatial review recommends accepting `glam` into `tutti-dsp`. The `vbap`
  crate uses only elementary vector and 2x2/3x3 operations, so `glam` is not
  needed (§4.2).
- The `vbap` crate is derived from Ardour's speaker-selection code (§4.2); no
  research note mentions it.
- The research note cites "Bilbao et al." for stateful ADAA. The stateful
  extension is Holters (DAFx-19; *Applied Sciences* 10(1), 2020); Bilbao and
  colleagues' work is the memoryless follow-up in *IEEE Signal Processing
  Letters*. RTNeural has one author (Chowdhury), not two.

**Confirmed by the code:** the SVF is Simper's trapezoidal form; the limiter is
sample-peak only; the polysynth uses PolyBLEP (saw, pulse) and PolyBLAMP
(triangle); dither is rectangular and TPDF over a xorshift32, no shaping; the
strip pan is a linear balance and VBAP's normalisation is the only constant-power
law; the waveshaper has six kinds and no oversampling or antialiasing.

**Could not be verified** (cited in §11 as unverified, and not relied on beyond
motivation): the exact BS.1770-5 Annex 2 filter (48 taps; the 4x/8x under-read
figures of 0.55 to 0.69 dB and 0.17 dB are from secondary sources and the
standard's text was not read); every product claim about Pro-Q 4, Pro-L 2,
Ableton, Logic, Bitwig, Valhalla and iZotope; the licence of the model weights
for stem separation; the LGPL status of Ardour's `vbap_speakers.cc` at the source
file; whether a maintained pure-Rust SOFA/HDF5 reader exists; the Dattorro, Jot,
Välimäki velvet-noise, Schlecht and Habets, Yeh and wavetable-mipmap citations
(from memory; none re-checked here); and the Gerzon-Craven theorem's exact
statement for dithered quantisers (stated here for the monic minimum-phase NTF).

## 11. References

Checked against a source in this pass:

- J. D. Parker, V. Zavalishin, E. Le Bivic, "Reducing the aliasing of nonlinear
  waveshaping using continuous-time convolution", DAFx-16, Brno. The abstract
  confirms the continuous-time construction, the pairing with low-order
  oversampling and the extension to feedback systems; the full text was not read.
  <https://dafx.de/paper-archive/details/vem_XXF5qBbfiWOH2RVVAA>
- M. Holters, "Antiderivative antialiasing for stateful systems", DAFx-19;
  *Applied Sciences* 10(1):20, 2020. <https://dafx.de/paper-archive/details/lp5GcJOo79OBkfcMfN3N_g>
- S. Bilbao, F. Esqueda, J. D. Parker, V. Välimäki, "Antiderivative antialiasing
  for memoryless nonlinearities", *IEEE Signal Processing Letters* (year not
  confirmed; cited in J. Vicanek's note, <https://www.vicanek.de/articles/AADistortion.pdf>).
- A. Röbel, X. Rodet, "Efficient spectral envelope estimation and its
  application to pitch shifting and envelope preservation", DAFx-05, Madrid.
  <https://dafx.de/paper-archive/details/EDSTSaO17Gd_0LV03iwDfw>
- J. Laroche, M. Dolson, "Improved phase vocoder time-scale modification of
  audio", *IEEE Trans. Speech Audio Process.* 7(3), 323 to 332, 1999 (identity
  and scaled phase locking). <https://www.ee.columbia.edu/~dpwe/papers/LaroD99-pvoc.pdf>
- F. Zotter, M. Frank, "All-round ambisonic panning and decoding", *JAES* 60(10),
  807 to 820, 2012 (AllRAP and AllRAD). <https://aes2.org/publications/elibrary-page/?id=16554>
- P. Majdak et al., "Spatially oriented format for acoustics 2.1: introduction
  and recent advances", *JAES* 70(7/8), 565 to 584, 2022; AES69-2022 is SOFA 2.1.
  <https://www.aes.org/e-lib/browse.cfm?elib=21824>
- S. P. Lipshitz, R. A. Wannamaker, J. Vanderkooy, "Quantization and dither: a
  theoretical survey", *JAES* 40(5), 355 to 375, 1992. <https://www.aes.org/e-lib/browse.cfm?elib=7047>
- S. D'Angelo, V. Välimäki, "An improved virtual analog model of the Moog ladder
  filter", ICASSP 2013 (bibliographic record only).
- J. Chowdhury, "RTNeural: fast neural inferencing for real-time systems",
  arXiv:2106.03037. <https://arxiv.org/abs/2106.03037>
- Rubber Band Library: GPL-2-or-later, with a separate commercial licence.
  <https://breakfastquay.com/rubberband/license.html>
- IEM Plug-in Suite: GPLv3 (Arch and Debian package metadata).
  <https://www.archlinux.org/packages/extra/iempluginsuite>
- Debian copyright file for Ardour (`vbap_speakers.cc`, Pulkki 1998, listed
  LGPL-2.1-or-later; the permissive notice's scope unread).
  <https://metadata.ftp-master.debian.org/changelogs/main/a/ardour/stable_copyright>
- ITU-R BS.1770-5 Annex 2 (true peak: 4x interpolation); the filter length and
  error figures are from secondary sources (Slate Digital, an AES convention
  paper by I. Dash on its error sources). **The standard was not read: unverified.**

Cited from memory, **unverified in this pass**:

- V. Pulkki, "Virtual sound source positioning using vector base amplitude
  panning", *JAES* 45(6), 456 to 466, 1997 (cited by the `vbap` crate's own
  documentation, which is where the citation was read).
- J. Dattorro, "Effect design, part 1: reverberator and other filters", *JAES*
  45(9), 1997; J.-M. Jot, A. Chaigne, 1991; V. Välimäki et al., velvet-noise
  decorrelators (2012); S. Schlecht, E. Habets, FDN feedback matrices; D. Yeh,
  circuit-modelling theses and DAFx papers; V. Zavalishin, *The Art of VA Filter
  Design*; A. Simper's SVF notes; M. Nachbar et al., "ambiX" (ambisonics format
  proposal, 2011); wavetable mipmapping practice.
- Product feature claims (Pro-Q 4, Pro-L 2, Live, Logic, Bitwig, Valhalla,
  iZotope, Soundtoys, NAM and AIDA-X licences) from the research notes.
