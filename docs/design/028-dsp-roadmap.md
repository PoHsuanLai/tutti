# 028: the DSP roadmap, by family

[Doc 024](024-design-model-realization.md) sorts tutti's algorithms by the
mathematical class they belong to (an LTI system, a static map, a generator, a
time-scale map) and gives each class a trait whose queries are answered from the
model's structure. This document asks what the engine lacks and answers it in
the same terms: for each family it names the mathematical object, the doc 024
class that states it (or the small addition that is needed where none does), the
queries the object answers, the contract cases those queries make possible, and
where the code lives. The ranking and the staging come after that, because the
order in which families are built follows from what each needs from the traits.

Status: design only; nothing here is implemented. Written against `main` at
`f2647ae8b`: doc 024 step 1 has landed (#455: `Kernel` is `Realization`,
`ReverbNode<D>` is `DesignNode<D>`; #463: the `model` module with `System`, `Lti`,
`Design::Model`, `Realization<M>`, `Effect<M>`, and the `contract` feature) and so
have consolidations 1, 2, 3, 9 and 10 (#448, #451, #446, #443, #444). The class
traits step 1 did not add (`Memoryless`, `SteadyState`, `Periodic`, `Noise`, the
combinators) are quoted from doc 024 §4.2, not from code. It does not
contradict [doc 022](022-reverb.md) (the reverb items stay its steps, and its
exclusion of shimmer stands, §9 D3) or [doc 026](026-stretch.md) (the listening
gate is still the owner's; formant preservation is a new realization beside
026's, §3.9). References to "§9" mean this document's; its questions were taken on the recommendation on 2026-11-11 and are recorded as decisions. Claims about tutti were
checked against that code; claims about the outside world are in §11, each marked
verified or not.

## Summary

- **Where tutti stands (§1).** The linear fundamentals exist: a Simper
  trapezoidal SVF, a ladder, Linkwitz-Riley crossovers, compressor, multiband,
  gate, a lookahead limiter, chorus/flanger/phaser, delay, Freeverb, an FDN, a
  partitioned convolver, R128 loudness with true-peak measurement,
  PolyBLEP/PolyBLAMP oscillators, VBAP and HRTF. The gaps are nonlinear fidelity
  (the waveshaper aliases and nothing oversamples), mastering safety (the limiter
  is sample-peak), a utility layer (polarity, mid/side, DC block, tremolo), EQ
  depth, noise-shaped dither, spectral effects, pitch and formant processing,
  wavetable and FM synthesis, and ambisonics.
- **Most families need no new trait (§2, §3).** An EQ is `SeriesN<SvfModel, N>`,
  a pan law and a rotation are order-0 `Lti`, a limiter is a `SteadyState`
  model with an inherent detector, an oversampled shaper is a class 6 pair around
  a class 3 map. What is added is small: one trait kept because more than one
  family answers it (`BandLimited`; the planned `GainMatrix` is struck, below); four combinators
  justified by composition laws (`Controlled` for a dynamic EQ band, which doc 024 §4.5a
  generalises to any trajectory, `Filtered`
  for a noise process through an `Lti`, `Oversampled` for up, map, down, and
  `Lure<L, Phi>` for a nonlinearity inside a feedback loop); three amendments
  proposed to doc 024 §4.2 (`onset`, the measured first non-zero frame, added
  beside `latency`, which stays the alignment delay PDC reads; the MIMO norms with
  `h2_norm`; and `Lure`, which lifts doc 024 Q17); and structures
  (`PolyphaseFir`, `HalfbandIir`, `StftPair`). Single-family queries
  (antiderivatives, the overshoot bound, the formant shift) are inherent methods,
  as doc 024 §4.2 says until a second family answers them.
- **True peak is a measured detector with a stated margin (§3.3).** The peak on
  the N-times grid divided by `cos(pi/2N)` bounds the true peak of a signal
  band-limited to the base Nyquist (0.69 dB at 4x, 0.17 dB at 8x); the output
  guarantee is stated with the condition on the gain's variation over the
  reconstruction support. The windowed minimum stays inherent (doc 024 step 4).
- **Spatial moves into `tutti-dsp` first (§4).** The VBAP solve, the HRTF
  convolver and the HRIR sphere build go into a private `spatial` module of
  `tutti-dsp`; the angle smoother and the nodes stay in `tutti-spatial` (doc 024
  consolidation 2). The owner's `vbap` crate is copied in and brought to tutti's
  shape (no `glam`, no allocation after build), and the dependency is dropped.
  AllRAD and binaural ambisonic decode need this move; encode, rotation and a
  plain decoder do not.
- **Order (§5, §6).** Table stakes first: oversampling and ADAA, the true-peak
  limiter, the utility pack and pan laws, the EQ family, noise-shaped dither;
  then stretch (doc 026), formant and pitch, reverb (doc 022), the STFT framework,
  wavetable and FM, nonlinear virtual analog; then ambisonics. Neural inference is
  watched, not built.
- **Non-goals (§7).** Real-time neural stem separation, embedding GPL or closed
  libraries, wave digital filters for now.
- **Decisions (§9).** Recorded decisions with their grounds, including those that
  were open questions, taken on the recommendation on 2026-11-11; none are open.

## 1. Where tutti stands

### 1.1 What is there

| Area | What the code has | Where |
|---|---|---|
| Filters | Simper's trapezoidal SVF, eight responses (`SvfType`: low/high/band-pass, notch, all-pass, bell, low and high shelf); a four-stage ladder, `u = tanh(x - k * s[3])` with the feedback taken from the last stage's stored state and four linear stages; LR crossovers; one SVF band per `EqBandNode` | `tutti-dsp/src/filter/` |
| Dynamics | compressor with external sidechain, multiband, gate, a lookahead limiter (`LimiterKernel` over a monotonic-minimum window; the detector `fold_peak` is the max-abs of the frame) and a brickwall clamp, envelope followers | `tutti-dsp/src/dynamics/`, `tutti-nodes/src/dynamics/` |
| Modulation, delay | chorus and flanger (one swept delay), phaser (swept all-pass chain), interpolated delay line | `swept_delay.rs`, `allpass.rs`, `delay.rs` |
| Reverb | Freeverb, an FDN, a partitioned convolver; the doc 022 `Network` | `reverb.rs`, `fdn.rs`, `convolver.rs`, `network/` |
| Nonlinear | `Shaper`: `Tanh`, `Atan`, `Softsign`, `HardClip`, `Crush`, `SoftCrush`, each one line, no oversampling and no antialiasing | `waveshape.rs` |
| Dither | `DitherNoise`, a xorshift32; `Rectangular` (`[-0.5, 0.5]` LSB) and `Triangular` (`[-1, 1]` LSB); no shaping | `dither.rs` |
| Pan | the strip's `balance_gains`: `left = min(1, 1 - p)`, `right = min(1, 1 + p)`, a linear balance that boosts nothing; the polysynth voice has its own constant-power law, `sqrt((1 -+ p)/2)` (`tutti-polysynth/src/synth_voice.rs`); the graph's `CrossfadeCurve::EqualPower` is `sin`/`cos` (`tutti-graph/src/fade.rs`); VBAP normalises to unit power | several |
| Measurement | EBU R128 integrated, range, momentary, short-term and **true peak** (through the `ebur128` crate, which oversamples 4x below 96 kHz and 2x from 96 kHz to below 192 kHz, and reads the sample peak above); YIN, onsets, beats, key, chords, transcription, spectra | `tutti-measure` |
| Synthesis | `f32x8` oscillator bank: sine, saw and pulse with PolyBLEP, triangle with PolyBLAMP, white and pink noise, `tanh`; unison, portamento, MPE; no wavetable, FM or granular | `tutti-polysynth/src/kernel.rs` |
| Resampling, stretch | FFT resampler (adapted from rubato), a windowed-sinc kernel, cubic Hermite for the sampler read; a phase-vocoder stretch that doc 026 replaces | `resample/`, `interp.rs`, `tutti-sampler` |
| Spatial | VBAP over the `vbap` crate; HRTF binaural by partitioned FFT convolution over an HRIR sphere in the `hrir_sphere_builder` format (not SOFA); `ChannelTopology` records that B-format is not speaker positions, and doc 023 G11 plans only the topology tag | `tutti-spatial`, `tutti-types/src/topology.rs` |
| Export | measure-then-apply loudness and peak normalisation (`Normalize`, `render_normalized_to_file`) | `tutti-export/src/normalize.rs` |

### 1.2 The gaps

Verified absent by search of `tutti-dsp`, `tutti-nodes`, `tutti-spatial`,
`tutti-sampler`, `tutti-polysynth`, `tutti-measure` and `tutti-types`:
oversampling and antiderivative antialiasing; a true-peak limiter (the limiter's
detector is a sample peak, and the graph's level meter, `block_stats`, is also a
sample peak); polarity, mid/side, width, DC block, tremolo and auto-pan, ring
modulation, frequency shifter and Hilbert pair; higher-order EQ slopes, tilt,
dynamic and linear-phase EQ; noise-shaped dither; spectral effects; pitch
correction and formant processing; warp markers; wavetable, FM and granular
synthesis; spherical-harmonic code; stem separation.

What other products ship is not the justification for any item below; each is
justified by what it does for the engine.

## 2. How a family is placed

Doc 024's rule is used throughout: a **design** is a pure function from typed
controls to a **model**; a model is a system up to equivalence, held by a class
trait whose queries are answered from the model's structure and never by
expanding it to a canonical form (doc 024 §2.1, §2.4 item 7); a **realization**
is one hand-written structure that computes the map, held to the model by the
contract. A class trait is added only when a second family answers the same
query, and a query only one family answers stays an inherent method of its model
(doc 024 §4.2). Three tests decide where a family lands:

1. **Does an existing class trait state its mathematics?** If yes, implement it
   and write nothing new (the EQ, the pan laws, the utility pack).
2. **If not, is the missing piece a query, a structure or a class?** A query that
   one family answers is an inherent method. A query that several answer is a
   trait. A structure (the STFT pair, a polyphase prototype) is a model type. A
   feedback loop around a nonlinearity is a combinator (§3.10).
3. **If it forces a shape, leave it inherent** and say so (§3.13).

**Wet/dry is the graph's.** No family here proposes a node-level dry/wet. Doc
013's M4 makes the per-node mix a graph primitive (`Editor::set_mix`), and
`Blend`/`DryWet` is being retired, so new designs use `NoBlend` and the mix is
applied by the graph, aligned by the node's declared `latency`.

### 2.1 Amendments proposed to doc 024

Three amendments to doc 024's §4.2 are proposed here, each marked **(proposal)** where used.
Doc 024 itself was amended on 2026-11-11 (its §4.2, §4.5, §4.5a and §4.10, from the IAMF stress
test), and four items of this document change with it: `GainMatrix` is struck (§3.5: `Fold` with
a basis and a metric, doc 024 §4.2); `Controlled` is generalised to a source of controls with a
law (§3.4; doc 024 §4.5a); doc 026's `Warp` is a monotone `Trajectory<f64>` and no separate trait
(§5); and `Fidelity` for `StftEffect` (§3.8) is the aliasing norm of the lifted periodic system
(doc 024 §4.2, class 6). P1 and P2 are
additive and land in the PR of their first consumer (§9 D10); P3 reverses a
recorded recommendation of doc 024 (§9 D10).

**P1. The measured onset, added beside the alignment delay.** `System::latency`
stays what the graph reads: the one number PDC aligns by, that a node's `Shape`
declares, and that `latency_plan`, plugin latency and CLAUDE.md ("report only
processing latency to PDC") already mean. Doc 024 defines it as the processing
part of the onset of the impulse response, and its Latency contract case measures
the first non-zero output against it. Several processors have an alignment delay
that is not the first non-zero frame: a linear-phase FIR (non-zero pre-ringing
taps before the centre, group delay `(L-1)/2`), an IIR half-band (`h[0] != 0`, a
frequency-dependent group delay), an STFT effect (the delay that makes the render
independent of the block size is not when the first output is ready), and a
reverb (whose first non-zero frame includes its musical pre-delay). So the new
query is the measured one:

```rust
pub trait System {
    fn latency(&self) -> Samples;       // the alignment delay: what PDC reads and `Shape` declares (unchanged)
    fn tail(&self, floor: Db) -> Ring;  // counted from `latency`, not from `onset` (doc 013, "A tail excludes latency")
    /// (proposal) The first frame at which the response to a unit impulse is non-zero, measured.
    /// Defaults to `latency`, which is right for a lookahead or a partition (a delayed copy).
    /// Musical delay (a pre-delay, a tap spacing) is in it and never in `latency`.
    fn onset(&self) -> Samples { self.latency() }
}
```

Doc 024's Latency case now measures the first non-zero output against `onset()`;
a new case checks `latency()` as the delay of a band-limited tone burst below
`0.05 fs` (from the envelope's centre of mass, within half a frame). Consequences,
each part of the proposal:

1. **The graph sees one number.** The node adapter puts `latency()` into `Shape`;
   `onset` never reaches `tutti-graph`, the plan, export or doc 013's M4 mix.
2. **The tail is counted from `latency`.** For a linear-phase FIR the tail is the
   remaining `(L-1)/2` taps after the centre, not `L-1`, and
   `contract::assert_honest_tail` must hold with it (§8). Musical delay still
   counts toward the tail, as doc 024 says of a pre-delay.
3. **Pre-ringing before `latency` is aligned away by design.** PDC and export trim
   the first `latency` frames, so a linear-phase EQ's pre-ringing falls before frame
   0 of the aligned output, and M4's graph mix aligns the dry by the same number.
4. **Whole frames, declared at shape time.** A linear-phase FIR design refuses an
   even `L` at construction (a constructor refusal, not only a mutant), so
   `(L-1)/2` is a whole number. An IIR half-band declares its group delay at DC
   rounded to the nearest frame, ties up; its residual phase deviation above DC is
   a stated phase-error query beside `Fidelity` (which is a magnitude). An STFT
   effect declares `size - 1` (§3.8). A quality tier that moves `latency` is a new
   `Shape` (a recompile through the graph's crossfade), never a per-block report.
5. **The reverb.** Its onset includes the pre-delay (musical delay), so for the
   reverb `onset() = latency() + min_delay()`, where `Network::min_delay` already
   adds the pre-delay and each stage's shortest path; its `latency()` is today's
   (the largest FIR stage latency, zero for the time-domain stages) and does not
   change. This agrees with doc 024's text (`onset(h) = latency + musical delay`)
   and with the `Network` implementation of `System` on `main`. When P1 lands the
   code changes in three places: `System` gains `onset` with its default; `Network`
   overrides it; and doc 024's Latency case in the `contract` machinery measures
   `onset()`, with the tone-burst case added for `latency()`.

A stage with real delay must declare it: doc 026's zero latency for the stretcher
(D8 there) is a choice made because the position-driven voice reads ahead, not a
precedent for declaring zero while delaying.

**P2. Norms for several inputs and outputs, and the white-noise gain.** For an
order-0 system `hinf_norm` (the peak of the response over frequency) is the
largest singular value of the matrix, so a gain-matrix trait needs no separate
spectral norm: define the MIMO `hinf_norm` as the largest singular value of the
response matrix at the worst frequency, and the MIMO `l1_norm` as the induced
infinity-norm of the impulse-response matrix (the largest row sum of absolute
values). Add `h2_norm`, the square root of the energy of the impulse response
(the power gain of unit white noise), provided by quadrature of `|H|^2` on the
control side to a stated tolerance and overridden exactly by an FIR (the sum of
squares). `h2_norm` is exact (or toleranced), the one norm outside doc 024 Q18's
upper-bound convention, because `Filtered::variance` must be an equality that the
contract tests against the measured variance. `l1_norm` squared is an upper bound
on it, not the variance (§3.6). The MIMO definitions also change doc 024 §6.2's
case `hinf_norm <= l1_norm` and the trait default `hinf = l1`, which hold for one
input and one output only: for several they become
`hinf_norm <= sqrt(outputs) * l1_norm` (the 2-norm of a matrix is at most
`sqrt(rows)` times its infinity-norm), and a mono-to-stereo pan law
`[cos, sin]` has `hinf = 1` against `l1 = 0.707` at the centre. An order-0 `Fold` model
overrides the default.

**P3. `Lure<L, Phi>`, a feedback combinator for one narrow case** (§3.10). Doc 024
§4.9 and Q17 decline a general `Feedback<A, B>` because closing a loop needs a
stability argument that depends on the loop. This adds one loop shape, a stable
`Lti` closed around a memoryless map in a sector, whose stability argument (the
circle criterion) is stated once. It lifts Q17 for that case and no other.

### 2.2 The new vocabulary, in one table

| Item | Kind | Class | Answers | Section |
|---|---|---|---|---|
| `onset`, `h2_norm`, MIMO norms | amendments to `System`, `Lti` | 1, all | measured first frame; white-noise gain; matrix norms | 2.1 |
| `ShapeKind::f1`, `f2`; `Adaa1`, `Adaa2` | inherent methods; realizations | 3, 4 | antiderivatives; the box-averaged map | 3.1 |
| `PolyphaseFir`, `HalfbandIir`, `Decimator`, `Oversampled` | models | 6 (`TimeScale`, `Fidelity`) | ratio, reach, stopband; `overshoot_bound` on the FIR only | 3.2 |
| `Controlled<S, D>` | combinator (doc 024 §4.5a) | 2 / 4 | the model at a source's `theta(t)`; at a settled detector level, the `Lti` system | 3.4 |
| ~~`GainMatrix`~~ | **struck 2026-11-11**: `Fold` with a basis and a metric, doc 024 §4.2 and step 7a | 1 order 0 | matrix, column power, isometry test relative to the metric | 3.5 |
| `ShEncode`, `ShRotation`, `ShDecode` | models | 1 order 0 | degree energy (on `ShEncode`); isometry; commutation | 3.5 |
| `Filtered<N, L>` | combinator | 5 (`Noise`) with 1 | shaped psd; variance from `h2_norm` | 3.6 |
| `BandLimited` | trait | 5 | top harmonic at a floor; aliasing harmonics | 3.7 |
| `StftPair`, `StftEffect` | structure, model | 1 up to `Fidelity` | window-sum constant; `latency` | 3.8 |
| `Lure<L, Phi>` | combinator | new, narrow | sector, circle limit, critical gain, small-signal system | 3.10 |

**Why a combinator with one user is still right.** Doc 024's own combinators
(`Series`, `Parallel`, `Wiener`, `GainControl`) are justified by composition laws,
not by how many families use them: a combinator states how a query of the whole
follows from the queries of its parts, once. The same holds here. `Filtered`: the
psd is `psd_N |H|^2` and the white-noise variance is `variance_N h2_norm(H)^2`
(users: noise-shaped dither today, any coloured noise source after). `Oversampled`:
the delay is the sum of the stages' and the peak is `l1(down) sup|f|` over the up
stage's overshoot (users: the oversampled saturator and clipper, the limiter's
detector path, and `Lure` realizations, which need oversampling). `Lure`: the
circle criterion, critical gain and small-signal system follow from `L` and `phi`
(users: the ladder and the diode clipper, both on the roadmap, item 13).
`Controlled`: the response at a settled level follows from the detector's steady
state and the design map (user: dynamic EQ; an envelope-controlled filter is a
possible second, not scheduled). Where no composition law exists, the item is a
plain parameter: `StftEffect` takes its per-bin gains as a plain slice, not a
`BinGain` trait, until a second implementer exists.

### 2.3 Types of the sketches

CLAUDE.md's unit-type and range-checked rules apply to every public type the
sketches imply, and the sketches follow them: no public fields; `Order` (0 to 7)
and an oversampling factor (2, 4, 8, 16) are range-checked newtypes with
`new -> Option`, `get`, `TryFrom` and serde through it, and are checked by
`check_range_contract`; the ADAA order is a two-variant enum; angles are
`Azimuth` and `Elevation`; frame counts and delays are `Samples`, rates `Hz`,
levels `Db`, times `Seconds`; a delay that is not a whole frame is a `group_delay`
query, not a `Samples`. A harmonic floor, a window-sum error and a per-bin gain
are `Amplitude` or plain ratios where an existing unit covers them; the sketches
use bare floats for these, and the implementing PRs type them or state why not. Where the types stop: a kernel narrows once at its edge,
and the antiderivatives take `f64` (not `f32`) because the quotient of two nearby
`F1` values cancels, a stated precision the unit cannot carry.

## 3. The families

Each subsection gives the mathematics, the trait or structure (Rust, as
type-checked, §10), the queries, the contract cases with a mutant each (a mutant
must fail its case, doc 024 §6), where it lives, which doc 024, 022 or 026 step
hosts it, and effort (S days, M about a week, L more). No case compares one render
against another render (CLAUDE.md); each figure is analytic or from a spec, or
the section says it cannot be covered here. Everything is in `tutti-dsp` (layer
2) unless stated, and a node file holds only the adapter.

### 3.1 Antialiased nonlinearity

**Mathematics.** A static map `f` applied to samples aliases because `f(x(t))`
has harmonics past Nyquist. First-order antiderivative antialiasing (Parker,
Zavalishin and Le Bivic, DAFx-16) treats the samples as the piecewise-linear
signal `x_lin(t)`, applies `f`, averages over one sample interval, and samples:

```
y[n] = (1/(x[n] - x[n-1])) * integral of f over [x[n-1], x[n]]
     = ( F1(x[n]) - F1(x[n-1]) ) / ( x[n] - x[n-1] )          F1' = f
```

When `|x[n] - x[n-1]|` is below a threshold the quotient is ill-conditioned and
the realization returns `f` at the midpoint (the limit; the difference is
`O(f'' dx^2 / 24)`). Second order uses `F2` (`F2' = F1`) and a box of two samples,
with fallbacks for `x[n] ~ x[n-1]`, for `x[n-1] ~ x[n-2]` and for both. Holters's
stateful extension (a nonlinearity inside a filter) is not this section; it
belongs to a `Lure` realization (§3.10).

**Not transparent in the linear region.** For `f(x) = x`, `F1 = x^2/2` and the
quotient is `(x[n] + x[n-1])/2`: `H(z) = (1 + z^-1)/2`, `|H| = cos(pi f/fs)`,
which is -3 dB at `fs/4` and -16 dB at `0.45 fs`; second order is `cos^2`. The
droop and the half-sample group delay are part of the model, stated as a
`Fidelity` term and a `group_delay` (not `latency`, a whole-frame count), and are
why ADAA is paired with oversampling (§3.2).

**Structure.** No trait: the antiderivatives are inherent methods of the
shaper's smooth kinds, `ShapeKind::f1(x, drive)` and `f2`, which return `None` for
a kind that gets no ADAA (`Crush` and `SoftCrush`, excluded by choice: a
staircase has a closed-form `F1`, but the design keeps them to oversampling). A
second implementer would be a `Lure`'s nonlinearity or Holters's stateful ADAA;
until one exists a trait is premature. The realizations take a `ShapeKind` and a
drive:

```rust
pub struct Adaa1 { kind: ShapeKind, drive: f64, x1: f64, f1_prev: f64 }   // sketched, with the tests below
impl Adaa1 { pub fn step(&mut self, x: f64) -> f64 { /* quotient, midpoint fallback */ } }
```

The model is `SteadyState` (doc 024 §2.2 class 4: the output depends on the
previous sample, so the `Memoryless` cases do not apply): its `steady_state(x)`
is `f(x)` exactly (the quotient's limit), `tail` is `order` frames, `peak_gain`
is the map's `max_gain`. With drive `h`: `tanh(h x)` has `F1 = ln cosh(h x) / h`,
computed in the stable form `|u| + ln(1 + e^(-2|u|)) - ln 2` (a plain `ln cosh`
overflows for large `|u|`); `atan` scaled by `2/pi` has
`F1 = (2/pi)(u atan u - ln(1+u^2)/2)/a` with `a = h pi/2` and `u = a x` (the
shipped curve is `(2/pi) atan(a x)`, slope `h` at the origin; checked against it by
central difference in the sketch); softsign has the even
`F1 = (|u| - ln(1+|u|))/h`; hard clip has the piecewise quadratic. Constants of
integration cancel in the quotient.

**Contract cases.**

| Case | Figure | Mutant |
|---|---|---|
| Antiderivative | `F1' = f` and `F2' = F1` by central difference at fixed points, per kind and at two drives (checked in the sketch to 1e-6) | `F1` unscaled by the drive |
| Identity map | `f(x) = x` gives `y[n] = (x[n] + x[n-1])/2` to the ulp (hard clip inside its linear region, checked in the sketch) | the naive `f(x[n])`, which gives `x[n]` |
| Box average | `y[n]` equals the mean of `f` over the segment `[x[n-1], x[n]]` by high-order quadrature, to 1e-9, for a curved `f` (`tanh`) | `f(x[n])` (the naive map), or the midpoint used for every `dx` |
| DC | a constant input (`dx = 0`) gives `f(x)` to 1e-9 | the quotient taken without the fallback (0/0) |
| Small step | for `dx` just below and just above the threshold the output differs from the box average by at most `max|f''| dx^2 / 24` (closed form for each kind); the naive `f(x[n])` is off by `f' dx / 2`, orders larger | fallback evaluated at `x[n]` instead of the midpoint; a threshold of `1e-12`, where the quotient cancels in f64 and the error is `eps * F1 / dx`, not `dx^2` |
| Second-order fallbacks | each of the three fallback branches returns the limit of the exact expression to 1e-6 | omit the combined branch |

**Not coverable here:** the aliasing floor of a given kind at a given `f0`. It
depends on the harmonics of `f(x_lin)`, the piecewise-linear interpolant of a
sine, not of a sine, and a margin against a render would be an unpinned figure.
The box-average case above is the exact definition and is what the realization is
held to; the floor is recorded per kind as a regression figure in the PR, not
asserted as a spec.

**Lives in** `tutti-dsp/src/waveshape.rs` and `DistortionNode` (an `order`
control). **Hosted by** doc 024 step 5; the kernels ship before it as plain
functions with the cases above. **Effort** S for order 1, S more for order 2.

### 3.2 Multirate: the oversampler

**Mathematics.** Oversampling by `N` is: zero-stuff, low-pass with a prototype to
the base Nyquist, process at `N fs`, low-pass again and decimate. It is doc 024's
class 6 with an LTI prototype; the prototype's response is what the queries read.
The two filter families are different structures, so they are two types, not one
struct with a filter field (doc 024 §2.4 item 7):

- `PolyphaseFir`: a linear-phase FIR split in `N` polyphase branches, with a
  passband edge `fp` (a field of the model) and a transition band. Group delay
  `(L-1)/2` high-rate frames per stage; for the sum of the two stages to be a
  whole base-rate frame count each `(L-1)/2` must be a multiple of `N`
  (`L = 2kN + 1`), a design constraint the contract checks.
- `HalfbandIir`: a polyphase all-pass cascade for `N = 2`, applied repeatedly for
  4x and 8x. Its structure gives queries the FIR lacks: the response is power
  complementary (`|H(w)|^2 + |H(pi - w)|^2 = 1`), so passband ripple and stopband
  attenuation are tied, and `Fidelity` is answered from the coefficients.

Both are also `Lti` prototypes (so a `Decimator<P>` has the `l1_norm` the peak bound needs), and both implement doc 026 §3.2's `TimeScale` (`ratio`, and `reach`, the base-rate
frames a realization reads either side) and `Fidelity`; a `Decimator<P>` is the
`1/N` type, so an oversampled shaper is up, map, down, each a `TimeScale`:

```rust
pub struct Oversampled<U, D> { /* up: U, a shaper kind and drive, down: D */ }
impl<U: TimeScale, D: TimeScale> System for Oversampled<U, D> { /* latency the sum of the stages'; onset 0 */ }
impl<D: TimeScale> SteadyState for Oversampled<PolyphaseFir, D> {
    /* steady_state(x) = f(x); peak_gain = l1(down) * sup |f(u)| over |u| <= overshoot_bound(up): the up stage
       can lift |x| <= 1 to overshoot_bound, and the down stage can lift it again by its l1 (>= 1 for a unit-DC low-pass) */
}
```

`Oversampled` is not `Wiener` (doc 024 §4.9's `Wiener` is an LTI part then a
map at one rate); it is the same shape with rate-changing linear parts, and its
one user so far is this. If a second appears, doc 024's `Wiener` is the place to
generalise it.

**Queries.** `ratio`; `reach`; `error_floor`, the worst `|H|` over the
**stopband only**, `[fs - fp, N fs/2]` in base-rate units (the transition band
`[fp, fs - fp]` is excluded, where a half-band is -6 dB at `fs/2` by
construction), so `fp` is a field and the band is stated at the prototype's rate;
the passband ripple over `[0, fp]` from the same `frequency_response`;
`latency` and `onset` (P1); and, on the FIR only, an inherent `overshoot_bound`: the least
`B` with `|y| <= B` for every `|x| <= 1` through the up stage, which is the
maximum over branches of the branch's `l1` (the sign pattern of the worst branch
attains it, so it is exact). It is a property of the chosen finite FIR, not of
an ideal reconstruction (whose branch sums diverge), and the all-pass IIR's
branches are infinite sums, so there is no `overshoot_bound` for it.

**Sampler tier.** Doc 026's sampler read is a position-driven fractional read at
a varying ratio (`Varispeed`, `reach`); its stated fallback for Hermite's alias
floor (026 Q5) is the windowed-sinc kernel of `resample/` evaluated at a
fractional offset. That is a kernel table, not a polyphase bank with a fixed
factor, and it is 026's, not this section's.

**Contract cases** (`TimeScale` and `Fidelity`):

| Case | Figure | Mutant |
|---|---|---|
| Ratio | output frames are `N` times input, exactly | phase off by one frame |
| Stopband | a tone at the image band of the up stage (high-rate input) leaves the up stage under `error_floor`; a tone above `fp` at the down stage's input leaves under it | a prototype with half the taps |
| Passband | a tone below `fp`: gain within the passband ripple | the gain `N` missing |
| Latency and onset | the first non-zero output equals `onset()`; the tone-burst delay equals `latency()` within half a frame; for the FIR `(L-1)/2` is a whole multiple of `N`, refused at construction otherwise | a stage one frame short |
| Overshoot (FIR) | the worst branch's sign pattern reaches `overshoot_bound` to 1e-9 and no input exceeds it | the sum over all branches |
| Power complement (IIR) | `|H(w)|^2 + |H(pi-w)|^2 = 1` to 1e-9 at 64 frequencies | a coefficient perturbed |
| Quality | across tiers `Fidelity` is non-increasing and `Effort` non-decreasing (`Graded`, doc 024 §4.7) | a tier that changes nothing |

A quality change is a rebuild through the graph's crossfade (doc 024 §4.7), so a
tier that changes `latency` (an IIR Draft against an FIR Standard) is a new `Shape`
and recompiles PDC then; the node says so in its docs. Doc 024 Q8 says an export may not yet
render at a different `Quality` from the live graph; this document does not
reopen it, so the oversampler's tier is the node's, live and export alike.

**Lives in** `tutti-dsp/src/resample/` (the polyphase and half-band kernels,
beside `WindowedSinc`; the FFT resampler stays a separate realization). **Hosted
by** doc 024 step 5 (`Graded` for oversampling is named in its row), with the
kernels first as plain code, and step 7 for the resampler's adoption of
`TimeScale`. **Effort** M.

### 3.3 True peak and the inter-sample-peak limiter

**The meter, and where it lives.** `tutti-measure` already reports true peak (the
`ebur128` crate, §1.1). A streaming reading in the plan, like `Editor::open_meter`,
cannot use `tutti-dsp`'s interpolator: `tutti-graph` depends on `tutti-types`,
`tutti-rt` and `tutti-ump` only, and the layer-2 crates have no edge between
them. Options:

1. **A tap read by `tutti-measure`.** `Graph::open_tap` already copies a port out
   of the plan, and `TapReader` is a `Source`; a true-peak reader in
   `tutti-measure` pulls from any `Source` through the polyphase kernel
   (`tutti-measure` depends on `tutti-dsp`). No new op, no layer change. The
   reading sees the signal before the engine's declick fader and fold (the tap's
   stated property) and lags by the ring; it is a control-side meter.
2. **A second copy of the kernel in `tutti-graph`** behind a `level_cell` op, with
   its own tests. Sample-exact per block, at the cost of a duplicated algorithm.
3. **Move the polyphase kernel below both**, to a layer-1 crate. A layer-table
   change for the owner, for one kernel.

`tutti-measure`'s existing reading goes through `ebur128`, a loudness meter that
also reports true peak at 4x or 2x; a second polyphase reader is wanted for a
streaming figure and to share the limiter's own kernel and its `cos(pi/2N)`
margin, so the meter and the limiter state one figure. A tap counts what a slow
reader loses (`TapReader::take_lost`), so this is a metering reading, not a
guarantee: the limiter's guarantee is the realization's own. Recommendation:
option 1. It needs nothing new in the graph, and no consumer named here needs a
meter that is sample-accurate with the block. Option 2 is the fallback if one
appears.

**The limiter: one design, a measured detector.** The detector takes the actual
interpolated peak over the lookahead window; it does not use `overshoot_bound`,
which bounds *any* input and would attenuate a signal whose true peak is below the
ceiling. Let `x(t)` be the ideal reconstruction of the base-rate signal, band-limited
to `fs/2`, and `P_N` the maximum of `|x|` over the `N`-times grid in the window.

*The margin, in closed form.* For a real signal band-limited to `sigma = pi fs`
and bounded by `M` on the whole line, with `|x(t0)| = M` at its global maximum,
Bernstein's inequality gives `x(t0 + s) >= M cos(sigma s)` for `|s| <= pi/(2 sigma)`. The nearest grid
point to `t0` is within half a grid step, `s <= 1/(2 N fs)`, so
`sigma s <= pi/(2N)` and

```
P_N >= M cos(pi / 2N)        so         true peak M <= P_N / cos(pi / 2N)
```

The factor is `1.0824` (0.688 dB) at `N = 4` and `1.0196` (0.169 dB) at `N = 8`,
and it is attained by a tone at `fs/2` whose peak falls midway between grid
points (checked in the sketch for the continuous-time tone: the grid peak is exactly
`A cos(pi/2N)`). With a
finite interpolator the grid values carry its error: relative error `delta`
(passband ripple plus stopband leakage, from `Fidelity`) gives
`M <= P_hat_N (1 + delta) / cos(pi/2N)`. The Bernstein-type statement is
standard in the peak-to-average literature for oversampled signals; citing and
proving it is a first-PR obligation, and it is unverified here (§11). **Limit:**
the inequality needs `|x| <= M` everywhere, so `M` is the global supremum of the
signal; the detector sees a window, whose maximum `m` can be below the global one
and then `P_N >= m cos(pi/2N)` does not follow (the curvature near a smaller
local peak can be as large as `sigma^2 M`). The realization therefore takes the
window long enough that the gain at `t` is the minimum over every window covering
`t`, so the window holding the maximiser governs; the proof obligation includes
that covering argument, and the contract's stimulus (below) is built to test it.

*The gain, and what the guarantee is.* The gain `g` for a window is
`c_eff / (P_hat_N (1 + delta) / cos(pi/2N))`, where `c_eff = c - Dg_max B X_max`
is the ceiling less the variation term below (`c_eff = c` when the gain is
constant over the support). It is transparent (`g = 1`) exactly when
`P_hat_N (1 + delta) <= c_eff cos(pi/2N)`. The output is the
reconstruction `y(t) = sum_k h(t-k) g_k x_k` by the detector's interpolator of
the gained samples. Write `g_c` for the gain at the sample nearest `t` and
`Dg = max_k |g_k - g_c|` over the interpolator's support (`2S + 1` frames). Then

```
|y(t)| <= g_c |x_P(t)| + Dg * B * X  <=  g_c * P_hat_N (1+delta)/cos(pi/2N)  +  Dg * B * X
```

with `B` the interpolator's `overshoot_bound` and `X = max |x_k|` over the support.
The first term is `<= c_eff` by the choice of `g`. The second is the price of a gain
that varies across the support: it vanishes when the gain is constant there. The
two things the realization owes are therefore (i) the detector lane is taken as
the peak over `+-S` frames so a protecting gain reaches every output point whose
reconstruction the peak touches, and (ii) the attack ramp is long enough that `Dg`
over `2S + 1` frames is as small as the design states (the cost is a slightly
softer attack than today's instant one). The node's `Dg` is measured by the
contract, and its internal target is `c_eff`, so the sum stays at `c`. The guarantee holds for the reconstruction by the detector's
own interpolator; a DAC's differs by that interpolator's `Fidelity`.

**Composition.** The model is `System` plus `SteadyState` (`steady_state` maps the
settled grid peak to the gain), with the detector and the windowed minimum
**inherent**, as doc 024 step 4 says (the limiter's windowed minimum stays
inherent: class E and a nonlinear operator). It is not `Wiener`, whose linear
part must be `Lti`; an up-sample, `abs` and sliding maximum is not linear.

**Contract cases:**

| Case | Figure | Mutant |
|---|---|---|
| Margin | a tone at `(0.5 - e) fs` (a tone at exactly `fs/2` is not represented by its base-rate samples, which are `+-a`) over a long run with the phase swept: in every window `P_hat_N (1 - delta) <= true peak <= P_hat_N (1 + delta) / cos(pi/2N)`, the lower bound approached and the upper only in the limit `e -> 0`; and the limited output's 16x-oversampled peak is `<= c` | detector without the `cos` division (the upper bound fails) |
| Transparent | a signal with `P_hat_N (1 + delta) <= c_eff cos(pi/2N)` has gain exactly 1 | a fixed headroom of 1 dB |
| Inter-sample peak | a full-scale pair of alternating samples with the peak between them: sample peak `0.707`, true peak `1.0`; the limited true peak is `<= c` plus the stated `Dg` term | detect on the sample peak (today's limiter) |
| Constant gain | when `g` is constant over the support the output peak equals `g` times the input's to `delta` | gain applied before the interpolator |
| `Dg` | the measured gain variation over `2S+1` frames is at most the declared figure | an instant attack (`Dg` large) |
| Delay | `latency()` equals the lookahead plus the interpolator's stage delay, measured by the tone burst | omit the interpolator's |

**Lives in** `tutti-dsp/src/dynamics/` (a true-peak detector and a mode of
`LimiterKernel`, which exists since #443) and a `LimiterNode` mode in
`tutti-nodes`. **Hosted by** doc 024 step 4, using the oversampler kernels of
§3.2 (not step 5's adoption). **Effort** S to M.

### 3.4 The EQ family

**Parametric EQ.** Doc 024 resolved it: one design whose model is
`SeriesN<SvfModel, N>`, with an exact product response, stability of the worst
band and ring time of the slowest (doc 024 §4.9, Q4). Higher-order slopes are
`SeriesN` of the same band model with the pole angles of the Butterworth
polynomial; tilt and Baxandall shelves are closed-form biquads. The Butterworth
oracle is exact with no warp slack: a prewarped bilinear (trapezoidal) cascade of
order `n` has `|H|^2 = 1 / (1 + (tan(w/2)/tan(wc/2))^(2n))`, so the contract pins
the response to the ulp, `-3.01 dB` at `fc` exactly, and a slope of `20 n dB` per
decade only for `fc` well below Nyquist (the figure is qualified). `SvfModel` is
second order, so the first PR covers even orders; odd orders add a first-order
section. Mutant: a wrong `Q` ladder fails at the anchors.

**Dynamic EQ.** A dynamic band is a bell whose gain follows a detector on a
band-passed sidechain. Doc 024 has the pieces but not the structure: the
controlled thing is the gain *parameter of an `Lti` filter*. It is doc 024's
`Controlled` (§4.5a, a design evaluated at `theta(t)` from a source of controls) with the source
a `SteadyState` detector in steady state (`Settled`) and a design map, a composition, not a
second trait. The earlier sketch, `Controlled<D, F>` with `F: Fn(Db) -> L`, is the `Settled`
instance of the general type, and doc 024's proposed `Ltv` it was written against is not built:

```rust
/// A dynamic EQ band: the design `D` (a bell whose gain is its control) driven by a detector in
/// steady state; `Controlled<Settled<Det>, D>` in doc 024 §4.5a's terms.
/// `at(level)` is `design(detector.steady_state(level))`.
```

`at(level)` answers every class-1 query at a settled level. It is exact only when
(a) the detector is feed-forward (a detector fed from the band's own output makes
`at` an implicit equation), (b) the sidechain band-pass has unit gain at the
band's centre (otherwise the level the detector sees is the input level times
that gain, which `at`'s caller must apply), and (c) the gain map's convention is
stated (the band's gain at the threshold is the static bell, or 0 dB, per the
design). `Controlled` implements `System`; the contract reads `at`. Cases: a
steady sine at the band centre, settled ten time constants, measures
`|at(L).frequency_response|` within 0.1 dB at five levels (mutant: the detector
reading the output); the attack and release constants measured from a level step
equal the detector's; superposition fails at two levels (a negative case that
keeps the model out of `Lti`, not a mutant).

**Linear-phase EQ.** A symmetric FIR by frequency sampling, run by the existing
`Convolver`. The model is an `Lti` whose group delay is constant; its `latency` is
the centre tap, `(L-1)/2` (P1; the declared alignment delay), while its `onset()` is
near 0 (the pre-ringing taps are non-zero) and the pre-ringing falls before frame 0 of
the aligned output. No trait: one family (a symmetric FIR) has this
answer. Cases: the magnitude equals the design's on the design grid to 1e-6
(exact for an odd-length unwindowed inverse DFT of the sampled response) and
between grid points within the stated interpolation bound; the phase is linear to
the frequency-sampling error; `l1_norm` equals the sum of absolute taps; the
tone-burst delay equals `latency()`; an even `L` is refused by the constructor; the
tail is the remaining `(L-1)/2` taps. Mutant: an asymmetric FIR fails the phase case.

**Match EQ and spectral dynamics** are not families: match EQ is a
`tutti-measure` spectrum, a smoothing and the FIR design; frequency-selective
dynamics is signal-dependent, so a `BinMap` (§3.8), not a per-bin gain.

**Lives in** `tutti-dsp/src/filter/` (the cascades, tilt, the FIR design) and
`tutti-nodes` (one multi-band `EqNode`). **Hosted by** doc 024 steps 2 to 4 (SVF,
`SeriesN`, detector); the linear-phase EQ's model through the `Convolver` is step
6 (`FirSummary`), though a plain FIR-by-convolver ships earlier. **Effort** M each.

### 3.5 Gain matrices: pan laws, VBAP and ambisonics

**Mathematics.** Every panner is a gain matrix: doc 024 §4.8 states it (an
order-0 `Lti`: latency 0, tail `Finite(0)`, `spectral_radius` 0, the response the
matrix at every frequency), and rows 31, 32, 44 and 52 are that. More than one
family exposes the matrix itself, which `Lti` does not. **Struck 2026-11-11: the
planned `GainMatrix` trait.** `Fold` (`tutti-types`) already is the sparse matrix
between two topologies, and doc 024 §4.2 (order 0, basis and metric) makes it the one order-0
map: a validated constructor from a matrix, `matrix`, `column_power` and an `is_isometry`
measured in the topologies' metrics, stored in `f64` and narrowed once at the apply (Q26, step
7a). Pan laws, mid/side, VBAP, the spherical-harmonic models and a basis conversion are
`Design`s whose model is a `Fold`; `Fold::between` stays the default policy (the ITU
coefficients). For order 0, `hinf_norm` is the largest singular value and `l1_norm` the largest
row sum (P2), in the metric. The earlier trait's `is_isometry` was Euclid-only
(`hinf_norm == 1` and every `column_power == 1`), which classifies a change of normalisation as
not an isometry; the relative form replaces it. `spectral_norm` is not a separate query (P2).

**Pan laws.** A law is a mono-to-stereo map `Pan -> (gl, gr)`, a 2x1 matrix whose
`column_power` is the figure that defines it: 1 for the constant-power law
(`gl = cos(theta)`, `gr = sin(theta)`, `theta = (p + 1) pi/4` for position `p` in
`[-1, 1]`); `gl + gr` constant for the constant-amplitude law; and for the
`-a dB` centre laws (`a` = 3, 4.5, 6) `gl = gr = 10^(-a/20)` at `p = 0`. A stereo
balance (today's strip, doc 024 row 52) is the 2x2 case. Three existing laws are
in the code and the work is to **consolidate them** (a doc 024 consolidation in
kind): the strip's `balance_gains`, the polysynth voice's inline
`sqrt((1 -+ p)/2)`, and `CrossfadeCurve::EqualPower`; the polysynth law is constant power (`gl^2 + gr^2 = 1`) but a different position
mapping, `gl = sqrt((1 - p)/2)`, not `cos((p+1) pi/4)` (at `p = 0.5` it gives 0.5
against 0.383), so folding it onto the cos/sin mapping changes the polysynth's
render. The consolidated module therefore offers both as named variants and the
polysynth keeps its own, with no render change; unifying onto one mapping is an
decision (§9 D17). The crossfade is the cos/sin law's two-input form. Cases: power 1 at five positions (mutant: `sin` and `cos`
swapped, which preserves power and is caught only by the hard-left case below);
hard left is `(1, 0)`; centre is `(1/sqrt 2, 1/sqrt 2)` (constant power) or
`10^(-a/20)`; monotone in `p`.

**VBAP.** The matrix has at most three non-zero entries per column (a tuple), all
non-negative, `column_power = 1` (the normalisation of Pulkki 1997), the active
tuple's gains positive when the direction is inside its triangle, and a source at
a speaker's position puts all energy there. Mutants: a skipped normalisation
(column power off), a tuple selection that ignores the interior-speaker test
(a gain continuity break across a triangle edge).

**Spherical harmonics.** Real harmonics in ACN order, SN3D or N3D (the ambiX
convention; FuMa is an import concern):

- `ShEncode` (direction to coefficients, `(order+1)^2 x 1`). `degree_energy(n)`,
  an inherent method, is the sum over `m` of `Y_nm^2` for degree `n`: 1 for SN3D
  and `2n+1` for N3D at every direction (the addition theorem; checked to first
  order in the sketch). Column power is therefore `order + 1` (SN3D) or
  `(order + 1)^2` (N3D); an encode is **not** an isometry. The two normalisations are two
  bases of one field, `N3D = sqrt(2n + 1) SN3D` per degree `n`, with Gram matrices `I` and
  `diag(2n + 1)` (doc 024 §4.2): the conversion between them is an isometry in the metric.
- `ShRotation` (block diagonal, one orthogonal block per degree; SN3D and N3D
  differ by a per-degree scalar that commutes with it, so the matrix is the same).
  `hinf_norm = 1` and every `column_power = 1`, so it **is** an isometry
  (`Fold::is_isometry`, in both metrics). **Rotation commutes with encode:** `R(q) E(d) = E(q d)`, an
  identity between two models' answers, checked on the control side (verified in
  the sketch for a yaw at first order). The test rotation must not be an
  involution (a 180 degree turn makes the transpose mutant pass). Mutant: the
  inverse rotation fails it.
- `ShDecode` (coefficients to `speakers`; it carries the normalisation, which the
  decoder depends on). A sampling decoder is an isometry up to `1/sqrt(speakers)`
  only for a layout that is a spherical `t`-design with `t >= 2 * order`, in an N3D
  basis (the N3D metric: in SN3D coordinates it is an isometry relative to the SN3D Gram
  matrix); a regular polyhedron is not enough (an icosahedron is a 5-design, exact to
  order 2 and not 3), and 5.1 or 7.1.4 layouts are not designs. For those the
  decoder is AllRAD: decode to a dense virtual layout that closes the convex hull
  (imaginary speakers) and pan the virtual feeds to the real layout with VBAP. In
  `Series` order that is `Series<ShDecode, VbapColumns>`: the SH decode to virtual
  speakers **first**, then the VBAP matrix (one column per virtual speaker). Max-rE
  weighting has `rE` equal to the largest root of the Legendre polynomial
  `P_(order+1)`: `1/sqrt 3`, `sqrt(3/5)` and `0.8611` for orders 1 to 3 (numerical
  beyond). Binaural decode is a `Series` of the virtual-layout decode and the
  HRTF's `Lti` pair (doc 024 row 45).

**Channel convention.** ACN and SN3D. The topology is what lets a node refuse a wrong wiring:
a bus must say what it carries. Under doc 024 §4.2 that is not a separate `Ambisonic` kind
(doc 023 G11's plan, **struck 2026-11-11**) but `ChannelTopology::ambisonic(order, norm, ordering)`
over `Speaker` variants (a harmonic by ACN index), with the metric `gram` beside it; step 7a
builds it.

**Lives in** `tutti-types` (`Fold`, the topology and its metric), `tutti-dsp/src/spatial/` (the matrices; §4) and `tutti-spatial`
(the nodes: encode, rotate, decode, binaural; it stays at layer 5 over
`tutti-nodes`, §4.1). **Hosted by** doc 024 step 7 (panners and fold) and step 6
(the HRTF as a published model). **Effort** S for pan laws and their
consolidation; M for VBAP in `tutti-dsp` (§4); L for ambisonics.

### 3.6 Dither and noise shaping

**Mathematics.** A quantiser with error feedback is
`v[n] = x[n] - sum_k c_k e[n-k]`, `y[n] = Q(v[n])`, `e[n] = y[n] - v[n]`, so
`y - x = e[n] - sum_k c_k e[n-k]`: the error is the quantisation noise filtered by
the **noise transfer function** `NTF(z) = 1 - sum_k c_k z^-k`, monic by
construction. `DitherNoise`'s dither is **non-subtractive**: the quantiser sees
`v + d`. With `d` triangular on `[-1, 1]` LSB (variance `lsb^2/6`) and the
quantisation error uniform (`lsb^2/12`), the total error variance of `y - x`
unshaped is `lsb^2/4` for TPDF and, on average, `lsb^2/6` for rectangular dither
(whose noise modulation makes the second moment signal-dependent, so only TPDF's
figure is level-independent); the
generator's own variance alone (`lsb^2/6`, doc 024 row 19) is not what a
measurement of `y - x` shows. Whether the dither is fed back changes the result
(it is shaped only if it is). The noise model holds only while the quantiser does
not overload, which for a high-order NTF near full scale is not guaranteed: the
quantiser input is bounded by `|x| + (l1(NTF) - 1)(lsb/2 + dither)`, a condition
the design states.

**Structure.** `Noise` (doc 024 §4.2) is the process, and the shaping is an `Lti`:

```rust
/// A noise process through an LTI filter. psd(f) = psd_N(f) |H(f)|^2 (two-sided, per unit
/// normalised frequency, flat for white input); variance = variance_N * h2_norm(H)^2
/// for white input; the range scales by l1_norm(H) (the guarantee, not the variance).
pub struct Filtered<N: Noise, L: Lti> { /* noise, filter */ }
```

`variance` uses `h2_norm` (P2), not `l1_norm`: `l1^2` is only an upper bound and
doc 024 §6.2 tests `Noise::variance` against the measured variance. (Checked in
the sketch: for the taps `[.5, .5, .5, -.5]` the variance is `1.0` while `l1^2` is
`4.0`.) The NTF is a monic `Lti`; "minimum phase" and "monic" are properties of
its taps, checked by the design, with no trait: a single family answers them.

**Queries and cases.** The shaped psd at any frequency; total power; and the
Gerzon-Craven integral, `mean of ln|NTF(e^jw)| over the circle`, which is 0 for a
monic minimum-phase NTF and positive otherwise (noise cannot be removed, only
moved: a shaper that lowers the audible band by `A` dB raises the rest). It is
computed by uniform quadrature on at least `2^14` points, which is accurate to the
quadrature error away from zeros near the unit circle and degrades near them; the
tolerance is stated per design (checked in the sketch: `[-0.9]` gives 0, `[-1.5]`
gives `ln 1.5`). Cases: monic (`h[0] = 1`); minimum phase (the integral within the
stated tolerance of 0); the measured error spectrum of quantised silence with TPDF
dither equals `Filtered::psd` within a stated statistical tolerance over a long
seeded run; total error power `lsb^2/4` unshaped; no overload within the stated
input range. Mutants: a non-monic NTF; a feedback sign error; a non-minimum-phase
NTF (integral positive). Psychoacoustic NTFs are designed from published
weighting curves, not copied from a proprietary shaper (Lipshitz, Wannamaker and
Vanderkooy 1992 for the theory).

**Lives in** `tutti-dsp/src/dither.rs` (the shaper beside `DitherNoise`, the NTF
designs); `tutti-export`'s `process/dither.rs` selects it. **Hosted by** doc 024
step 7 (dither). **Effort** S for a fixed set of NTFs; M for a weighted,
rate-dependent design.

### 3.7 Generators: band-limited oscillators, wavetable and FM

**Mathematics.** Doc 024 class 5, `Periodic`: a frequency and a harmonic series.
Aliasing is the harmonics above Nyquist folding back; two families answer "how
many harmonics above a floor", so the trait is kept:

```rust
pub trait BandLimited: Periodic {
    /// The largest harmonic index whose amplitude is at least `floor` (linear amplitude).
    fn top_harmonic(&self, floor: f64) -> usize;
    /// Harmonics `k` with `k * frequency > rate/2` and amplitude at least `floor`: the ones that fold.
    fn aliasing_harmonics(&self, rate: Hz, floor: f64) -> usize { /* provided */ }
}
```

PolyBLEP and PolyBLAMP are not band-limited (they attenuate the first alias
harmonics), so they implement `Periodic` and not `BandLimited` (doc 024 row 34).

- **Mipmapped wavetable.** One table per octave band, each holding only harmonics
  below Nyquist for the highest `f0` it serves. `WavetableModel` carries a level's
  harmonics; `top_harmonic(floor)` is the level's. Table harmonics at or below
  Nyquist do not make the **render** alias-free: the 4-point interpolation
  creates images of the in-band harmonics, so the `Fidelity` terms are the droop
  and the image floor. The table build (FFT, zero above the limit, inverse FFT)
  is control-side over `RealFft`; the realization is a lookup, interpolation and a
  crossfade between levels.
- **FM and phase modulation.** A carrier `c f0` phase-modulated by a modulator
  `m f0` at index `beta` is `sin(c w t + beta sin(m w t)) = sum_n J_n(beta) sin((c + n m) w t)`.
  The ratio is held as **integer multiples of a common fundamental** `f0`
  (`PmPair { f0, c, m, beta }`): an arbitrary real ratio such as 1.41 is
  quasi-periodic, not `Periodic`, and a `gcd` of two `f64` Hz is ill-defined, so
  that case is outside the class. Sidebands with `c + n m < 0` fold to positive
  frequency with a sign, and `harmonic(k)` includes them. `top_harmonic(floor)` is
  exact: the largest `n` with `|J_n(beta)| >= floor` (by the Bessel recurrence),
  mapped to `c + n m`. Carson's rule, `ceil((c f0 + (beta + 1) m f0) / f0)`, is
  an approximation (about 98% of the power) and is kept only as a cross-check.
  DC is zero for sine carrier and sine modulator only; the phase convention is
  stated. Feedback FM (an operator modulating itself) has no closed-form
  sidebands and is inherent to its realization, which oversamples.

**Cases.** Bessel amplitudes equal tabulated `J_n(beta)` (checked in the sketch
for `J_0(1)`, `J_1(1)`); `harmonic(c)` equals `J_0(beta)` up to the fold-over
term `J_(-2c/m)` (a stated, tiny figure); `top_harmonic` is monotone in the floor;
for a wavetable, a built table's harmonics equal its source coefficients to 1e-6.
The render's alias measurement looks at the **non-harmonic bins** (aliases land
at non-multiples of `f0`), at the top `f0` of each level, against the model's
`Fidelity`. Mutants: a table that keeps one harmonic above the limit; a sideband
sign error; a missing negative-frequency fold.

**Lives in** `tutti-dsp` (`wavetable.rs`, `pm.rs`); `tutti-polysynth` keeps its
`f32x8` bank and calls in (doc 024: instruments' models call into `tutti-dsp`).
**Hosted by** doc 024 step 7. **Effort** M each. Karplus-Strong and modal banks
are small `Lti` models (a delay line with a loss filter) that ride step 7 as
demand appears.

### 3.8 Spectral processing: one framework

**Mathematics.** An STFT processor analyses with window `w_a`, hop `H`, applies a
frequency-domain map and resynthesises with `w_s` by overlap-add. With a periodic
window and `size % H == 0`, the identity map reconstructs the input scaled by a
constant when the overlap sums of the product window are one constant `c`:

```
sum over m of  w_a[n + mH] * w_s[n + mH] = c     for every n in [0, H)
```

For the shipped pairs `c` is not 1: Hann for both analysis and synthesis at hop
`size/4` gives `c = 1.5`; Hann analysis with a rectangular synthesis gives
`c = 2` (checked in the sketch). The synthesis is scaled by `1/c`, and
`pr_error` is the worst relative departure of the overlap sums from their
constant. This generalises `StftGeometry::is_cola`, which checks that the sum of
`w^2` is *flat*, not equal to 1.

```rust
pub struct StftPair { /* size, hop (size % hop == 0), periodic analysis and synthesis windows */ }
impl StftPair { pub fn cola_constant(&self) -> f64; pub fn pr_error(&self) -> f64; }
/// A per-bin real gain constant in time (a spectral EQ, a settled spectral gate) is a plain
/// slice parameter: no trait until a second implementer exists.
pub struct StftEffect { /* pair, per-bin gains: Vec<f32> */ }   // onset 0, latency size - 1
```

**Delay.** The first output sample is ready only after a whole window
(`StftGeometry::latency`, in seconds, is one window: the time for the first frame
to be analysable). A realization whose render is independent of the host block
size must delay by the worst case: output sample `n` is last covered by the frame
starting at `floor(n/H) H`, which needs input up to `n + size - 1` when `n` falls
on a hop boundary, so `latency = size - 1` (P1), and the "blocks 1, 17, 64
bit-identical" case is consistent with it. `onset()`, the first non-zero frame of `h`, is 0 for
an identity-like map.

**Class.** A per-bin multiplier is a circular convolution, so the system is `Lti`
only to the extent the gain's impulse response is short against the window.
`StftEffect` is `System` with `Fidelity` (the share of the gain's impulse-response
energy that wraps at this size: a zero-phase real gain is symmetric about 0 and
wraps circularly, so the figure is computed from the gain's inverse transform) and
offers the class-1 queries at the sampled response, stated with that bound as doc
024 Q23 does for the frozen-time query. The wrap share is the aliasing norm of the lifted periodic
system (doc 024 §4.2, class 6: block-periodic systems are LTI under blocking), and the model's
period is `Cadence::stream(hop)`. A map that is not a time-constant multiplier (a gate
with hysteresis, freeze, morph, spectral dynamics) is a **`BinMap`** the
realization runs, with its own cases and no class trait (doc 024 §2.4 item 8).

**Relation to doc 026.** `LockedVocoder` is a `TimeScale` realization over the
same pair: it differs in hops between analysis and synthesis and shares
`StftGeometry`, `RealFft` and the windows; it shares no per-bin map. §9 D6 places
the pair.

**Cases:** `pr_error` under 1e-6 and `cola_constant` equal to 1.5 and 2 for the
two shipped pairs; the identity map returns the input delayed by `latency` to
1e-5; a constant gain `g` is `g` times the input; a zero gain at the Nyquist bin
removes a Nyquist tone; block independence (1, 17, 64 bit-identical); no
allocation. Mutants: a hop that breaks COLA (`pr_error` rises); a missing
synthesis window; a `1/c` scale missing.

**Lives in** `tutti-dsp/src/stft.rs` beside `geometry.rs`; kernels (gate,
spectral-subtraction denoise with a Wiener gain, freeze, blur, resonator) are
`BinMap`s, each a small file; one `StftNode` in `tutti-nodes`. A channel vocoder
(filter bank and followers) and a frequency shifter (a Hilbert pair, `Lti`) are not
STFT. **Hosted by** after doc 026 stage 2 (the vocoder is written in `tutti-dsp`;
it reuses the pair). **Effort** M for the framework, S per kernel.

### 3.9 Pitch and formant

**Mathematics.** Doc 026 settles the time-scale half: `TimeScale`, `Retime<M>`,
`Transposed<R>`. Formant preservation changes what a pitch shift does to the
**spectral envelope**: a plain transposition by `k` moves the formants by `k`;
preserving them corrects the shifted spectrum by `E(f)/E_k(f)`, `E_k` the envelope
after the shift. The envelope is estimated cepstrally; the true-envelope
estimator of Röbel and Rodet (DAFx-05) iterates a cepstral smoothing to touch the
harmonic peaks. Pitch and formant are two independent controls.

**Structure.** No trait: the two realizations that answer it, `Transposed<R>`
(formant shift equals the transposition) and a formant-preserving realization
(formant shift 0), are both the stretcher family, so each carries inherent
`transpose()` and `formant_shift()` methods (in cents). The estimator is a
**processor's internal step**, so by doc 024 Q24's cut ("what a processor needs
stays in `tutti-dsp`") it is private to the realization in `tutti-dsp`;
the processor cannot depend upward on `tutti-measure`, and no trait is added. If a
display ever wants the envelope, the estimator is exported from `tutti-dsp` at that
point and `tutti-measure` offers an analyser over it; until then it is private.

**Cases** (analytic, synthetic vowels): a harmonic stack shaped by a known
all-pole envelope (a Klatt-style resonator pair), shifted by +7 semitones with
formant shift 0: the fitted formant centres move by under `fs/order` (the
cepstral resolution) while the pitch moves by 7 semitones within the resolution
of the pitch estimator named in the case (the YIN window and interpolation; a
"within 1 cent" figure is stated for that estimator only); with formant shift equal
to the transposition the formants move by 7 semitones; the envelope fit of a
stack with a known envelope equals it within a stated dB at the harmonic peaks;
block independence. Mutant: omit the envelope correction (formants move).

**Pitch correction** (a scale quantiser with a retune speed over YIN) is a pure
function of `(detected pitch, scale, speed)` plus a one-pole `Smoother` on the
cents error; the quantiser is data (§3.13). It is a node composition, not a
family.

**Hosting.** Doc 026's stage 8 is "slice, mono or `-ffi` Signalsmith, each only if
the listening gate asks"; formant shifting is none of those and is not gated by
the listening test. It is a new realization after doc 026 stage 4 (the sampler
renders through `Retime` and `Transposed`), in the sampler first. A **live**
pitch shifter needs an input-fed push shape, which doc 026 §12 rejected for lack
of a consumer ("a live input that would need real latency has no consumer"); a
live shifter is that consumer, and building it amends 026 §12 (§9 D9).
**Effort** M for the realization, M to L for the live shifter and correction.

### 3.10 A nonlinearity inside a feedback loop

**The gap.** Doc 024 row 2 (the ladder) is "Partly": the small-signal model fits
class 1, and the `tanh` makes the large-signal response drive-dependent. Class 4 is
a map and an `Lti` in series; a nonlinearity *inside* the loop is not that. The
object is a **Lur'e system**: a stable `Lti` `L(z)` closed around a memoryless
map `phi` in a sector `[0, beta]`, `beta = max_gain(phi)`. Doc 024 §4.9 and Q17
decline a general `Feedback<A, B>` because stability depends on the loop; this adds
one loop shape whose stability argument is stated once (P3).

**The loop form matters.** The ladder is `u = tanh(x - k s[3])`: `phi` acts on the
*sum* of the input and the feedback, so with `e = x - k y`, `y = L phi(e)`:

```
e = x - k L phi(e)            (Injection::Sum: the ladder)
y = L (x - k phi(y))          (Injection::Linear: a diode clipper, x enters through L)
```

They share the loop operator and differ in the input-to-output map, so `Lure`
carries the injection point:

```rust
pub struct Lure<L: Lti, Phi: Memoryless<In = f32, Out = f32>> { /* open: L, phi, k, phi'(0), sup|phi|, injection */ }
impl<L, Phi> Lure<L, Phi> {
    pub fn sector_max(&self) -> f64;                  // beta = phi.max_gain()
    pub fn circle_ok(&self, grid: &[Hz]) -> bool;     // 1/beta + k Re L(e^jw) > 0 for all w
    pub fn critical_gain(&self, grid: &[Hz]) -> Option<f64>;   // None if L has no real negative crossing
    pub fn small_signal(&self) -> impl Lti;           // phi'(0) L / (1 + k phi'(0) L), Injection::Sum
    pub fn output_bound(&self, x_max: f64) -> f64;    // Sum: l1(L) sup|phi|; Linear: l1(L)(x_max + k sup|phi|)
    pub fn well_posed(&self, feedthrough: f64) -> bool;   // monotone phi and k * h[0] >= 0: one solution per sample
}
```

**What is stated, and in which norm.**

- **Circle criterion.** For `phi` in `[0, beta]`, a stable `L` and the loop above,
  `1/beta + k Re L(e^jw) > 0` for every `w` makes the closed loop `l2`-stable and,
  with `x = 0`, the origin globally asymptotically stable: no self-oscillation. For
  four identical one-poles (continuous time, cutoff 1) and `tanh` (`beta = 1`),
  `min Re L = -0.3466`, so the circle bound is `k < 2.885`, against the exact
  small-signal critical gain `k* = 4` at `w = 1` (`L = -1/4` there). Both are
  queries; between them the small-signal loop is stable but the circle criterion
  cannot certify it, which is conservatism of the criterion, not conditional
  stability (computed in the sketch; the trapezoidal discretisation
  moves the figures, which are for the continuous prototype).
- **`critical_gain` is `Option<f64>`:** it is `-1/(phi'(0) Re L(e^jw_pc))` at the
  phase crossover `arg L = -pi`; two poles have none (unconditionally stable), a
  conditionally stable `L` has a band, and several crossings take the smallest.
- **BIBO and `l2` are different.** For the bounded maps the doc names (`tanh`,
  `atan`, softsign, hard clip) and a stable `L`, the `Sum` form has
  `|y| <= l1(L) sup|phi|` for **every** `k` and any `x`; the `Linear` form has
  `|y| <= l1(L)(max|x| + k sup|phi|)`. Boundedness is trivial and `output_bound` is
  that figure, injection-aware. The small-gain
  condition `hinf(L) lip(phi) k < 1` is an `l2` result, and for the ladder
  (`L(0) = 1`, `hinf >= 1`) it only certifies `k < 1`, which is why the circle
  criterion is the tool. `sup|phi|` is a field (`Memoryless` has only the secant
  `max_gain = sup |f(x)|/|x|`, which does not bound `|f|`), and `phi'(0)` is a field
  checked by finite difference against `eval`.
- **Well-posedness.** The delay-free loop is an implicit equation per sample; with
  `phi` monotone and `k h[0] >= 0` (`h[0]` the feedthrough of `L`) it has exactly
  one solution, so a solver cannot fail to converge for want of one. `circle_ok` on
  a finite grid is a numerical check, not a certificate: it holds only if `Re L` is
  not underestimated between grid points.
- **Not offered:** the limit-cycle amplitude (it needs a describing function), and
  nodal DK for several nonlinearities.

**The unit-delay ladder is a different model.** `ladder_step` takes the feedback
from stored state, which inserts a `z^-1` in the loop; its small-signal response
and `k*` differ from the delay-free loop, so by doc 024 §2.1 it is another model,
or the cheaper realization of one with a `Fidelity` term that is not small near
the cutoff. A solved realization (a Newton step or two per sample) is a second
realization of the delay-free `Lure`.

**Cases:** small-signal: at input levels below 1e-3 the impulse response equals
`small_signal.frequency_response`; below `critical_gain` an impulse decays, above
it a small input grows at the rate of the small-signal poles until `phi` limits it;
inside the circle bound the origin is stable at every tested level; output bounded
by `output_bound` at any `k` for the stated injection; `well_posed` holds for every shipped design; DC: the fixed point of the loop equation equals the
settled output; block independence; no allocation and a bounded solver iteration
count. Mutants: `phi` applied after the sum instead of on it (the injection
form); a solver stopping after zero iterations; the unit-delayed feedback, which
moves `k*` and fails the critical-gain case.

**Lives in** `tutti-dsp/src/filter/` and the `model` module; `Lure` goes to doc 024
§4.2 at the PR of the first realization, (§9 D10). Candidates: the nonlinear TPT
ladder, a diode clipper, and Holters's stateful ADAA of a ladder (§3.1). Wave digital filters author the same circuits another
way and are a non-goal (§7). **Hosted by** after §3.1, §3.2 (these circuits need
oversampling) and doc 024 step 2. **Effort** M.

### 3.11 Reverb

Doc 022 owns the reverb and this adds nothing to its order. Dattorro (step 3),
early reflections as `Parallel<Taps, Network>` (step 3b), velvet noise
(doc 022 §6 row 6) and Schlecht and Habets on feedback matrices (doc 022 §6 row 2) are already
its items. The only addition is queries on the existing `Network`: the spectral
flatness of the lossless prototype (colourlessness) and a velvet-noise
decorrelator for stereo width (a sparse `Fir` `Lti`: its `l1_norm` is the number of
non-zero taps, its response a sum of delayed unit pulses, exact), both designs in
`tutti-dsp/src/network/`. Shimmer is §9 D3. **Effort** S each.

### 3.12 The utility pack

Each is a node over an existing class; none needs a new trait.

| Item | Math and class | Cases (mutant) | Where |
|---|---|---|---|
| Polarity, mono sum, channel swap | order-0 `Lti`, a signed matrix | the matrix; `column_power` (a sign dropped) | `tutti-nodes`, building a `Fold` (which lives in `tutti-types`, so it cannot implement dsp traits, doc 024 row 32) |
| Mid/side, width | order-0: `M = (L+R)/sqrt 2`, `S = (L-R)/sqrt 2` (an isometry, checked in the sketch); width scales `S` by `w` | round trip identity to 1e-12; `w = 0` is mono (a swapped `S`) | `tutti-dsp` matrix, node |
| DC blocker | `Lti`, `H = ((1+R)/2)(1 - z^-1)/(1 - R z^-1)` with `R = (1 - tan(wc/2))/(1 + tan(wc/2))`: `-3 dB` at `wc` exactly and unity at Nyquist (checked in the sketch); the unscaled `(1 - z^-1)/(1 - R z^-1)` has Nyquist gain `2/(1+R)` | `H(1) = 0`; `-3.01 dB` at `wc` to 1e-12 (the unscaled form fails); stable for `R < 1` | `tutti-dsp/src/filter/` |
| Tremolo, auto-pan | a time-varying order-0 gain `1 + d lfo(t)` over a class 5 LFO | sidebands at `+-f_lfo` of amplitude `d/2` for a sinusoidal LFO (a `d` doubled); auto-pan's column power varies as its law | `tutti-nodes` over `ModulatorNode` |
| Ring modulation | a product with a `Periodic` carrier | sideband amplitudes closed form (a missing carrier phase); no aliasing while `fc + fm < fs/2` | `tutti-nodes` |
| Hilbert pair, frequency shifter | the Hilbert transform as two all-pass chains 90 degrees apart (`Lti`); an SSB shift is a `Series` and a product | phase difference `90 deg +- e` over the band, `e` stated; sideband suppression (a chain of the wrong order) | `tutti-dsp/src/allpass.rs` |
| Pan laws | order-0 `Fold` model | §3.5 | `tutti-dsp/src/balance.rs` |
| Delay variants (ping-pong, tempo-synced, ducking) | `Lti` with feedback as structure inside the model (doc 024 Q17); ducking is a `GainControl` in the loop | loop gain below 1 (stability); echo spacing equals the tempo-derived time (the transport's tempo ignored) | `tutti-nodes` over `DelayLineNode` |
| De-esser, transient shaper | class 4: a band-passed detector `GainControl`; two followers' difference `GainControl` | `steady_state` curve; time constants | after doc 024 step 4 |

**Effort** S each. A loudness-normalise **node** is not an item: real-time gain
from an integrated figure is not causal, and the measure-then-apply normalisation
already exists in `tutti-export/src/normalize.rs`.

### 3.13 What stays inherent

Doc 024 §2.4 says a family that cannot be stated by a class without forcing it
stays a plain type with its own tests. These do:

- **Warp markers and the beat map.** A monotone piecewise-linear map from beat to
  source frame: data, a pure function of position (doc 024 row 29's reasoning),
  consumed by doc 026's warp (§6.2 there: a longer segment list), which doc 024 §4.5a reads as a
  monotone scalar `Trajectory<f64>` and not as a separate `Warp` trait. Auto-warp from
  `tempo` and `beat` is a `tutti-measure` analysis plus a data edit.
- **Scale quantisation and retune policy:** a pure function, data plus event logic.
- **Granular synthesis.** Grains are events (class E) over a position-driven
  `Source`; the scheduler is inherent, the window and the read are existing
  primitives.
- **Tempo, beat, key and chord tracking, onset detection:** measurements (class M).
- **Voice allocation, MPE state:** `tutti-voicing`'s `Voicing::step`.
- **A neural inference core** (LSTM, GRU, 1-D convolution and dense layers, run
  from weights loaded as data): a stateful nonlinear system with no closed-form
  queries. Its model is `System` (latency 0, tail `Unbounded`) plus layer shapes and
  a `lipschitz` bound from weight norms; a class would claim answers it does not
  have. Its contract is the layer equations on hand-built tiny networks with known
  outputs (an LSTM with zero weights and a set bias has a closed-form state),
  never against another runtime. §9 D11 asks whether to build it.
- **Spring reverb and tape hysteresis:** memory inside the nonlinearity itself;
  not a `Lure` (the loop is linear plus a memoryless map) and not class 4. Out of
  scope until a consumer exists.

## 4. Spatial consolidation

`tutti-spatial` is the one effect family outside the pattern CLAUDE.md sets
("a pure algorithm over slices goes in `tutti-dsp`; a node file holds only the
`Node` adapter"): over a third of its non-test source (3,109 lines) is pure
algorithm. Moving it makes the HRTF and VBAP models able to live next to the
traits and the contract machinery (a crate-root test module on `main`; a feature
in doc 024 Q6's plan), which AllRAD and binaural ambisonic decode
need: AllRAD is `Series<ShDecode, VbapColumns>` (§3.5) and binaural decode is a
series with the HRTF pair. Encode, rotation and a plain decoder need neither
move. The option first taken here, algorithms into `tutti-dsp` and nodes staying in
`tutti-spatial`, was a recommendation, weighed against two alternatives (leave the crate as it
is, or dissolve it into `tutti-nodes` now, which moves every public path and has
no benefit until `DesignNode` exists); step 3 below then dissolved the crate into `tutti-nodes`; it is taken as a decision on the recommendation (§9 D13), reversible by the owner.

### 4.1 What moves, what stays

| Moves to `tutti-dsp/src/spatial/` (private module; types exported at the root) | Stays in `tutti-spatial` |
|---|---|
| The pure VBAP solve of `vbap/panner.rs`: direction (and spread, width) to gains, over a speaker layout | `AngleSmoother`, `ExponentialSmoother` and `SpatialTarget`: doc 024 row 46 and consolidation 2 (#451) keep the angle variant in spatial over `tutti_dsp::Smoother`, and this document does not reverse that |
| `hrtf/panner.rs`'s `HrtfBinaural` convolution core and `ring_out`; the public `HrtfBinauralError`, `MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`, `MAX_PREPARED_RATE` | `VbapPannerNode`, `HrtfBinauralNode` and their controls, `speaker_of_channel` |
| `hrtf/sphere.rs`: `HrirSphere::parse(&[u8])`, `HrtfSet::build`, `resample_into`/`resample_batched` (on `WindowedSinc`), `Face`, the barycentric lookup | `layout.rs`, `fork.rs`, `hold_finite`, `mix.rs` (graph assembly; its `tutti-nodes` edge keeps the crate at layer 5), `VbapError` |

**The smoothing seam.** `VbapPanner` and `HrtfBinaural` today own an
`AngleSmoother` as a field. Because the smoother stays, it moves up into the
nodes: the node smooths the angles and hands the algorithm **smoothed angles per
call**, which is what `tutti-dsp`'s rule asks ("every control arrives per call as a
plain typed value"). The block ramp between start and end gains stays beside the
smoother; where exactly the seam falls in `solve_block` is the first PR's
decision, and its proof is the unedited digests. This makes the moved portion
smaller than "the whole panner file".

**State.** `HrtfBinaural` holds `Arc<HrirSphere>` and `Arc<HrtfSet>` and swaps the
set on `set_sample_rate`. `tutti-dsp` is documented as having no `Arc`-published
state. The algorithm in `tutti-dsp` takes the built `HrtfSet` as plain immutable
data (as the convolver already takes its `IrSpectra`, which sits behind an `Arc`
as fork-shared immutable data, not as published state); the **node owns the `Arc`**
and the rebuild on a rate change, which is control-side. If doc 024 step 6 later
publishes the set through an `ArcLend`, the `Reclaimer` belongs to the node's
crate, not to `tutti-dsp`.

**Costs.** Everything that moves is `pub(crate)` today except
`HrtfBinauralError` and the three constants, which are defined in `tutti-dsp` and
re-exported unchanged from `tutti_spatial`. `tutti-spatial` stays at layer 5; the
layout script does not change; moved tests move unedited; doc 024 row 46,
consolidation 2 and the "*Spatial*: already a crate" sentence are updated in the
PRs. The `hrtf` feature gates about a thousand lines of `tutti-spatial` today;
after the move `tutti-dsp` (which has `default = []` and no features) compiles
them for every user, a build cost the PR measures. The nodes leave only when doc
024 step 6's `DesignNode` replaces them, and whether to dissolve the crate is
reconsidered then.

### 4.2 The `vbap` crate

`vbap = "0.1"` is the owner's own crate (`PoHsuanLai/vbap`, MIT, 1,473 lines:
`config.rs` 523, `panner.rs` 321, `math.rs` 179, `presets.rs` 188, `speaker.rs`
120, `error.rs` 43, `lib.rs` 99). Its solve is copied into `tutti-dsp` and
brought to tutti's shape, and the dependency is dropped:

| In the crate | What it is | In `tutti-dsp` |
|---|---|---|
| `SpeakerConfigBuilder`, `SpeakerConfig` | a builder of `(azimuth, elevation)` pairs that picks speaker **pairs** (2D: a 2x2 inverse each) or **triplets** (3D: candidates by `volume / side-sum`, pruning of crossing edges shortest first via `lines_intersect`, an interior-speaker rejection by `is_inside_triangle`, a 3x3 inverse) | a `SpeakerLayout` value built on the control side from the topology's speaker positions; the tuple table is a `Box<[Tuple]>` with `[u16; 3]` indices and a length, not a `Vec<usize>` per tuple; `build` returns a typed `SpeakerGeometryError` |
| `VBAPanner::compute_gains_into` | for each tuple, candidate gains `M^-1 d`; the tuple with the largest minimum gain wins; gains clamped at 0 and normalised to `sum g^2 = 1` | `SpeakerLayout::gains_into(&self, azimuth, elevation, spread, out: &mut [f32])`: the same selection and normalisation, no allocation, narrowing from `f64` once; the allocating `compute_gains` is not copied |
| `math`: `spherical_to_cartesian`, `cartesian_to_spherical`, `lines_intersect`, `point_on_arc` | trigonometry in `DVec3` with `libm::sincos`, `asin`, `atan2` | the same on a private `[f64; 3]` with `dot`, `cross`, `angle_between`, `normalize_or_zero`, a 3x3 inverse and a 2x2 inverse, about 80 lines; `libm` is already a `tutti-dsp` dependency |
| `presets` | stereo, stereo wide, LCR, quad, 5.1, 7.1, 7.1.4, 5.1.4, hexagon, octagon | only the five tutti uses (stereo, quad, 5.1, 7.1, 7.1.4) |
| `Speaker::with_distance` | a distance, unused by the solve | dropped |

**`glam` is not needed.** The crate uses it for `DVec2`/`DVec3`, `DMat2`/`DMat3`,
`cross`, `dot`, `angle_between`, `normalize_or_zero`, `determinant`, `inverse` and
`mat * vec`, which are a few dozen lines of `f64` arrays; `tutti-dsp` stays at
`tutti-types`, `rustfft`, `libm` and `thiserror`, and `glam` leaves `Cargo.lock`.

**Bit-exactness.** The golden digests (`smoother_digest.rs`, `vbap_fold_digest.rs`)
go through glam's inverse and vector operations, and CLAUDE.md allows a re-record
only for an argued libm move, so the replacement keeps glam's **operation order**
(read from glam 0.30's source): `DMat3::inverse` is `tmp0 = y x z`, `tmp1 = z x x`,
`tmp2 = x x y` (cross products, `a.y*b.z - b.y*a.z` and cyclic), `det = z . tmp2`,
columns `tmp_i * (1/det)`, then transposed; `mat * vec` is
`x_axis*v.x + y_axis*v.y + z_axis*v.z` left to right; `DMat2::inverse` is
`(y.y, -x.y, -y.x, x.x) * (1/det)`; `dot` is `(x*x) + (y*y) + (z*z)`. The one
operation that is a libm call inside glam is `angle_between`'s `acos` (glam's
`acos_approx` is `libm::acos` or `f64::acos` depending on glam's features; the PR
reads which one `Cargo.lock`'s build uses), which only
feeds **tuple selection** (volume per side length, distances and their ordering),
not the gains; for the symmetric presets equal distances tie, so a different
`acos` rounding could reorder a tie. The PR therefore pins the selected tuples of
the five presets against hand-derived expected sets (a spec figure) **before**
comparing digests. If a digest still moves, it is a re-record, allowed only for a
reason argued in the PR (§9 D14); the plan is for it not to.

**Unit types.** Controls arrive per call as `Azimuth`, `Elevation`, `Spread` and
`StereoWidth`; the solve converts degrees to radians once and works in `f64`
(the crate's own comment, "the types stop at the trigonometry", is the stated
reason a bare float is used in the kernel, per CLAUDE.md "Where the types stop").
A speaker's position is `(Azimuth, Elevation)`.

**Public surface.** `VbapError::Vbap(vbap::VBAPError)` is a variant on a
`#[non_exhaustive]` enum, wrapped by `tutti-session`'s `Error::Spatial`.
Dropping the dependency removes that variant and adds one that wraps
`SpeakerGeometryError`; the doc comments that name `VbapError::Vbap` in
`vbap/node.rs` and the session's wrapper change in the same PR.

**Provenance and the tests.** The crate says its pair and triplet selection is
"based on Ardour's `choose_speaker_pairs()`" and `choose_speaker_triplets()` in
`vbap_speakers.cc`, a file whose Debian copyright entry lists Ville Pulkki's 1998
VBAP library as LGPL-2.1-or-later (a second permissive notice in the Debian
testing file could not be read; unverified at the source file). The crate itself
is MIT. Copying the selection into a `MIT OR Apache-2.0` workspace carries that
provenance with it, and that was decided on the recommendation (§9 D15). The tests do not depend
on it: every VBAP property is a figure from Pulkki's published method (sum of
squares 1, non-negative gains, at most three non-zero, positive gains inside the
active triangle, continuity across a triangle edge, a source at a speaker puts
all energy there), so the selection can be re-derived from the paper and held to
those cases without comparing to the crate's output. The recommendation is to
re-derive it.

### 4.3 The PRs

1. **The VBAP solve.** (**Landed**, `dsp/spatial-vbap`: `tutti-dsp`'s `SpeakerLayout`,
   `SpeakerGeometryError`, `PanningMode`, `Tuple`, `MAX_SPEAKERS` (a cap to lift into a `Bounds` value, doc 024 §4.4, not a design limit); `VbapError::Geometry`
   replaces `Vbap`; `vbap` and `glam` are out of `Cargo.lock`. The selection is Pulkki's
   method, the convex-hull facets of the speaker directions with the shortest diagonal of a
   coplanar polygon (D15), not the crate's. Stereo, quad, 5.1 and 7.1 render bit for bit as
   before; **Atmos 7.1.4's tuples changed** (the crate's selection left the centre speaker
   in no triplet and listed each coplanar square twice over, overlapping; thirteen
   non-overlapping triangles now) and its digest moved, argued in the PR and pinned by
   `tutti-dsp`'s `spatial` tests and `tutti-spatial`'s `vbap_preset_digest.rs`.) The solve and the layout into `tutti-dsp::spatial`; the
   `vbap` dependency and `VbapError::Vbap` replaced; `glam` leaves the lock. Proof:
   the moved tests and digests unedited; the tuple sets of the five presets pinned;
   Pulkki's properties as new cases; `cargo tree -p tutti-dsp` shows no `glam`.
2. **HRTF set, convolver and sphere build.** (**Landed**, `dsp/spatial-hrtf`:
   `tutti-dsp`'s `HrtfBinaural`, `HrtfBinauralError`, `MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`,
   `MAX_PREPARED_RATE`, `HRTF_STEP`, `HRTF_LATENCY`; `AngleSmoother` stays in the node, which
   hands the convolver a closure that returns the aimed direction once per completed step, so
   the render is bit-identical and the digests are unedited.) `spatial/hrtf.rs`, `sphere.rs`;
   `HrtfBinauralError` and the constants re-exported unchanged. Proof:
   `hrtf_render.rs` and the contract suite unedited; the scalar-planner determinism
   note of `RealFft` applies.
2b. **The VBAP block kernel.** (**Landed**, `dsp/vbap-kernel`: `tutti-dsp`'s `VbapKernel`
   (a layout and the `last_solve` cache; `gains` is the mono branch below width 0.001 and
   the two virtual sources otherwise) and `VbapGains` (a ramp's end point, with
   `render_channel`, the per-channel ramp and its signed-zero held-gain shortcut). The
   node keeps the `Param` reads and guards, the smoother and `settled_at`, the speaker map
   and the `Io` loop; `BlockGains`, `frame_gains`, `solve_frame` and `render_channel` are
   gone from `tutti-spatial`, no shim left. Every digest is identical and `tests/vbap_*`
   pass unedited. The mono fold stays as it was: a gain-matrix form changes bits and is
   its own PR.)
3. **Cleanup.** (**Landed**, `dsp/nospatial`: the crate is removed, which is the
   dissolve-into-`tutti-nodes` alternative of §4's introduction, taken now: the panner
   nodes, `SpatialTarget`, the smoothers, the layout and fork helpers and `VbapError` are
   `tutti-nodes`' private `spatial` module, exported at its root (`tutti_nodes::VbapPannerNode`,
   `HrtfBinauralNode`, their controls), their tests, benches and the `assert_no_alloc` gate
   beside them, every digest identical; the `hrtf` feature, the self dev-dependency and
   `tutti-session`'s and `tutti`'s `spatial` and `hrtf` features are gone (the
   `SessionNode` impls and `Error::Spatial` are ungated); `tutti-nodes` gains one outside
   dependency, `thiserror`, for `VbapError`.) **3d, the mix builder, is deleted, not
   moved:** `build_vbap_mix`, `VbapMixParts` and their types had no production user, and
   with fan-in (doc 013) a mix is a panner per source wired to the output channels.
   The remaining spatial clean-ups, 3a to 3c and 3e (the node shape, the controls, the
   errors and names), now happen inside `tutti-nodes`. CLAUDE.md's `tutti-dsp` paragraph,
   doc 024 rows 44 to 46 and the "already a crate" sentence are updated.

Effort: M in all, as #448 and #451 were. After step 3 the HRIR file format
(§9 D16) is the one spatial question before ambisonic binaural decode.

## 5. The ranked roadmap

Table stakes first, then differentiators. "Class" is doc 024's; "Kind" says
whether the item is a new `Design`, a new realization of an existing model, or a
node only; "Waits on" names the doc 024, 022 or 026 step that hosts it or what it
needs; "Pinned to" names the cases (§3). Effort is S, M or L.

| # | Item | Class | Kind | Lives in | Waits on | Pinned to | Effort |
|---|---|---|---|---|---|---|---|
| 1 | Oversampler (polyphase FIR and half-band IIR, 2/4/8x, declared `latency`) | 6 | new models and realizations | `tutti-dsp/resample/` | kernels: none; model: doc 024 step 5 | stopband, passband, ratio, delay, overshoot (§3.2) | M |
| 2 | ADAA waveshapers, 1st then 2nd order | 4 over a class 3 map | new realization of the waveshaper's model | `tutti-dsp/waveshape.rs` | step 5 (kernels first) | derivative, identity, box average (§3.1) | S |
| 3 | True-peak limiter; true-peak meter as a tap read by `tutti-measure` | 4 (`SteadyState`, inherent detector) | new realization of the limiter; a reader | `tutti-dsp/dynamics/`, `tutti-measure` | item 1 kernels; doc 024 step 4 | margin, inter-sample peak, `Dg` (§3.3) | S to M |
| 4 | Utility pack and pan laws (including consolidating the three existing laws) | 1 order 0, 5 | nodes, small designs | `tutti-dsp`, `tutti-nodes` | step 7 for the pan models | matrices, sidebands, DC corner (§3.12, §3.5) | S |
| 5 | EQ family: slopes, tilt, multi-band node; dynamic EQ; linear-phase | 1 (`SeriesN`), `Controlled`, FIR | new designs, one combinator | `tutti-dsp/filter/`, `tutti-nodes` | steps 2 to 4; step 6 for the FIR model | Butterworth magnitude, settled response, phase (§3.4) | M each |
| 6 | De-esser, transient shaper | 4 | new designs | `tutti-dsp/dynamics/` | step 4 | `steady_state`, time constants | S |
| 7 | Noise-shaped dither | 5 with 1 | new combinator | `tutti-dsp/dither.rs`, `tutti-export` | step 7 (dither) | NTF psd, Gerzon-Craven (§3.6) | S |
| 8 | Stretch and pitch lock; warp markers | 6 | doc 026's | `tutti-dsp`, `tutti-sampler` | the owner's listening gate | doc 026 §7 | L |
| 9 | Formant-preserving pitch shift; then pitch correction | 6 | new realization | `tutti-dsp` | doc 026 stage 4 | formant shift within `fs/order` (§3.9) | M to L |
| 10 | Reverb: doc 022's items; colourlessness query, velvet decorrelator | 1 | 022's, plus queries | `tutti-dsp/network/` | doc 022 steps 3, 3b | doc 022 §8 | M |
| 11 | STFT framework; spectral gate, freeze, blur | per kernel | new structure, kernels | `tutti-dsp/stft.rs` | doc 026 stage 2 | window-sum constant, identity, block independence (§3.8) | M |
| 12 | Wavetable (mipmapped) and FM/PM | 5 (`BandLimited`) | new designs | `tutti-dsp`, `tutti-polysynth` | step 7 | non-harmonic bins, Bessel amplitudes (§3.7) | M each |
| 13 | Nonlinear virtual analog: nonlinear TPT ladder, diode clipper | `Lure` | new combinator, new realizations | `tutti-dsp/filter/` | items 1, 2; step 2 | small-signal response, critical gain, circle bound (§3.10) | M |
| 14 | Ambisonics: bus, encode, rotate, decode; AllRAD; binaural | 1 order 0 | new models | `tutti-dsp/spatial/`, `tutti-spatial` | doc 023 G11; §4 for AllRAD and binaural | degree energy, isometry, commutation (§3.5) | L |
| 15 | Neural amp and pedal inference | inherent | own core | a new crate, later | after items 1 to 13; §9 D11 | layer equations (§3.13) | L |

The ranking puts table stakes before differentiators, and folds two items in: the
pan-law item joins the utility pack, because a law is an order-0 matrix with a
small contract; and sampler interpolation tiers are doc 026's (026 Q5), not an
item here.

**Tracked, not scheduled:** MSEG and multi-stage looping envelopes
(`tutti-modulation`, a class 5 `Piecewise`), chords and key beyond triads,
downbeat and tempo-change tracking, click and crackle repair, and a `Separator`
seam for offline stem separation with no bundled model. Each is a
`tutti-measure` or modulation item with no model work.

## 6. The staged plan

Stages are PRs that pass `cargo xtask ci`. Kernels that need no new trait ship
early as plain `tutti-dsp` code with analytic tests and adopt the class traits in
the doc 024 step that hosts them, as the smoother did (consolidation 2): a stage
that needs a class trait step 1 did not add (`Memoryless`, `SteadyState`, ...)
waits for the doc 024 step that adds it.

| Stage | Content | Proves it | After |
|---|---|---|---|
| A1 | Spatial PR 1: the VBAP solve into `tutti-dsp::spatial`; `vbap` and `glam` dropped (§4) | unedited tests and digests; preset tuples pinned; Pulkki's properties | §9 D13 |
| A2 | Spatial PRs 2 (HRTF) and 3 (cleanup) | `hrtf_render.rs`, contract suite; paths stable | A1 |
| B1 | Utility pack; pan-law consolidation into `balance.rs` (the polysynth law and the crossfade onto it) (§3.12, §3.5) | matrices, sidebands, DC corner; power figures | none (the pan models become `Design`s with `Model = Fold` at step 7, after 7a) |
| B2 | Oversampling kernels and ADAA kernels as plain code and free functions (§3.1, §3.2) | derivative, identity, box average, stopband, overshoot, power complement | none |
| B3 | Doc 024 step 5 adopts them: waveshaper `Memoryless`, `Graded` oversampling, `Adaa` realizations, the oversampler models | the aliasing case, digests | B2, step 5 |
| B4 | True-peak limiter and the true-peak reader (§3.3) | margin, inter-sample peak, `Dg` | **B2 (kernels) and doc 024 step 4** (not B3) |
| C1 | EQ family: slopes and tilt on `SeriesN`; then `Controlled` and dynamic EQ; then the linear-phase FIR (§3.4) | Butterworth magnitude, settled response, phase | steps 2, 3, 4; the FIR's model with step 6 (a plain FIR-by-convolver earlier) |
| C2 | De-esser and transient shaper; noise-shaped dither (§3.6) | `steady_state`, NTF psd, Gerzon-Craven | steps 4 and 7 |
| D1 | Doc 026 stages 1 to 8 as written (the owner's listening gate first) | doc 026 §7 | the gate |
| D2 | The formant realization (§3.9); the live shifter if §9 D9 allows | formant shift within `fs/order` | doc 026 stage 4 |
| D3 | The STFT framework and first kernels (§3.8) | window-sum constant, identity, block independence | doc 026 stage 2 |
| E1 | Reverb: doc 022 steps 3 and 3b, then the two queries (§3.11) | doc 022 §8 | doc 022 steps 1c, 2; doc 024 step 3 |
| E2 | Wavetable and FM (§3.7) | non-harmonic bins, Bessel amplitudes | doc 024 step 7 |
| E3 | `Lure`, the nonlinear TPT ladder, a diode clipper (§3.10) | small-signal response, critical gain, circle bound | B3, C1; step 2 |
| F1a | Ambisonics: `ChannelTopology::ambisonic` (doc 023 G11, doc 024 step 7a); encode, rotation and a plain decoder as `Fold` models and nodes (§3.5), on doc 024 step 7a | degree energy, isometry, commutation | G11 only |
| F1b | AllRAD (`Series<ShDecode, VbapColumns>`) | AllRAD cases | F1a, A1 |
| F1c | Binaural ambisonic decode | series with the HRTF pair | F1a, A2 |
| F2 | Neural core (§9 D11), only if chosen | layer equations | after E3 |

**Alignment with doc 024 §10.** B3 is step 5; B4 and C2's dynamics half are step
4 (using B2's kernels, not step 5's adoption); C1 is steps 2 to 4, with the
linear-phase EQ's `Convolver` model at step 6; B1's pan models and E2 are step 7;
A2 moves the HRTF convolver (not the `Convolver`, which is already in
`tutti-dsp`), whose model is step 6; D1 is doc 026's own order, whose stage 1 is
doc 024 step 7's "stretch" item written first, as doc 026 §9 says.

## 7. Non-goals

- **Real-time neural stem separation and denoise in the graph.** An offline job at
  most. The CPU cost is high, the common high-quality checkpoints carry
  non-commercial licences (unverified beyond secondary sources), a model is a
  100 MB-class asset, and a pure-Rust runtime (candle, tract) is slow at that size.
  A `Separator` seam (a trait, offline, progress, no bundled model) is cheap and
  independent of any model; it is tracked in §5, not scheduled.
- **Embedding GPL or closed libraries.** Rubber Band is GPL-2-or-later with a paid
  commercial licence, élastique is closed, the IEM and SPARTA plug-in suites are
  GPLv3. The workspace is `MIT OR Apache-2.0` and the owner's rule is pure-Rust
  codecs and formats. The mathematics (real spherical harmonics, rotation, AllRAD,
  max-rE, ambiX conventions) is published and is implemented here from it.
- **Wave digital filters, for now.** They author the same circuits `Lure` models by
  another method, with a high cost per circuit and no query the Lur'e view does not
  give. Watch only.
- **Neural-vocoder stretching, polyphonic audio-to-MIDI and Dolby Atmos
  rendering** (a licensing matter, not DSP).

## 8. What the contract gains

No family in §3 needs a class group of its own. The cases reduce to
patterns doc 024 §6 already has, plus one:

| Pattern | Used by | Query it reads |
|---|---|---|
| Response against the impulse's FFT (`Lti`) | EQ, FIR, DC block, Hilbert, `Lure::small_signal`, the prototypes | `latency` against `onset` (P1) |
| Static curve and harmonics (`Memoryless`) | the shapers, the `phi` of a `Lure` | `ShapeKind::f1`, `f2` (inherent) |
| Steady state and time constants (`SteadyState`) | the ADAA model, the oversampled shaper, the limiter, `Controlled`'s detector | `Controlled::at` |
| Generator spectrum (`Periodic`, `Noise`) | wavetable, PM, shaped dither | `BandLimited::top_harmonic`; `Filtered::psd` |
| Time scale (`TimeScale`, `Fidelity`) | the prototypes, the shifter | inherent `overshoot_bound`, `formant_shift` |
| Honest tail counted from `latency` (`contract::assert_honest_tail`) | linear-phase FIR (the remaining `(L-1)/2` taps), the limiter, the STFT effect | `System::tail` against `latency` (P1) |
| **An identity between two models' answers** | rotation against encode, the STFT window-sum constant, mid/side round trip | `Fold::is_isometry` (relative to the metric), `column_power` |

The last pattern is new to doc 024: both sides are closed-form queries of models,
checked on the control side, so it is consistent with CLAUDE.md's rule against
comparing a render to a reference render.

## 9. Decisions and open questions

### 9.1 Decisions

Taken on the recommendation, 2026-11-11 (the owner's standing delegation of lane
decisions and approval to change sound for argued fixes); each is reversible by the
owner. D9 to D17 were open questions and are recorded here with their
recommendation as the decision.

| # | Decision | Ground |
|---|---|---|
| D1 | The limiter's detector is a measured oversampled peak divided by `cos(pi/2N)`; `overshoot_bound` is not used by it, and the windowed minimum stays inherent | §3.3; a worst-case bound would attenuate signals under the ceiling |
| D2 | A true-peak plan meter is a tap read by `tutti-measure`, not an op in `tutti-graph` | §3.3: `tutti-graph` cannot reach `tutti-dsp` |
| D3 | Shimmer is not a reverb `Design`; doc 022's exclusion stands. A shimmer after a live pitch shifter exists is a graph composition | §3.9, §3.11; it needs a live shifter that does not exist |
| D4 | Antiderivatives, the FIR overshoot bound and the formant shift are inherent methods; only `BandLimited` is a new trait (`GainMatrix` was struck 2026-11-11 for `Fold` with a basis) | doc 024 §4.2: a trait needs a second family; §2.2 names each implementer |
| D5 | The oversampler is two types (`PolyphaseFir`, `HalfbandIir`) with one `TimeScale` and `Fidelity`; the tier is the node's and export uses the live tier | §3.2; doc 024 Q8 stands |
| D6 | The STFT pair lives in `tutti-dsp/src/stft.rs`. Doc 026 stage 2 is not delayed: `LockedVocoder` is written on `StftGeometry` as 026 says, and the pair is factored out afterwards (D3 follows stage 2) with the vocoder moved onto it only if the two share code | §3.8; a separate crate would cut `tutti-dsp` along a boundary every processor crosses |
| D7 | The formant estimator is private to the realization in `tutti-dsp`; no trait | §3.9; doc 024 Q24's cut |
| D8 | `AngleSmoother` stays in `tutti-spatial`; the nodes own smoothing and the `Arc` of the HRTF set | §4.1; doc 024 row 46, consolidation 2 |
| D9 | A live pitch shifter's input-fed push shape amends doc 026 §12 when the shifter is built, and not before; the sampler realization (stage D2 of §6) does not need it | §3.9; doc 026 §12 rejected the shape for lack of a consumer |
| D10 | Doc 024 §4.2 amendments: P1 (`onset` added; `latency` stays the alignment delay, per the PDC owner's naming) and P2 (MIMO norms, `h2_norm`) land with their first consumers (P1 with the oversampler or linear-phase EQ, P2 with the dither work); P3 (`Lure<L, Phi>`) for the ladder and diode clipper only, at the PR of the first realization, and a general `Feedback<A, B>` stays declined | §2.1, §3.10; P3 lifts doc 024 Q17 for one loop shape whose stability argument is the circle criterion |
| D11 | Neural inference: none now; hosted plugins cover NAM-class models. If built later: a pure-Rust core (LSTM, GRU, 1-D convolution, dense) in a new crate above `tutti-dsp`, weights as control-side data, no allocation on the audio thread, no `candle`/`ort` there, with §3.13's contract. Revisit after items 1 to 13 | §3.13, §7 |
| D12 | Commit to ambisonics, first through third order in ACN/SN3D, with the ambisonic topology (G11, now `ChannelTopology::ambisonic`, doc 024 step 7a) first and AllRAD and binaural decode as the deliverable; not above order 3 until a consumer asks | §3.5 |
| D13 | The spatial consolidation is taken as §4 describes: algorithms into `tutti-dsp`, nodes and the smoother stay, in the three PRs of §4.3 | §4 |
| D14 | If a golden digest moves despite the bit-exact plan, a re-record is allowed only for a reason argued in the PR; the plan is to keep the digests bit-exact by pinning the preset tuples and glam's operation order first | §4.2 |
| D15 | The VBAP speaker-tuple selection is re-derived from Pulkki's published method and held to the spec cases of §4.2, so that no code derived from Ardour's `vbap_speakers.cc` (listed LGPL-2.1-or-later in Debian's copyright file, unverified at the source file) enters the workspace; the mathematics of the solve is kept as it is | §4.2 |
| D16 | The HRIR runtime format stays `hrir_sphere_builder`'s; SOFA (AES69-2022, netCDF/HDF5) is read by an offline conversion tool outside `xtask` (which takes no dependencies) and the engine; whether a maintained pure-Rust HDF5 reader exists is unverified, and a C library needs the owner first | §4, §11 |
| D17 | The polysynth's pan law (`sqrt((1-p)/2)`) and the cos/sin mapping stay as two named variants of the consolidated law, so the polysynth render does not change; unifying them is an argued render change, not part of this work | §3.5 |

### 9.2 Open questions

None open.

## 10. What was type-checked

The sketches of §3 were checked in a throwaway crate kept outside the repository,
with `cargo check` and `cargo test` clean. Doc 024's class traits and the vocabulary
types are **stand-ins** (the `model` module is not on `main`), `TimeScale` is
restated as doc 026 §3.2 defines it (including `reach`), and the proposed `onset`
and `h2_norm` are added to the stand-in traits. Every trait kept in this document
has an implementer there (`GainMatrix`, since struck for `Fold` with a basis: a pan law, mid/side, `ShEncode`, a yaw
rotation; `BandLimited`: a wavetable and a PM pair), and the combinators
(`Filtered`, `Controlled`, `Lure`, `Oversampled`) are written and exercised by a
test each (`Controlled` selecting a design by detector level (the `Settled` case), `Oversampled`'s peak
gain with the down stage, `Lure`'s injection-aware bound and well-posedness). The tests assert the figures this document states: the antiderivatives (including
the shipped atan curve)
by central difference and the identity-map quotient; the true-peak margin for a
tone at `fs/2` (grid peak `A cos(pi/2N)`, 0.688 dB at 4x, 0.169 dB at 8x); pan-law
power and the isometry of mid/side and a rotation; rotation commuting with encode
at first order, and the inverse-rotation mutant failing; first-order degree
energy; the ADAA small-step error against the naive map's; `Filtered::variance` against `l1^2`; the Gerzon-Craven integral for two
NTFs; Bessel amplitudes; the STFT window-sum constants (1.5 and 2); the DC-blocker
corner and Nyquist gain; the circle bound (2.885) and the critical gain (4) of four
one-poles. The TPDF `lsb^2/4` check is arithmetic on the two variances, not a
simulation. What was **not** checked: the limiter's output bound end to end (stated,
proved in two lines, a first-PR obligation), the aliasing floors, the oversampler's
stopband numbers, higher-order harmonics, and anything that needs the real crates.
No figure here is a measurement of tutti.

## 11. References

Verified against a source in this pass (abstract or record level unless stated):

- J. D. Parker, V. Zavalishin, E. Le Bivic, "Reducing the aliasing of nonlinear
  waveshaping using continuous-time convolution", DAFx-16.
  <https://dafx.de/paper-archive/details/vem_XXF5qBbfiWOH2RVVAA>
- M. Holters, "Antiderivative antialiasing for stateful systems", DAFx-19;
  *Applied Sciences* 10(1):20, 2020.
  <https://dafx.de/paper-archive/details/lp5GcJOo79OBkfcMfN3N_g>
- A. Röbel, X. Rodet, "Efficient spectral envelope estimation and its application
  to pitch shifting and envelope preservation", DAFx-05.
  <https://dafx.de/paper-archive/details/EDSTSaO17Gd_0LV03iwDfw>
- J. Laroche, M. Dolson, "Improved phase vocoder time-scale modification of audio",
  *IEEE Trans. Speech Audio Process.* 7(3), 1999.
  <https://www.ee.columbia.edu/~dpwe/papers/LaroD99-pvoc.pdf>
- F. Zotter, M. Frank, "All-round ambisonic panning and decoding", *JAES* 60(10),
  807 to 820, 2012. <https://aes2.org/publications/elibrary-page/?id=16554>
- P. Majdak et al., "Spatially oriented format for acoustics 2.1", *JAES* 70(7/8),
  2022; AES69-2022 is SOFA 2.1. <https://www.aes.org/e-lib/browse.cfm?elib=21824>
- S. P. Lipshitz, R. A. Wannamaker, J. Vanderkooy, "Quantization and dither: a
  theoretical survey", *JAES* 40(5), 355 to 375, 1992.
  <https://www.aes.org/e-lib/browse.cfm?elib=7047>
- J. Chowdhury, "RTNeural: fast neural inferencing for real-time systems",
  arXiv:2106.03037 (one author). <https://arxiv.org/abs/2106.03037>
- Rubber Band Library: GPL-2-or-later, with a separate commercial licence.
  <https://breakfastquay.com/rubberband/license.html>
- IEM Plug-in Suite: GPLv3 (package metadata).
  <https://www.archlinux.org/packages/extra/iempluginsuite>
- Debian copyright file for Ardour: `vbap_speakers.cc`, Pulkki 1998, listed
  LGPL-2.1-or-later; the scope of a second permissive notice was not readable.
  <https://metadata.ftp-master.debian.org/changelogs/main/a/ardour/stable_copyright>

**Unverified in this pass** (cited from memory or secondary sources, and not
relied on beyond motivation):

- The Bernstein-type inequality behind the `cos(pi/2N)` margin (§3.3), as used in
  the peak-to-average literature for oversampled signals; its statement for the
  limiter's setting is a first-PR obligation to cite and prove. The figures 0.688
  dB and 0.169 dB are computed from it.
- ITU-R BS.1770-5 Annex 2 (true peak, 4x interpolation, a 48-tap filter per
  secondary sources); the standard's text was not read.
- S. Bilbao, F. Esqueda, J. D. Parker, V. Välimäki, "Antiderivative antialiasing for
  memoryless nonlinearities", *IEEE Signal Process. Lett.* (year not confirmed).
- V. Pulkki, "Virtual sound source positioning using vector base amplitude
  panning", *JAES* 45(6), 456 to 466, 1997 (read only as cited by the `vbap`
  crate's own documentation).
- J. Dattorro, *JAES* 45(9), 1997; J.-M. Jot, A. Chaigne, 1991; V. Välimäki et al.
  on velvet noise (2012); S. Schlecht, E. Habets on FDN feedback matrices;
  V. Zavalishin, *The Art of VA Filter Design*; A. Simper's SVF notes; D. Yeh's
  circuit-modelling work; M. Nachbar et al., "ambiX" (2011); S. D'Angelo,
  V. Välimäki, ICASSP 2013 (record only); the circle criterion (Zames; Khalil,
  *Nonlinear Systems*) and Gerzon-Craven noise shaping, stated here for the monic
  minimum-phase FIR case.
- Whether a maintained pure-Rust SOFA/HDF5 reader exists; the licence of stem
  model weights.
